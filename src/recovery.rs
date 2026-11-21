//! Client-owned recovery artifacts for a delegated review: what was asked,
//! which processes ran, what state the review is in, and the result.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use crate::diag;
use crate::error::{os_error, Error, Result};
use crate::json::{self, Object, Value};
use crate::os;
use crate::pystr::PyStr;
use crate::result::ReviewResult;

pub struct Recovery {
    pub directory: Vec<u8>,
    review_id: PyStr,
    process: Object,
}

fn io_error(error: std::io::Error, path: &[u8]) -> Error {
    Error::Other(os_error(&error, Some(path)))
}

fn join(directory: &[u8], name: &str) -> Vec<u8> {
    let mut out = directory.to_vec();
    out.push(b'/');
    out.extend_from_slice(name.as_bytes());
    out
}

/// `PurePath.is_relative_to`: `path` is `root` or lies below it.
fn is_relative_to(path: &[u8], root: &[u8]) -> bool {
    let parts = |p: &[u8]| -> Vec<Vec<u8>> {
        let mut out = vec![if p.starts_with(b"/") { b"/".to_vec() } else { Vec::new() }];
        out.extend(p.split(|&b| b == b'/').filter(|s| !s.is_empty() && *s != b".").map(<[u8]>::to_vec));
        out
    };
    let path = parts(path);
    let root = parts(root);
    path.len() >= root.len() && path[..root.len()] == root[..]
}

impl Recovery {
    pub fn create(directory: &PyStr, roots: &[Vec<u8>], review_id: &PyStr, request: &Value) -> Result<Self> {
        let raw = directory
            .encode_surrogateescape()
            .ok_or_else(|| Error::Other(format!("the recovery directory cannot be encoded: {}", directory.display())))?;
        let directory = os::realpath(&raw);
        if roots.iter().any(|root| is_relative_to(&directory, root)) {
            return Err(Error::Usage("the recovery directory must be outside the reviewed inputs".into()));
        }
        create_private_directory(&directory)?;
        let metadata = fs::metadata(os::os_path(&directory)).map_err(|e| io_error(e, &directory))?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::Usage("the recovery directory must be private (mode 0700)".into()));
        }
        let mut entries = fs::read_dir(os::os_path(&directory)).map_err(|e| io_error(e, &directory))?;
        if entries.next().is_some() {
            return Err(Error::Usage(
                "the recovery directory is not empty; recover the existing run before relaunching".into(),
            ));
        }
        // SAFETY: getppid has no preconditions.
        let parent = unsafe { libc::getppid() };
        let process = Object::new()
            .with("client_pid", std::process::id())
            .with("parent_pid", parent as i64)
            .with("review_id", review_id)
            .with("started_at", os::utc_timestamp());
        let recovery = Recovery { directory, review_id: review_id.clone(), process };
        recovery.write_json("process.json", &Value::Object(recovery.process.clone()), true)?;
        recovery.write_json("request.json", request, false)?;
        recovery.state("starting", Object::new())?;
        Ok(recovery)
    }

    fn write_json(&self, name: &str, payload: &Value, exclusive: bool) -> Result<()> {
        let destination = join(&self.directory, name);
        let temporary = if exclusive {
            destination.clone()
        } else {
            join(&self.directory, &format!(".{name}.{}", os::uuid4_hex()))
        };
        let outcome = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(os::os_path(&temporary))
                .map_err(|e| io_error(e, &temporary))?;
            let mut text = json::dumps(payload, json::SORTED);
            text.push('\n');
            file.write_all(text.as_bytes()).map_err(|e| io_error(e, &temporary))?;
            drop(file);
            if !exclusive {
                fs::rename(os::os_path(&temporary), os::os_path(&destination)).map_err(|e| io_error(e, &temporary))?;
            }
            Ok(())
        })();
        if !exclusive {
            let _ = fs::remove_file(os::os_path(&temporary));
        }
        outcome
    }

    pub fn state(&self, state: &str, fields: Object) -> Result<()> {
        let mut payload = Object::new()
            .with("review_id", &self.review_id)
            .with("state", state)
            .with("updated_at", os::utc_timestamp());
        for (key, value) in fields.iter() {
            payload.insert(key.clone(), value.clone());
        }
        self.write_json("state.json", &Value::Object(payload), false)
    }

    pub fn reviewer_started(&mut self, pid: u32, argv: &[String]) -> Result<()> {
        self.process.insert("reviewer_pid", Value::from(pid));
        self.process
            .insert("reviewer_argv", Value::Array(argv.iter().map(|a| Value::from(a.as_str())).collect()));
        self.write_json("process.json", &Value::Object(self.process.clone()), false)
    }

    pub fn url_file(&self) -> Vec<u8> {
        join(&self.directory, "browser-url.txt")
    }

    pub fn finish(&self, result: &ReviewResult, exit_code: i32) -> Result<()> {
        self.write_json("result.json", &result.to_value(), false)?;
        self.state(
            "finished",
            Object::new().with("decision", result.decision.as_str()).with("exit_code", exit_code),
        )
    }

    /// Starts mirroring diagnostics into `stderr.log`.
    pub fn open_log(&self) -> Result<()> {
        let path = join(&self.directory, "stderr.log");
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(os::os_path(&path))
            .map_err(|e| io_error(e, &path))?;
        diag::set_log(Some(file));
        Ok(())
    }

    /// Records a failure, then stops mirroring diagnostics.
    pub fn close(&self, error: Option<&Error>) {
        if let Some(error) = error {
            let message = match error.message() {
                m if m.is_empty() => "review interrupted".to_string(),
                m => m,
            };
            diag::write(&format!("task_review: {message}\n"));
            let _ = self.state(
                "error",
                Object::new().with("exit_code", crate::EXIT_ERROR).with("error", message.as_str()),
            );
        }
        diag::set_log(None);
    }
}

/// `Path.mkdir(mode=0o700, parents=True, exist_ok=True)`.
fn create_private_directory(path: &[u8]) -> Result<()> {
    match fs::metadata(os::os_path(path)) {
        Ok(m) if m.is_dir() => return Ok(()),
        Ok(_) => {
            let error = std::io::Error::from_raw_os_error(libc::EEXIST);
            return Err(io_error(error, path));
        }
        Err(_) => {}
    }
    let parent = os::dirname(path);
    if !parent.is_empty() && parent != path && fs::metadata(os::os_path(&parent)).is_err() {
        fs::create_dir_all(os::os_path(&parent)).map_err(|e| io_error(e, &parent))?;
    }
    let mut builder = fs::DirBuilder::new();
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(os::os_path(path)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && os::os_path(path).is_dir() => Ok(()),
        Err(e) => Err(io_error(e, path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_compare_by_component() {
        assert!(is_relative_to(b"/a/b", b"/a"));
        assert!(is_relative_to(b"/a", b"/a"));
        assert!(!is_relative_to(b"/ab", b"/a"));
        assert!(is_relative_to(b"/x", b"/"));
    }
}
