//! Running the reviewer process: one JSON request argument in, one JSON
//! result line out, bounded by a timeout and by the user's interrupt.

use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::diag;
use crate::error::{os_error, Error, Result};
use crate::os;
use crate::pystr::{py_strip, PyStr};

pub const REVIEWER_ENV: &str = "TASK_REVIEW_REVIEWER";
pub const LAUNCHER_ENV: &str = "RDG_CONTROL_COMMAND";
pub const TIMEOUT_ENV: &str = "TASK_REVIEW_TIMEOUT";
pub const HTML_REVIEWER: &str = "html";
pub const RDG_REVIEWER: &str = "rdg";
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 1800;
pub const URL_FILE_ENV: &str = "HTML_REVIEW_URL_FILE";

/// Reviewer precedence: TASK_REVIEW_REVIEWER, RDG_CONTROL_COMMAND, then the
/// built-in HTML reviewer.
pub fn resolve_reviewer() -> String {
    for name in [REVIEWER_ENV, LAUNCHER_ENV] {
        if let Some(value) = std::env::var_os(name) {
            let value = PyStr::from_os(&value).strip();
            if !value.is_empty() {
                return value.display();
            }
        }
    }
    HTML_REVIEWER.into()
}

/// Python's `int()` on ASCII input: an optional sign, then digits that may be
/// grouped by single underscores.
fn python_int(text: &str) -> Option<i128> {
    let (negative, body) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let grouped = !body.is_empty()
        && body.split('_').all(|group| !group.is_empty() && group.bytes().all(|b| b.is_ascii_digit()));
    if !grouped {
        return None;
    }
    let value: i128 = body.replace('_', "").parse().unwrap_or(i128::MAX);
    Some(if negative { -value } else { value })
}

pub fn resolve_timeout() -> Result<Option<u64>> {
    let raw = std::env::var(TIMEOUT_ENV).unwrap_or_else(|_| DEFAULT_TIMEOUT_SECONDS.to_string());
    let trimmed = py_strip(&raw);
    let parsed = python_int(trimmed);
    let Some(seconds) = parsed else {
        return Err(Error::Usage(format!(
            "{TIMEOUT_ENV} must be a whole number of seconds: {}",
            crate::pystr::py_repr(&PyStr::from(trimmed))
        )));
    };
    if seconds < 0 {
        return Err(Error::Usage(format!(
            "{TIMEOUT_ENV} must not be negative: {}",
            crate::pystr::py_repr(&PyStr::from(trimmed))
        )));
    }
    Ok(if seconds == 0 { None } else { Some(seconds.min(u64::MAX as i128) as u64) })
}

/// Turns a reviewer name or launcher path into argv.
pub fn resolve_launcher_command(launcher: &str) -> Result<Vec<String>> {
    if launcher == HTML_REVIEWER {
        let exe = std::env::current_exe().map_err(|e| Error::Launcher(format!("unable to locate senpai: {e}")))?;
        return Ok(vec![exe.to_string_lossy().into_owned(), "html-review".into()]);
    }
    if launcher == RDG_REVIEWER {
        return Ok(vec!["rdg".into()]);
    }
    if std::fs::metadata(launcher).map(|m| m.is_file()).unwrap_or(false) {
        return Ok(vec![launcher.into()]);
    }
    let parts = shlex_split(launcher).map_err(|m| Error::Other(m.into()))?;
    if parts.is_empty() {
        return Err(Error::Usage(format!("{REVIEWER_ENV} is empty")));
    }
    Ok(parts)
}

/// `shlex.split` in POSIX mode without comments.
pub fn shlex_split(text: &str) -> std::result::Result<Vec<String>, &'static str> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                if in_word {
                    out.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(next) => current.push(next),
                    None => return Err("No escaped character"),
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => current.push(ch),
                        None => return Err("No closing quotation"),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.peek() {
                            Some(&n) if n == '"' || n == '\\' => {
                                current.push(n);
                                chars.next();
                            }
                            Some(_) => current.push('\\'),
                            None => return Err("No closing quotation"),
                        },
                        Some(ch) => current.push(ch),
                        None => return Err("No closing quotation"),
                    }
                }
            }
            _ => {
                in_word = true;
                current.push(c);
            }
        }
    }
    if in_word {
        out.push(current);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// interruption
// ---------------------------------------------------------------------------

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Installs SIGINT/SIGTERM handlers that only set a flag, restoring the
/// previous handlers when dropped.
pub struct SignalGuard {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl SignalGuard {
    pub fn install(signals: &[libc::c_int]) -> Self {
        let mut previous = Vec::new();
        for &signal in signals {
            // SAFETY: the handler only stores to an atomic, which is
            // async-signal-safe; the old action is saved and restored.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as *const () as usize;
                libc::sigemptyset(&mut action.sa_mask);
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(signal, &action, &mut old) == 0 {
                    previous.push((signal, old));
                }
            }
        }
        SignalGuard { previous }
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (signal, old) in &self.previous {
            // SAFETY: restores an action previously returned by sigaction.
            unsafe {
                libc::sigaction(*signal, old, std::ptr::null_mut());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// running
// ---------------------------------------------------------------------------

pub struct Launch<'a> {
    pub launcher: &'a str,
    pub flag: &'a str,
    pub request_json: &'a str,
    pub timeout: Option<u64>,
    pub timed_out: &'a str,
    pub url_file: Option<Vec<u8>>,
}

/// Runs the reviewer in its own session, relays its stderr live, and kills
/// its whole process group when it ends, so nothing it started outlives it.
pub fn run_reviewer(launch: &Launch, mut started: impl FnMut(u32, &[String]) -> Result<()>) -> Result<Vec<u8>> {
    let argv = resolve_launcher_command(launch.launcher)?;
    let mut command_line = argv.clone();
    command_line.push(launch.flag.into());
    command_line.push(launch.request_json.into());
    let mut command = Command::new(&command_line[0]);
    command
        .args(&command_line[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = &launch.url_file {
        command.env(URL_FILE_ENV, os::os_path(path));
    }
    // SAFETY: setsid is async-signal-safe and touches no Rust state.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let _guard = SignalGuard::install(&[libc::SIGINT, libc::SIGTERM]);
    if interrupted() {
        return Err(Error::Launcher("the review was interrupted; no approval was returned".into()));
    }
    let mut child = command.spawn().map_err(|e| {
        Error::Launcher(format!("unable to run the reviewer: {}", os_error(&e, Some(command_line[0].as_bytes()))))
    })?;
    let pid = child.id();
    let kill_group = || {
        // SAFETY: signals the process group this function created; a group
        // that is already gone only returns ESRCH.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    };

    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let mut stderr = child.stderr.take().expect("piped stderr");
    let relay = thread::spawn(move || {
        let mut collected = Vec::new();
        let mut decoder = diag::Utf8Stream::default();
        let mut chunk = vec![0u8; 65536];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    collected.extend_from_slice(&chunk[..n]);
                    diag::write(&decoder.push(&chunk[..n]));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        diag::write(&decoder.finish());
        collected
    });

    let outcome: Result<()> = (|| {
        started(pid, &command_line)?;
        let deadline = launch.timeout.map(|s| Instant::now() + Duration::from_secs(s));
        loop {
            if child.try_wait().map_err(|e| Error::Launcher(e.to_string()))?.is_some() {
                return Ok(());
            }
            if interrupted() {
                return Err(Error::Launcher("the review was interrupted; no approval was returned".into()));
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(Error::Launcher(launch.timed_out.into()));
            }
            thread::sleep(Duration::from_millis(20));
        }
    })();
    kill_group();
    let status = child.wait().map_err(|e| Error::Launcher(e.to_string()))?;
    let output = reader.join().unwrap_or_default();
    let diagnostics = relay.join().unwrap_or_default();
    outcome?;
    let code = status.code().unwrap_or_else(|| -status.signal().unwrap_or(0));
    if code != 0 {
        let text = String::from_utf8_lossy(&diagnostics);
        let text = py_strip(&text);
        return Err(Error::Launcher(if text.is_empty() {
            format!("the reviewer exited with code {code}")
        } else {
            text.to_string()
        }));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shlex_matches_python_posix_mode() {
        assert_eq!(shlex_split(r#"a 'b c' "d \" e" f\ g"#).unwrap(), vec!["a", "b c", "d \" e", "f g"]);
        assert_eq!(shlex_split(r#""a\b""#).unwrap(), vec!["a\\b"]);
        assert_eq!(shlex_split("  ").unwrap(), Vec::<String>::new());
        assert!(shlex_split("'open").is_err());
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::python_int;

    #[test]
    fn follows_python_int() {
        for (text, expected) in [
            ("5", Some(5)), ("+5", Some(5)), ("-5", Some(-5)), ("1_000", Some(1000)), ("007", Some(7)),
            ("+_5", None), ("_5", None), ("5_", None), ("1__0", None), ("+-5", None), ("", None), ("5s", None),
        ] {
            assert_eq!(python_int(text), expected, "{text:?}");
        }
    }
}
