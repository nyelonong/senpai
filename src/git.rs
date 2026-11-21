use std::ffi::OsString;
use std::process::{Command, Stdio};

use crate::error::{Error, Result};
use crate::pystr::PyStr;

/// Runs git in `root` and returns raw stdout. Output is captured so git
/// chatter never reaches this client's stdout.
pub trait Git {
    fn run(&self, root: &[u8], args: &[OsString], allowed: &[i32]) -> Result<Vec<u8>>;
}

pub struct SystemGit;

impl Git for SystemGit {
    fn run(&self, root: &[u8], args: &[OsString], allowed: &[i32]) -> Result<Vec<u8>> {
        let root_display = PyStr::decode_surrogateescape(root).display();
        let output = Command::new("git")
            .arg("-C")
            .arg(crate::os::os_path(root))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::Git(format!("unable to run git in {root_display}: {}", crate::error::os_error(&e, Some(b"git")))))?;
        let code = output.status.code().unwrap_or(-1);
        if !allowed.contains(&code) {
            let detail = String::from_utf8_lossy(&output.stderr).trim_matches(crate::pystr::py_isspace_char).to_string();
            let first = args.first().map(|a| a.to_string_lossy().into_owned()).unwrap_or_default();
            let detail = if detail.is_empty() { format!("exit code {code}") } else { detail };
            return Err(Error::Git(format!("git {first} failed in {root_display}: {detail}")));
        }
        Ok(output.stdout)
    }
}

pub fn args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

pub fn run(git: &dyn Git, root: &[u8], items: &[&str]) -> Result<Vec<u8>> {
    git.run(root, &args(items), &[0])
}
