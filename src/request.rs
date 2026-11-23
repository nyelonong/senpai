//! Review requests and their validation, matching the protocol v1 rules.

use crate::error::{Error, Result};
use crate::json::{self, Object, Value};
use crate::os;
use crate::pystr::{py_repr, PyStr};

pub const PROTOCOL_VERSION: &str = "1";
pub const MAX_GUIDE_BYTES: usize = 48 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Repository {
    pub root: PyStr,
    pub paths: Option<Vec<PyStr>>,
    pub base: Option<String>,
    pub head: Option<String>,
}

impl Repository {
    pub fn payload(&self) -> Value {
        let mut object = Object::new().with("root", &self.root);
        if let Some(paths) = &self.paths {
            object.insert("paths", Value::Array(paths.iter().map(Value::from).collect()));
        }
        if let Some(base) = &self.base {
            object.insert("base", Value::from(base.as_str()));
        }
        if let Some(head) = &self.head {
            object.insert("head", Value::from(head.as_str()));
        }
        Value::Object(object)
    }

    pub fn root_bytes(&self) -> Result<Vec<u8>> {
        encode_path(&self.root)
    }
}

pub fn encode_path(value: &PyStr) -> Result<Vec<u8>> {
    value
        .encode_surrogateescape()
        .ok_or_else(|| Error::Other(format!("the path cannot be encoded: {}", value.display())))
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReviewRequest {
    pub version: String,
    pub review_id: PyStr,
    pub criterion: PyStr,
    pub repositories: Vec<Repository>,
    pub guide: Option<Value>,
}

impl ReviewRequest {
    pub fn payload(&self) -> Value {
        let mut object = Object::new()
            .with("version", self.version.as_str())
            .with("review_id", &self.review_id)
            .with("criterion", &self.criterion)
            .with("repositories", Value::Array(self.repositories.iter().map(Repository::payload).collect()));
        if let Some(guide) = &self.guide {
            object.insert("guide", guide.clone());
        }
        Value::Object(object)
    }

    pub fn to_json(&self) -> String {
        json::dumps(&self.payload(), json::COMPACT)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DocumentRequest {
    pub version: String,
    pub review_id: PyStr,
    pub criterion: PyStr,
    pub path: PyStr,
}

impl DocumentRequest {
    pub fn payload(&self) -> Value {
        Value::Object(
            Object::new()
                .with("version", self.version.as_str())
                .with("review_id", &self.review_id)
                .with("criterion", &self.criterion)
                .with("path", &self.path)
                .with("kind", "document"),
        )
    }

    pub fn to_json(&self) -> String {
        json::dumps(&self.payload(), json::COMPACT)
    }

    pub fn path_bytes(&self) -> Result<Vec<u8>> {
        encode_path(&self.path)
    }
}

fn result_error(message: impl Into<String>) -> Error {
    Error::Result(message.into())
}

/// Validates a repository root and canonicalises it with realpath.
pub fn normalize_root(value: Option<&Value>) -> Result<PyStr> {
    let root = match value {
        Some(Value::Str(s)) if !s.is_empty() => s,
        _ => return Err(result_error("each repository needs a non-empty root path")),
    };
    let bytes = encode_path(root)?;
    if !os::isabs(&bytes) {
        return Err(result_error(format!("repository root must be absolute: {}", py_repr(root))));
    }
    if root.split_byte(b'/').iter().any(|part| part == "." || part == "..") {
        return Err(result_error(format!("repository root must not contain '.' or '..': {}", py_repr(root))));
    }
    Ok(PyStr::decode_surrogateescape(&os::realpath(&bytes)))
}

pub fn normalize_root_str(value: &PyStr) -> Result<PyStr> {
    normalize_root(Some(&Value::Str(value.clone())))
}

/// Validates an optional scope of normalised repository-relative paths.
pub fn normalize_paths(value: Option<&Value>) -> Result<Option<Vec<PyStr>>> {
    let items = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(result_error("repository paths must be a list of strings")),
    };
    if items.is_empty() {
        return Err(result_error("repository paths must not be empty when present"));
    }
    let mut normalized = Vec::new();
    for item in items {
        let text = match item {
            Value::Str(s) if !s.is_empty() => s,
            _ => return Err(result_error("each repository path must be a non-empty string")),
        };
        let candidate = PyStr::from_wtf8_unchecked(
            text.as_wtf8().iter().map(|&b| if b == b'\\' { b'/' } else { b }).collect(),
        );
        let bytes = candidate.as_wtf8();
        let drive = bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/';
        if candidate.starts_with("/") || drive {
            return Err(result_error(format!("repository path must be relative: {}", py_repr(text))));
        }
        if candidate.split_byte(b'/').iter().any(|part| part.is_empty() || part == "." || part == "..") {
            return Err(result_error(format!(
                "repository path must be normalised without '.', '..' or '//': {}",
                py_repr(text)
            )));
        }
        if os::normpath(candidate.as_wtf8()) != candidate.as_wtf8() {
            return Err(result_error(format!("repository path must be normalised: {}", py_repr(text))));
        }
        normalized.push(candidate);
    }
    Ok(Some(normalized))
}

/// Validates an optional Git revision. Only printable ASCII without spaces is
/// accepted, and a leading '-' is refused so the value can never be read as a
/// git option.
pub fn normalize_revision(value: Option<&Value>, name: &str) -> Result<Option<String>> {
    let text = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Str(s)) => s,
        Some(_) => return Err(result_error(format!("repository {name} must be a string"))),
    };
    let bytes = text.as_wtf8();
    let printable = bytes.iter().all(|b| (0x21..=0x7e).contains(b));
    if bytes.is_empty() || bytes.len() > 256 || !printable || bytes[0] == b'-' {
        return Err(result_error(format!(
            "repository {name} must be 1 to 256 printable ASCII characters without spaces, not starting with '-': {}",
            py_repr(text)
        )));
    }
    Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
}

pub fn normalize_repository(value: &Value) -> Result<Repository> {
    let Some(object) = value.as_object() else {
        return Err(result_error("each repository must be an object"));
    };
    if !object.contains_key("root") {
        return Err(result_error("each repository needs a root path"));
    }
    let mut unknown: Vec<&PyStr> =
        object.keys().filter(|k| !["root", "paths", "base", "head"].iter().any(|known| k == known)).collect();
    if !unknown.is_empty() {
        unknown.sort();
        let names: Vec<String> = unknown.iter().map(|k| k.display()).collect();
        return Err(result_error(format!("unsupported repository keys: {}", names.join(", "))));
    }
    let base = normalize_revision(object.get("base"), "base")?;
    let head = normalize_revision(object.get("head"), "head")?;
    if head.is_some() && base.is_none() {
        return Err(result_error("a repository head needs a base"));
    }
    Ok(Repository {
        root: normalize_root(object.get("root"))?,
        paths: normalize_paths(object.get("paths"))?,
        base,
        head,
    })
}

fn guide_text(item: &Value, name: &str, limit: usize) -> Result<Value> {
    match item {
        Value::Str(s) if !s.is_blank() && s.char_len() <= limit => Ok(item.clone()),
        _ => Err(result_error(format!("guide {name} must be non-empty text of at most {limit} characters"))),
    }
}

fn is_fingerprint(value: &Value) -> bool {
    matches!(value, Value::Str(s) if s.as_wtf8().len() == 64 && s.as_wtf8().iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

pub fn normalize_guide(value: &Value) -> Result<Value> {
    let Some(object) = value.as_object() else {
        return Err(result_error("guide must be an object"));
    };
    if json::dumps(value, json::DEFAULT).len() > MAX_GUIDE_BYTES {
        return Err(result_error("guide exceeds the 48 KiB limit"));
    }
    const REQUIRED: [&str; 6] = ["version", "fingerprint", "summary", "before", "after", "chapters"];
    let has_required = REQUIRED.iter().all(|k| object.contains_key(k));
    let has_extra = object.keys().any(|k| !REQUIRED.iter().any(|r| k == r) && k != "flow");
    if !has_required || has_extra {
        return Err(result_error(
            "guide needs version, fingerprint, summary, before, after and chapters; only flow is optional",
        ));
    }
    if object.get("version") != Some(&Value::Int(1)) {
        return Err(result_error("unsupported guide version"));
    }
    let fingerprint = object.get("fingerprint").expect("required");
    if !is_fingerprint(fingerprint) {
        return Err(result_error("guide needs a scoped diff fingerprint"));
    }
    let mut normalized = Object::new().with("version", 1i64).with("fingerprint", fingerprint.clone());
    for key in ["summary", "before", "after"] {
        normalized.insert(key, guide_text(object.get(key).expect("required"), key, 8000)?);
    }
    if let Some(flow) = object.get("flow") {
        let flow = match flow.as_object() {
            Some(f) if f.has_exact_keys(&["before", "after"]) => f,
            _ => return Err(result_error("guide flow needs before and after label lists")),
        };
        let mut out = Object::new();
        for side in ["before", "after"] {
            let labels = match flow.get(side).and_then(Value::as_array) {
                Some(l) if (1..=20).contains(&l.len()) => l,
                _ => return Err(result_error("guide flow needs 1–20 labels per side")),
            };
            let checked: Result<Vec<Value>> = labels.iter().map(|l| guide_text(l, "flow label", 200)).collect();
            out.insert(side, Value::Array(checked?));
        }
        normalized.insert("flow", Value::Object(out));
    }
    let chapters = match object.get("chapters").and_then(Value::as_array) {
        Some(c) if c.len() <= 128 => c,
        _ => return Err(result_error("guide chapters must be a list of at most 128 chapters")),
    };
    let mut seen: Vec<(PyStr, PyStr)> = Vec::new();
    let mut out_chapters = Vec::new();
    for chapter in chapters {
        let chapter = match chapter.as_object() {
            Some(c) if c.has_exact_keys(&["title", "summary", "files"]) => c,
            _ => return Err(result_error("each guide chapter needs title, summary and files")),
        };
        let files = match chapter.get("files").and_then(Value::as_array) {
            Some(f) if !f.is_empty() => f,
            _ => return Err(result_error("each guide chapter needs at least one file")),
        };
        let mut references = Vec::new();
        for reference in files {
            let reference = match reference.as_object() {
                Some(r) if r.has_exact_keys(&["repository", "path"]) => r,
                _ => return Err(result_error("guide file references need repository and path")),
            };
            let root = normalize_root(reference.get("repository"))?;
            let path_value = reference.get("path").cloned().unwrap_or(Value::Null);
            let path = normalize_paths(Some(&Value::Array(vec![path_value])))?
                .expect("one path")
                .remove(0);
            let identity = (root.clone(), path.clone());
            if seen.contains(&identity) {
                return Err(result_error(format!("duplicate guide file reference: {}", path.display())));
            }
            seen.push(identity);
            references.push(Value::Object(Object::new().with("repository", root).with("path", path)));
        }
        out_chapters.push(Value::Object(
            Object::new()
                .with("title", guide_text(chapter.get("title").expect("required"), "chapter title", 200)?)
                .with("summary", guide_text(chapter.get("summary").expect("required"), "chapter summary", 8000)?)
                .with("files", Value::Array(references)),
        ));
    }
    normalized.insert("chapters", Value::Array(out_chapters));
    let normalized = Value::Object(normalized);
    if json::dumps(&normalized, json::DEFAULT).len() > MAX_GUIDE_BYTES {
        return Err(result_error("guide exceeds the 48 KiB limit"));
    }
    Ok(normalized)
}

pub fn read_guide(path: &[u8]) -> Result<Value> {
    use std::io::Read;
    let usage = |message: String| Error::Usage(format!("unable to read guide: {message}"));
    let mut raw = Vec::new();
    let file = std::fs::File::open(os::os_path(path)).map_err(|e| usage(crate::error::os_error(&e, Some(path))))?;
    file.take(MAX_GUIDE_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|e| usage(crate::error::os_error(&e, Some(path))))?;
    if raw.len() > MAX_GUIDE_BYTES {
        return Err(result_error("guide exceeds the 48 KiB limit"));
    }
    let text = std::str::from_utf8(&raw).map_err(|e| usage(utf8_error_message(&raw, &e)))?;
    let value = json::loads_str(text).map_err(|e| usage(e.0))?;
    normalize_guide(&value)
}

/// Python's `UnicodeDecodeError` wording for a strict UTF-8 decode.
pub fn utf8_error_message(raw: &[u8], error: &std::str::Utf8Error) -> String {
    let position = error.valid_up_to();
    let byte = raw[position];
    let reason = match error.error_len() {
        None => "unexpected end of data",
        Some(_) if !(0xC2..=0xF4).contains(&byte) => "invalid start byte",
        Some(_) => "invalid continuation byte",
    };
    format!("'utf-8' codec can't decode byte 0x{byte:02x} in position {position}: {reason}")
}

pub fn normalize_request(payload: &Value, require_review_fields: bool) -> Result<ReviewRequest> {
    let Some(object) = payload.as_object() else {
        return Err(result_error("the review request must be a JSON object"));
    };
    let version = object.get("version");
    if !matches!(version, Some(v) if v.is_str(PROTOCOL_VERSION)) {
        return Err(result_error(format!("unsupported review protocol version: {}", value_repr(version))));
    }
    let review_id = object.get("review_id").and_then(Value::as_pystr);
    let criterion = object.get("criterion").and_then(Value::as_pystr);
    let (review_id, criterion) = if require_review_fields {
        let review_id = match review_id {
            Some(s) if !s.is_empty() => s.clone(),
            _ => return Err(result_error("the review request needs a non-empty review_id")),
        };
        let criterion = match criterion {
            Some(s) if !s.is_empty() => s.clone(),
            _ => return Err(result_error("the review request needs a non-empty criterion")),
        };
        (review_id, criterion)
    } else {
        (review_id.cloned().unwrap_or_default(), criterion.cloned().unwrap_or_default())
    };
    let repositories = match object.get("repositories").and_then(Value::as_array) {
        Some(r) if !r.is_empty() => r,
        _ => return Err(result_error("the review request needs at least one repository")),
    };
    let normalized: Vec<Repository> = repositories.iter().map(normalize_repository).collect::<Result<_>>()?;
    let mut seen: Vec<&PyStr> = Vec::new();
    for repository in &normalized {
        if seen.contains(&&repository.root) {
            return Err(result_error(format!("duplicate repository root: {}", repository.root.display())));
        }
        seen.push(&repository.root);
    }
    let guide = match object.get("guide") {
        Some(g) => Some(normalize_guide(g)?),
        None => None,
    };
    Ok(ReviewRequest {
        version: PROTOCOL_VERSION.into(),
        review_id,
        criterion,
        repositories: normalized,
        guide,
    })
}

pub fn parse_request_json(text: &PyStr, require_review_fields: bool) -> Result<ReviewRequest> {
    let payload = json::loads(text).map_err(|e| result_error(format!("the review request is not valid JSON: {e}")))?;
    normalize_request(&payload, require_review_fields)
}

pub fn scoped_repositories(
    roots: &[PyStr],
    paths: &Option<Vec<PyStr>>,
    base: &Option<String>,
    head: &Option<String>,
) -> Result<Vec<Repository>> {
    roots
        .iter()
        .map(|root| {
            Ok(Repository { root: normalize_root_str(root)?, paths: paths.clone(), base: base.clone(), head: head.clone() })
        })
        .collect()
}

pub fn build_request(criterion: &PyStr, repositories: &[Repository]) -> Result<ReviewRequest> {
    let payload = Value::Object(
        Object::new()
            .with("version", PROTOCOL_VERSION)
            .with("review_id", os::uuid4_hex())
            .with("criterion", criterion)
            .with("repositories", Value::Array(repositories.iter().map(Repository::payload).collect())),
    );
    normalize_request(&payload, true)
}

/// Python's `repr()` of a decoded JSON value, for `{value!r}` messages.
pub fn value_repr(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(Value::Int(i)) => i.to_string(),
        Some(Value::BigInt(d)) => d.clone(),
        Some(Value::Float(f)) => json::float_repr(*f, false),
        Some(Value::Str(s)) => py_repr(s),
        Some(Value::Array(items)) => {
            let inner: Vec<String> = items.iter().map(|i| value_repr(Some(i))).collect();
            format!("[{}]", inner.join(", "))
        }
        Some(Value::Object(o)) => {
            let inner: Vec<String> = o.iter().map(|(k, v)| format!("{}: {}", py_repr(k), value_repr(Some(v)))).collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guide(extra: &str) -> Value {
        json::loads_str(&format!(
            r#"{{"version": 1, "fingerprint": "{}", "summary": "s", "before": "b", "after": "a",
               "chapters": [{{"title": "t", "summary": "c", "files": [{{"repository": "/", "path": "x.py"}}]}}]{extra}}}"#,
            "a".repeat(64)
        ))
        .unwrap()
    }

    #[test]
    fn paths_must_be_normalised_and_relative() {
        for bad in ["/a", "C:/a", "a//b", "./a", "a/..", ""] {
            let value = Value::Array(vec![Value::str(bad)]);
            assert!(normalize_paths(Some(&value)).is_err(), "{bad}");
        }
        let ok = normalize_paths(Some(&Value::Array(vec![Value::str("a\\b")]))).unwrap().unwrap();
        assert_eq!(ok, vec![PyStr::from("a/b")]);
    }

    #[test]
    fn guide_validation_matches_python_rules() {
        assert!(normalize_guide(&guide("")).is_ok());
        assert!(normalize_guide(&guide(r#", "flow": {"before": ["x"], "after": ["y"]}"#)).is_ok());
        assert!(normalize_guide(&guide(r#", "extra": 1"#)).is_err());
        let mut bad_version = guide("");
        if let Value::Object(o) = &mut bad_version {
            o.insert("version", Value::Bool(true));
        }
        assert!(normalize_guide(&bad_version).is_err(), "True is not the integer 1");
    }

    #[test]
    fn requests_reject_bad_versions_and_duplicates() {
        let err = normalize_request(&json::loads_str(r#"{"version": 1}"#).unwrap(), true).unwrap_err();
        assert_eq!(err.message(), "unsupported review protocol version: 1");
        let dup = json::loads_str(r#"{"version":"1","review_id":"r","criterion":"c","repositories":[{"root":"/"},{"root":"/"}]}"#).unwrap();
        assert_eq!(normalize_request(&dup, true).unwrap_err().message(), "duplicate repository root: /");
    }

    #[test]
    fn revisions_must_be_safe_git_arguments() {
        for bad in ["", "-x", "--output=x", "a b", "main\n", "caf\u{e9}"] {
            assert!(normalize_revision(Some(&Value::str(bad)), "base").is_err(), "{bad:?}");
        }
        assert!(normalize_revision(Some(&Value::Int(1)), "base").is_err());
        for good in ["main", "origin/main", "HEAD~3", "v1.0^", "a1b2c3"] {
            assert_eq!(normalize_revision(Some(&Value::str(good)), "base").unwrap().as_deref(), Some(good));
        }
        let head_only = json::loads_str(r#"{"root":"/","head":"main"}"#).unwrap();
        assert_eq!(normalize_repository(&head_only).unwrap_err().message(), "a repository head needs a base");
    }
}
