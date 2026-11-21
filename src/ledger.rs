//! The approval ledger: one JSON object per line in the repository's git
//! directory, shared with approvals the Python client recorded.

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use sha2::{Digest, Sha256};

use crate::error::{os_error, Error, Result};
use crate::git::{self, Git};
use crate::json::{self, Object, Value};
use crate::os;
use crate::pystr::{py_splitlines, py_strip, PyStr};
use crate::request::{value_repr, Repository};
use crate::snapshot::{hex, sha256_hex};

pub const LEDGER_NAME: &str = "task-review-approval.jsonl";
/// The producer every ledger entry names. It stays the Python client's name so
/// approvals recorded by either implementation verify under the other.
pub const PRODUCER: &str = "task_review.py";
pub const APPROVING_DECISION: &str = "approve";
pub const DECISIONS: [&str; 4] = ["approve", "request-changes", "return-to-planning", "cancelled"];

pub fn decision_state(decision: &str) -> &'static str {
    match decision {
        "approve" => "approved",
        "cancelled" => "cancelled",
        _ => "rejected",
    }
}

fn decode_trimmed(output: &[u8]) -> String {
    py_strip(&String::from_utf8_lossy(output)).to_string()
}

pub fn resolve_common_git_dir(git: &dyn Git, root: &[u8]) -> Result<Vec<u8>> {
    let output = git::run(git, root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let directory = decode_trimmed(&output);
    if directory.is_empty() {
        return Err(Error::Git(format!(
            "unable to resolve the git directory for {}",
            PyStr::decode_surrogateescape(root).display()
        )));
    }
    Ok(directory.into_bytes())
}

fn join(directory: &[u8], name: &str) -> Vec<u8> {
    let mut out = directory.to_vec();
    if !out.is_empty() && !out.ends_with(b"/") {
        out.push(b'/');
    }
    out.extend_from_slice(name.as_bytes());
    out
}

pub fn ledger_path(git: &dyn Git, root: &[u8]) -> Result<Vec<u8>> {
    Ok(join(&resolve_common_git_dir(git, root)?, LEDGER_NAME))
}

pub fn is_git_worktree(git: &dyn Git, directory: &[u8]) -> bool {
    match git::run(git, directory, &["rev-parse", "--is-inside-work-tree"]) {
        Ok(output) => decode_trimmed(&output) == "true",
        Err(_) => false,
    }
}

/// Confirms the root is a Git worktree, in the launcher's own wording.
pub fn check_worktree(git: &dyn Git, root: &PyStr) -> Result<()> {
    let bytes = crate::request::encode_path(root)?;
    let fail = || Error::Result(format!("repository is not open or is not a Git worktree: {}", root.display()));
    let output = git::run(git, &bytes, &["rev-parse", "--is-inside-work-tree"]).map_err(|_| fail())?;
    if decode_trimmed(&output) != "true" {
        return Err(fail());
    }
    Ok(())
}

/// A document's ledger lives in the git directory of the repository that
/// contains it, or beside the document when it is in no repository.
pub fn document_ledger_path(git: &dyn Git, path: &[u8]) -> Result<Vec<u8>> {
    let mut directory = os::dirname(&os::abspath(path));
    if directory.is_empty() {
        directory = b".".to_vec();
    }
    if !is_git_worktree(git, &directory) {
        return Ok(join(&directory, LEDGER_NAME));
    }
    ledger_path(git, &directory)
}

/// Binds an approval to the exact bytes of the document and the criterion.
pub fn document_fingerprint(path: &[u8], criterion: &PyStr) -> Result<String> {
    let body = fs::read(os::os_path(path))
        .map_err(|e| Error::Document(format!("the document could not be read: {}", os_error(&e, Some(path)))))?;
    let absolute = PyStr::decode_surrogateescape(&os::abspath(path));
    let strict = |value: &PyStr| {
        value
            .encode_strict()
            .map(<[u8]>::to_vec)
            .ok_or_else(|| Error::Other(format!("'utf-8' codec can't encode {}", value.display())))
    };
    let mut digest = Sha256::new();
    digest.update(b"document\0");
    digest.update(strict(&absolute)?);
    digest.update(b"\0");
    digest.update(sha256_hex(&body).as_bytes());
    digest.update(b"\0");
    digest.update(strict(criterion)?);
    Ok(hex(&digest.finalize()))
}

pub struct Entry {
    pub state: String,
    pub decision: Value,
    pub fingerprint: Value,
    pub criterion: Value,
    pub repositories: Vec<Repository>,
    pub annotations: Value,
    pub error: Option<String>,
    pub document: Option<PyStr>,
}

impl Entry {
    pub fn new(state: &str, decision: Value, fingerprint: Value, criterion: Value, repositories: &[Repository]) -> Self {
        Entry {
            state: state.into(),
            decision,
            fingerprint,
            criterion,
            repositories: repositories.to_vec(),
            annotations: Value::Array(Vec::new()),
            error: None,
            document: None,
        }
    }

    pub fn to_value(&self) -> Value {
        let mut object = Object::new()
            .with("ts", os::utc_timestamp())
            .with("producer", PRODUCER)
            .with("state", self.state.as_str())
            .with("decision", self.decision.clone())
            .with("fingerprint", self.fingerprint.clone())
            .with("criterion", self.criterion.clone())
            .with("repositories", Value::Array(self.repositories.iter().map(Repository::payload).collect()))
            .with("annotations", self.annotations.clone());
        if let Some(document) = &self.document {
            object.insert("document", Value::from(document));
        }
        if let Some(error) = &self.error {
            object.insert("error", Value::from(error.as_str()));
        }
        Value::Object(object)
    }
}

pub fn append_entry(path: &[u8], entry: &Value) -> Result<()> {
    let mut payload = json::dumps(entry, json::SORTED);
    payload.push('\n');
    let display = PyStr::decode_surrogateescape(path).display();
    let fail = |e: std::io::Error| {
        Error::Other(format!("unable to append to the approval ledger {display}: {}", os_error(&e, Some(path))))
    };
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(os::os_path(path))
        .map_err(fail)?;
    file.write_all(payload.as_bytes()).map_err(fail)?;
    drop(file);
    fs::set_permissions(os::os_path(path), fs::Permissions::from_mode(0o600)).map_err(fail)?;
    Ok(())
}

pub fn append_ledger(paths: &[Vec<u8>], entry: &Entry) -> Result<()> {
    let value = entry.to_value();
    for path in paths {
        append_entry(path, &value)?;
    }
    Ok(())
}

/// Every well-formed ledger line; a malformed line is skipped.
pub fn read_entries(path: &[u8]) -> Result<Vec<Object>> {
    let raw = match fs::read(os::os_path(path)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::Other(format!(
                "unable to read the approval ledger {}: {}",
                PyStr::decode_surrogateescape(path).display(),
                os_error(&e, Some(path))
            )))
        }
    };
    let text = String::from_utf8_lossy(&raw);
    let mut entries = Vec::new();
    for line in py_splitlines(&text) {
        let stripped = py_strip(line);
        if stripped.is_empty() {
            continue;
        }
        if let Ok(Value::Object(object)) = json::loads_str(stripped) {
            entries.push(object);
        }
    }
    Ok(entries)
}

pub fn newest_entry(path: &[u8]) -> Result<Option<Object>> {
    Ok(read_entries(path)?.pop())
}

/// Whether a ledger entry approves exactly this state. The producer check
/// comes first: another tool's bookkeeping never validates this decision.
pub fn approval_state(entry: Option<&Object>, fingerprint: &str) -> (bool, String) {
    let Some(entry) = entry else {
        return (false, "no task review approval is recorded".into());
    };
    if !matches!(entry.get("producer"), Some(v) if v.is_str(PRODUCER)) {
        return (false, format!("the newest ledger entry was not written by {PRODUCER}"));
    }
    let state = entry.get("state");
    if !matches!(state, Some(v) if v.is_str("approved")) {
        return (false, format!("the newest ledger entry is {}", value_repr(state)));
    }
    let decision = entry.get("decision");
    if !matches!(decision, Some(v) if v.is_str(APPROVING_DECISION)) {
        return (false, format!("the newest ledger entry decided {}", value_repr(decision)));
    }
    if !matches!(entry.get("fingerprint"), Some(v) if v.is_str(fingerprint)) {
        return (false, "the working tree changed since the approval was recorded".into());
    }
    (true, "the approval matches the current state".into())
}

/// A field carried over from the previous entry, unless it is missing or empty.
pub fn carried(entry: Option<&Object>, key: &str, default: Value) -> Value {
    match entry.and_then(|e| e.get(key)) {
        None | Some(Value::Null) => default,
        Some(Value::Str(s)) if s.is_empty() => default,
        Some(value) => value.clone(),
    }
}

pub fn ledger_paths(git: &dyn Git, repositories: &[Repository]) -> Result<Vec<Vec<u8>>> {
    let mut paths = Vec::new();
    for repository in repositories {
        check_worktree(git, &repository.root)?;
        paths.push(ledger_path(git, &repository.root_bytes()?)?);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str) -> Object {
        json::loads_str(text).unwrap().as_object().unwrap().clone()
    }

    #[test]
    fn approval_requires_producer_state_decision_and_fingerprint() {
        let good = entry(r#"{"producer":"task_review.py","state":"approved","decision":"approve","fingerprint":"f"}"#);
        assert!(approval_state(Some(&good), "f").0);
        assert_eq!(approval_state(Some(&good), "g").1, "the working tree changed since the approval was recorded");
        let other = entry(r#"{"producer":"x","state":"approved"}"#);
        assert_eq!(approval_state(Some(&other), "f").1, "the newest ledger entry was not written by task_review.py");
        let missing = entry(r#"{"producer":"task_review.py"}"#);
        assert_eq!(approval_state(Some(&missing), "f").1, "the newest ledger entry is None");
        assert_eq!(approval_state(None, "f").1, "no task review approval is recorded");
    }

    #[test]
    fn carried_skips_missing_and_empty_values() {
        let previous = entry(r#"{"decision":"","fingerprint":"abc","criterion":null,"n":0}"#);
        assert_eq!(carried(Some(&previous), "decision", Value::str("d")), Value::str("d"));
        assert_eq!(carried(Some(&previous), "fingerprint", Value::str("d")), Value::str("abc"));
        assert_eq!(carried(Some(&previous), "criterion", Value::str("d")), Value::str("d"));
        assert_eq!(carried(Some(&previous), "n", Value::str("d")), Value::Int(0));
    }

    #[test]
    fn reading_skips_malformed_lines() {
        let dir = std::env::temp_dir().join(format!("senpai-ledger-{}", os::uuid4_hex()));
        fs::create_dir_all(&dir).unwrap();
        let path = os::path_bytes(&dir.join(LEDGER_NAME));
        fs::write(os::os_path(&path), b"{\"a\": 1}\r\nnot json\n[1]\n  {\"b\": 2}  \n\xff\n").unwrap();
        let entries = read_entries(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].get("b"), Some(&Value::Int(2)));
        fs::remove_dir_all(&dir).unwrap();
    }
}
