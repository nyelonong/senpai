//! What the black-box tests share: the binary under test and its contract
//! values, private directories and Git repositories, running commands, and
//! small HTTP and Unix-socket clients of their own.
//!
//! Every test gets its own directory and passes environment variables to the
//! processes it starts; no test changes its own process environment, so tests
//! can run in parallel. Each command starts from this process's environment
//! with the inbox, Rex, Tern and terminal-browser switched off, so no test
//! touches the person's real inbox or panes.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub use serde_json::{json, Value};

pub const EXIT_OK: i32 = 0;
pub const EXIT_NOT_APPROVED: i32 = 1;
pub const EXIT_ERROR: i32 = 2;

pub const PROTOCOL_VERSION: &str = "1";
pub const PRODUCER: &str = "task_review.py";
pub const REVIEWER_PRODUCER: &str = "html_review.py";
pub const LEDGER_NAME: &str = "task-review-approval.jsonl";
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
pub const MAX_GUIDE_BYTES: usize = 48 * 1024;

pub const REVIEWER_ENV: &str = "TASK_REVIEW_REVIEWER";
pub const LAUNCHER_ENV: &str = "RDG_CONTROL_COMMAND";
pub const TIMEOUT_ENV: &str = "TASK_REVIEW_TIMEOUT";
pub const NO_BROWSER_ENV: &str = "HTML_REVIEW_NO_BROWSER";
pub const URL_FILE_ENV: &str = "HTML_REVIEW_URL_FILE";
pub const LIFETIME_ENV: &str = "HTML_REVIEW_LIFETIME";
pub const IDLE_ENV: &str = "HTML_REVIEW_IDLE";
pub const INBOX_ENV: &str = "SENPAI_INBOX";
pub const HOME_ENV: &str = "SENPAI_HOME";
pub const SERVE_IDLE_ENV: &str = "SENPAI_SERVE_IDLE";
pub const SURFACE_ENV: &str = "SENPAI_SURFACE";
pub const TERMINAL_BROWSER_ENV: &str = "SENPAI_TERMINAL_BROWSER";
pub const REX_ENV: [&str; 2] = ["REX_SESSION", "REX_BLOCK"];
pub const TERN_ENV: &str = "TERN_PANE";

/// Git settings that keep the person's configuration out of every test.
pub const GIT_ENV: [(&str, &str); 7] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_AUTHOR_NAME", "Task Review Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_NAME", "Task Review Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
];

/// The binary under test: `SENPAI_BINARY`, else the one Cargo built.
pub fn binary() -> PathBuf {
    std::env::var_os("SENPAI_BINARY")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_senpai")))
}

/// The environment every started process gets on top of this one.
pub fn isolate(command: &mut Command) -> &mut Command {
    for name in REX_ENV.iter().chain([&TERN_ENV, &SURFACE_ENV, &HOME_ENV, &REVIEWER_ENV, &LAUNCHER_ENV]) {
        command.env_remove(name);
    }
    command.env(INBOX_ENV, "0").env(TERMINAL_BROWSER_ENV, "none");
    for (name, value) in GIT_ENV {
        command.env(name, value);
    }
    command
}

/// `senpai ARGS`, isolated.
pub fn senpai<I, S>(args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(binary());
    command.args(args);
    isolate(&mut command);
    command
}

/// `senpai html-review ARGS`, isolated.
pub fn reviewer<I, S>(args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = senpai(["html-review"]);
    command.args(args);
    command
}

/// A `TASK_REVIEW_REVIEWER` value naming the built-in reviewer.
pub fn reviewer_launcher() -> String {
    format!("{} html-review", shell_quote(&binary().to_string_lossy()))
}

pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A finished command.
#[derive(Debug)]
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}\nstderr:\n{}", self.stdout, self.stderr))
    }
}

impl From<Output> for Run {
    fn from(output: Output) -> Self {
        use std::os::unix::process::ExitStatusExt;
        let code = output.status.code().unwrap_or_else(|| -output.status.signal().unwrap_or(0));
        Run {
            code,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

/// Runs to completion with no input, bounded by `timeout` seconds.
pub fn run(command: &mut Command) -> Run {
    run_with(command, None, 120)
}

pub fn run_with(command: &mut Command, input: Option<&[u8]>, timeout: u64) -> Run {
    command.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("start the command");
    if let Some(input) = input {
        let mut stdin = child.stdin.take().expect("stdin");
        let data = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    finish(child, timeout)
}

/// Waits for a started child. One still running after `timeout` seconds is
/// killed and fails the test, so a hang never passes.
pub fn finish(mut child: Child, timeout: u64) -> Run {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the command was still running after {timeout} s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let collect = |h: Option<std::thread::JoinHandle<Vec<u8>>>| h.map(|h| h.join().unwrap_or_default()).unwrap_or_default();
    Run::from(Output { status, stdout: collect(stdout), stderr: collect(stderr) })
}

fn drain<R: Read + Send + 'static>(mut reader: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = reader.read_to_end(&mut buffer);
        buffer
    })
}

/// Starts a command with piped output, for tests that interact while it runs.
pub fn spawn(command: &mut Command) -> Child {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    command.spawn().expect("start the command")
}

/// A private directory, removed when dropped. Its path is canonical (macOS
/// `/var` is a link to `/private/var`) and short enough for a Unix socket.
pub struct TempDir {
    path: PathBuf,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    pub fn new() -> Self {
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let name = format!("sp{}-{n}", std::process::id());
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&path).expect("create a temp dir");
        let path = std::fs::canonicalize(&path).expect("canonical temp dir");
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(path, content).expect("write");
}

pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Runs git in `root` and returns its stdout; fails the test on error.
pub fn git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(args);
    for (name, value) in GIT_ENV {
        command.env(name, value);
    }
    let output = command.output().expect("run git");
    assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn commit(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(root, &["-c", "commit.gpgsign=false", "commit", "-q", "-m", message]);
}

/// A repository named `name` under `base` with `a.py` and `other/b.txt`
/// committed once.
pub fn make_repo(base: &Path, name: &str) -> PathBuf {
    let root = base.join(name);
    write(&root.join("a.py"), "print('one')\n");
    write(&root.join("other/b.txt"), "unchanged\n");
    git(&root, &["init", "-q", "-b", "main", "."]);
    commit(&root, "initial");
    root
}

/// The Git common directory of `root`, where the ledger lives.
pub fn git_common_dir(root: &Path) -> PathBuf {
    let text = git(root, &["rev-parse", "--git-common-dir"]);
    let path = PathBuf::from(text.trim());
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

/// Every well-formed ledger line, skipping malformed ones as the client does.
pub fn read_entries(path: &Path) -> Vec<Value> {
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(Value::is_object)
        .collect()
}

pub fn ledger(root: &Path) -> Vec<Value> {
    read_entries(&git_common_dir(root).join(LEDGER_NAME))
}

/// Polls `condition` until it returns `Some`, or fails after `seconds`.
pub fn wait_for<T>(seconds: f64, what: &str, mut condition: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    loop {
        if let Some(value) = condition() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The non-empty, newline-terminated contents of `path`, once written.
pub fn wait_for_line(path: &Path, seconds: f64) -> String {
    wait_for(seconds, &format!("{}", path.display()), || {
        std::fs::read_to_string(path).ok().filter(|t| t.ends_with('\n')).map(|t| t.trim().to_string())
    })
}

/// An executable script.
pub fn script(path: &Path, body: &str) -> PathBuf {
    write(path, body);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path.to_path_buf()
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", self.text()))
    }

    /// A header by name, ignoring case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

/// `host:port` and the request target of an `http://` URL.
pub fn split_url(url: &str) -> (String, String) {
    let rest = url.strip_prefix("http://").expect("an http:// URL");
    match rest.find('/') {
        Some(slash) => (rest[..slash].to_string(), rest[slash..].to_string()),
        None => (rest.to_string(), "/".to_string()),
    }
}

/// Sends raw request bytes to `address` and reads the whole response.
pub fn http_raw(address: &str, request: &[u8]) -> Response {
    let mut stream = TcpStream::connect(address).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
    stream.write_all(request).expect("send");
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Response {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("a complete response head");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines.next().and_then(|l| l.split(' ').nth(1)).and_then(|s| s.parse().ok()).expect("a status");
    let headers: BTreeMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    if headers.get("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        body = dechunk(&body);
    }
    Response { status, headers, body }
}

/// Decodes a `Transfer-Encoding: chunked` body, as an HTTP client does.
fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    while let Some(end) = data.windows(2).position(|w| w == b"\r\n") {
        let size_text = String::from_utf8_lossy(&data[..end]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        data = &data[end + 2..];
        if size == 0 || data.len() < size {
            break;
        }
        body.extend_from_slice(&data[..size]);
        data = data.get(size + 2..).unwrap_or(&[]);
    }
    body
}

pub fn request(method: &str, url: &str, body: Option<&[u8]>, headers: &[(&str, &str)]) -> Response {
    let (address, target) = split_url(url);
    let mut text = format!("{method} {target} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        text.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    text.push_str("\r\n");
    let mut bytes = text.into_bytes();
    if let Some(body) = body {
        bytes.extend_from_slice(body);
    }
    http_raw(&address, &bytes)
}

pub fn get(url: &str) -> Response {
    request("GET", url, None, &[])
}

/// POSTs `payload` as JSON, or raw bytes when given a byte string.
pub fn post(url: &str, payload: &Value) -> Response {
    let body = serde_json::to_vec(payload).expect("encode");
    request("POST", url, Some(&body), &[("Content-Type", "application/json")])
}

pub fn post_bytes(url: &str, body: &[u8]) -> Response {
    request("POST", url, Some(body), &[("Content-Type", "application/json")])
}

// ---------------------------------------------------------------------------
// the inbox socket
// ---------------------------------------------------------------------------

/// The service's build id for `binary`, as `senpai` computes it.
pub fn build_id(binary: &Path) -> String {
    let manifest = include_str!("../../Cargo.toml");
    let version = manifest
        .lines()
        .find_map(|l| l.strip_prefix("version = \"").and_then(|r| r.strip_suffix('"')))
        .expect("a package version");
    let modified = std::fs::metadata(binary).and_then(|m| m.modified()).expect("binary mtime");
    let stamp = modified.duration_since(std::time::UNIX_EPOCH).expect("after the epoch");
    format!("{version}+{}.{:09}", stamp.as_secs(), stamp.subsec_nanos())
}

/// One request on the inbox socket under `home`, with this binary's build id.
pub fn socket_call(home: &Path, mut message: Value) -> Value {
    message["build"] = Value::String(build_id(&binary()));
    let mut stream = UnixStream::connect(home.join("senpai.sock")).expect("connect to the inbox");
    stream.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
    let mut line = serde_json::to_vec(&message).expect("encode");
    line.push(b'\n');
    stream.write_all(&line).expect("send");
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).expect("reply");
    serde_json::from_str(&reply).unwrap_or_else(|e| panic!("reply is not JSON ({e}): {reply}"))
}
