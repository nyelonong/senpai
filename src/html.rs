//! The built-in reviewer: one local HTML page per review.
//!
//! It speaks review protocol v1 like any other reviewer: one JSON request on
//! argv or stdin, one JSON result line on stdout. The diff comes from the
//! client's own snapshot code, so the page can never show a different change
//! than the one the client fingerprints. A page closed without a decision is
//! `cancelled`, never an approval.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use fancy_regex::Regex;

use crate::commands::validate_guide_snapshot;
use crate::error::{os_error, Error, Result};
use crate::git::{Git, SystemGit};
use crate::json::{self, Object, Value};
use crate::ledger::DECISIONS;
use crate::markdown;
use crate::os;
use crate::pystr::{py_repr, py_splitlines, py_strip, PyStr};
use crate::request::{parse_request_json, DocumentRequest, Repository, ReviewRequest, PROTOCOL_VERSION};
use crate::result::{is_annotation, is_document_annotation, validate_decision_semantics, ReviewResult};
use crate::snapshot::{self, collect_snapshot, split_patch_lines, EntryKind, StatusEntry};

pub const PRODUCER: &str = "html_review.py";
const HOST: &str = "127.0.0.1";
const NO_BROWSER_ENV: &str = "HTML_REVIEW_NO_BROWSER";
const URL_FILE_ENV: &str = "HTML_REVIEW_URL_FILE";
const LIFETIME_ENV: &str = "HTML_REVIEW_LIFETIME";
const IDLE_ENV: &str = "HTML_REVIEW_IDLE";
const LIFETIME_SECONDS: f64 = 1800.0;
const IDLE_SECONDS: f64 = 900.0;
const CLOSE_GRACE_SECONDS: f64 = 5.0;
const MAX_BODY_BYTES: usize = 8 << 20;
const MAX_RENDERED_LINES_PER_FILE: usize = 1500;
const MAX_HIGHLIGHT_BYTES: u64 = 400_000;

const PAGE_TEMPLATE: &str = include_str!("../assets/review-page.html");
const DIFF_VIEW: &str = include_str!("../assets/review-diff.js");
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'self' 'unsafe-inline'; connect-src 'self'; font-src 'self'";

const FONTS: [(&str, &[u8]); 4] = [
    ("jetbrains-mono-400.woff2", include_bytes!("../assets/fonts/jetbrains-mono-400.woff2")),
    ("jetbrains-mono-700.woff2", include_bytes!("../assets/fonts/jetbrains-mono-700.woff2")),
    ("inter-latin-var.woff2", include_bytes!("../assets/fonts/inter-latin-var.woff2")),
    ("source-serif-4-latin-var.woff2", include_bytes!("../assets/fonts/source-serif-4-latin-var.woff2")),
];
const VENDOR: [(&str, &[u8]); 5] = [
    ("markdown-it.min.js", include_bytes!("../assets/vendor/markdown-it.min.js")),
    ("markdown-it-task-lists.min.js", include_bytes!("../assets/vendor/markdown-it-task-lists.min.js")),
    ("markdown-it-footnote.min.js", include_bytes!("../assets/vendor/markdown-it-footnote.min.js")),
    ("highlight.min.js", include_bytes!("../assets/vendor/highlight.min.js")),
    ("mermaid.min.js", include_bytes!("../assets/vendor/mermaid.min.js")),
];

// ---------------------------------------------------------------------------
// requests
// ---------------------------------------------------------------------------

/// Validates a request the way RDG's `ReviewRequest::validate` does: the
/// client's rules plus non-blank review id and criterion.
pub fn parse_request(text: &PyStr) -> Result<ReviewRequest> {
    let request = parse_request_json(text, true)?;
    if request.review_id.is_blank() {
        return Err(Error::Result("review_id must not be empty".into()));
    }
    if request.criterion.is_blank() {
        return Err(Error::Result("criterion must not be empty".into()));
    }
    Ok(request)
}

pub fn parse_document_request(text: &PyStr) -> Result<DocumentRequest> {
    let fail = |m: String| Error::Document(m);
    let payload = json::loads(text).map_err(|e| fail(format!("parsing document request JSON: {e}")))?;
    let Some(object) = payload.as_object() else {
        return Err(fail("a document request must be a JSON object".into()));
    };
    let version = object.get("version");
    if !matches!(version, Some(v) if v.is_str(PROTOCOL_VERSION)) {
        let shown = match version {
            Some(Value::Str(s)) => s.display(),
            other => crate::request::value_repr(other),
        };
        return Err(fail(format!("unsupported review protocol version: {shown}")));
    }
    let review_id = match object.get("review_id") {
        Some(Value::Str(s)) if !s.is_blank() => s.clone(),
        _ => return Err(fail("review_id must not be empty".into())),
    };
    let criterion = match object.get("criterion") {
        Some(Value::Str(s)) if !s.is_blank() => s.clone(),
        _ => return Err(fail("criterion must not be empty".into())),
    };
    let raw_path = match object.get("path") {
        Some(Value::Str(s)) if !s.is_blank() => s.clone(),
        _ => return Err(fail("a document review needs a document path".into())),
    };
    let encoded = crate::request::encode_path(&raw_path)?;
    let path = os::abspath(&os::expanduser(&encoded));
    if !std::fs::metadata(os::os_path(&path)).map(|m| m.is_file()).unwrap_or(false) {
        return Err(fail(format!("the document does not exist: {}", raw_path.display())));
    }
    Ok(DocumentRequest {
        version: PROTOCOL_VERSION.into(),
        review_id,
        criterion,
        path: PyStr::decode_surrogateescape(&path),
    })
}

// ---------------------------------------------------------------------------
// diff payload
// ---------------------------------------------------------------------------

static HUNK_HEADER: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$").expect("hunk pattern"));

#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    pub kind: &'static str,
    pub old_line: Option<u64>,
    pub new_line: Option<u64>,
    pub text: String,
}

impl DiffLine {
    fn payload(&self) -> Value {
        Value::Object(
            Object::new()
                .with("type", self.kind)
                .with("old_line", self.old_line.map(|n| Value::Int(n as i128)))
                .with("new_line", self.new_line.map(|n| Value::Int(n as i128)))
                .with("text", self.text.as_str()),
        )
    }
}

fn digits_value(text: &str) -> u64 {
    text.chars().filter_map(|c| c.to_digit(10)).fold(0u64, |acc, d| acc.saturating_mul(10).saturating_add(d as u64))
}

/// Splits one file patch into display lines, with counts and a binary flag.
pub fn parse_patch(patch: &[u8]) -> (Vec<DiffLine>, usize, usize, bool) {
    let text = String::from_utf8_lossy(patch);
    let mut lines = Vec::new();
    let (mut added, mut removed, mut binary) = (0, 0, false);
    let (mut old_line, mut new_line): (Option<u64>, Option<u64>) = (None, None);
    let mut in_hunk = false;
    let meta = |raw: &str| DiffLine { kind: "meta", old_line: None, new_line: None, text: raw.to_string() };
    for raw in split_patch_lines(&text) {
        if let Ok(Some(header)) = HUNK_HEADER.captures(raw) {
            in_hunk = true;
            old_line = Some(digits_value(header.get(1).map_or("", |m| m.as_str()))).filter(|n| *n != 0);
            new_line = Some(digits_value(header.get(3).map_or("", |m| m.as_str()))).filter(|n| *n != 0);
            lines.push(meta(raw));
            continue;
        }
        if !in_hunk {
            if raw.starts_with("Binary files ") || raw == "GIT binary patch" {
                binary = true;
            }
            if !raw.is_empty() {
                lines.push(meta(raw));
            }
            continue;
        }
        let rest = || raw.char_indices().nth(1).map_or("", |(i, _)| &raw[i..]).to_string();
        if raw.starts_with('\\') {
            lines.push(meta(raw));
        } else if raw.starts_with('+') {
            added += 1;
            lines.push(DiffLine { kind: "add", old_line: None, new_line, text: rest() });
            new_line = Some(new_line.unwrap_or(0) + 1);
        } else if raw.starts_with('-') {
            removed += 1;
            lines.push(DiffLine { kind: "remove", old_line, new_line: None, text: rest() });
            old_line = Some(old_line.unwrap_or(0) + 1);
        } else if raw.is_empty() || raw.starts_with(' ') {
            lines.push(DiffLine { kind: "context", old_line, new_line, text: if raw.is_empty() { String::new() } else { rest() } });
            old_line = Some(old_line.unwrap_or(0) + 1);
            new_line = Some(new_line.unwrap_or(0) + 1);
        } else {
            lines.push(meta(raw));
        }
    }
    (lines, added, removed, binary)
}

fn status_for(entry: &StatusEntry) -> &'static str {
    if entry.kind == EntryKind::Untracked {
        "added"
    } else if entry.original_path.as_ref().is_some_and(|p| !p.is_empty()) {
        "renamed"
    } else if entry.mode_worktree == "000000" {
        "deleted"
    } else {
        "modified"
    }
}

pub fn language_for(path: &PyStr) -> Option<&'static str> {
    let display = path.display();
    let name = display.rsplit('/').next().unwrap_or("").to_lowercase();
    match name.as_str() {
        "makefile" => return Some("makefile"),
        "dockerfile" => return Some("dockerfile"),
        "gemfile" | "rakefile" => return Some("ruby"),
        _ => {}
    }
    let dot = name.rfind('.')?;
    if dot == 0 {
        return None;
    }
    Some(match &name[dot + 1..] {
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "tsx" => "typescript",
        "rs" => "rust",
        "go" => "go",
        "yml" => "yaml",
        "sh" | "bash" | "zsh" => "bash",
        "md" | "markdown" => "markdown",
        "html" | "htm" | "vue" => "xml",
        "rb" => "ruby",
        "pl" => "perl",
        "cs" => "csharp",
        "kt" => "kotlin",
        "swift" => "swift",
        "scala" => "scala",
        "lua" => "lua",
        _ => return None,
    })
}

fn read_for_highlighting(root: &[u8], path: &PyStr, binary: bool) -> Option<String> {
    if binary {
        return None;
    }
    let absolute = snapshot::absolute_path(root, path).ok()?;
    let metadata = std::fs::metadata(os::os_path(&absolute)).ok()?;
    if metadata.len() > MAX_HIGHLIGHT_BYTES {
        return None;
    }
    let raw = std::fs::read(os::os_path(&absolute)).ok()?;
    if raw.contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&raw).into_owned())
}

fn collect_file_diffs(git: &dyn Git, repository: &Repository) -> Result<Vec<Object>> {
    let root = repository.root_bytes()?;
    let baseline = snapshot::resolve_baseline(git, &root)?;
    let mut diffs = Vec::new();
    for entry in snapshot::selected_entries(git, repository)? {
        let patch = snapshot::entry_patch(git, &root, &baseline, &entry)?;
        let (lines, added, removed, binary) = parse_patch(&patch);
        let mut file = Object::new()
            .with("repository", &repository.root)
            .with("path", &entry.path)
            .with("old_path", entry.original_path.clone())
            .with("status", status_for(&entry))
            .with("binary", binary)
            .with("added", added)
            .with("removed", removed)
            .with("lines", Value::Array(lines.iter().map(DiffLine::payload).collect()));
        if let Some(language) = language_for(&entry.path) {
            file.insert("language", Value::from(language));
        }
        if let Some(text) = read_for_highlighting(&root, &entry.path, binary) {
            file.insert("text", Value::from(text));
        }
        diffs.push(file);
    }
    Ok(diffs)
}

fn int_field(object: &Object, key: &str) -> i128 {
    object.get(key).and_then(Value::as_int).unwrap_or(0)
}

/// The review document the page renders: criterion, repositories, diff.
pub fn build_payload(git: &dyn Git, request: &ReviewRequest) -> Result<Value> {
    if let Some(guide) = &request.guide {
        validate_guide_snapshot(guide, &collect_snapshot(git, &request.repositories)?)?;
    }
    let mut repositories = Vec::new();
    let (mut total_files, mut total_added, mut total_removed) = (0usize, 0i128, 0i128);
    for repository in &request.repositories {
        let diffs = collect_file_diffs(git, repository)?;
        let added: i128 = diffs.iter().map(|d| int_field(d, "added")).sum();
        let removed: i128 = diffs.iter().map(|d| int_field(d, "removed")).sum();
        total_files += diffs.len();
        total_added += added;
        total_removed += removed;
        let paths = repository
            .paths
            .as_ref()
            .filter(|p| !p.is_empty())
            .map(|p| Value::Array(p.iter().map(Value::from).collect()));
        repositories.push(
            Object::new()
                .with("root", &repository.root)
                .with("paths", paths)
                .with("added", Value::Int(added))
                .with("removed", Value::Int(removed))
                .with("files", Value::Array(diffs.into_iter().map(Value::Object).collect())),
        );
    }
    let guide = build_guide(request.guide.as_ref(), &repositories)?;
    Ok(Value::Object(
        Object::new()
            .with("version", PROTOCOL_VERSION)
            .with("review_id", &request.review_id)
            .with("criterion", &request.criterion)
            .with("repositories", Value::Array(repositories.into_iter().map(Value::Object).collect()))
            .with(
                "totals",
                Object::new()
                    .with("files", total_files)
                    .with("added", Value::Int(total_added))
                    .with("removed", Value::Int(total_removed)),
            )
            .with("guide", guide),
    ))
}

fn pystr_field(object: &Object, key: &str) -> PyStr {
    object.get(key).and_then(Value::as_pystr).cloned().unwrap_or_default()
}

fn build_guide(guide: Option<&Value>, repositories: &[Object]) -> Result<Value> {
    let files: Vec<&Object> = repositories
        .iter()
        .flat_map(|r| r.get("files").and_then(Value::as_array).into_iter().flatten())
        .filter_map(Value::as_object)
        .collect();
    let identity = |file: &Object| (pystr_field(file, "repository"), pystr_field(file, "path"));
    let mut chapters = Vec::new();
    let mut result = Object::new().with("authored", guide.is_some());
    match guide {
        Some(guide) => {
            let mut covered = vec![false; files.len()];
            for chapter in guide.get("chapters").and_then(Value::as_array).into_iter().flatten() {
                let mut members = Vec::new();
                for reference in chapter.get("files").and_then(Value::as_array).into_iter().flatten() {
                    let wanted = (
                        reference.get("repository").and_then(Value::as_pystr).cloned().unwrap_or_default(),
                        reference.get("path").and_then(Value::as_pystr).cloned().unwrap_or_default(),
                    );
                    let Some(index) = files.iter().position(|f| identity(f) == wanted) else {
                        return Err(Error::Result(format!(
                            "guide references a file outside the captured diff: {}",
                            wanted.1.display()
                        )));
                    };
                    if covered[index] {
                        return Err(Error::Result(format!("duplicate guide file reference: {}", wanted.1.display())));
                    }
                    covered[index] = true;
                    members.push(Value::from(index));
                }
                chapters.push(Value::Object(
                    Object::new()
                        .with("title", chapter.get("title").cloned().unwrap_or(Value::Null))
                        .with("summary", chapter.get("summary").cloned().unwrap_or(Value::Null))
                        .with("file_indices", Value::Array(members)),
                ));
            }
            let remaining: Vec<Value> = (0..files.len()).filter(|i| !covered[*i]).map(Value::from).collect();
            if !remaining.is_empty() {
                chapters.push(Value::Object(
                    Object::new()
                        .with("title", "Other changes")
                        .with("summary", "These files were not assigned to an authored chapter. Review them too.")
                        .with("file_indices", Value::Array(remaining)),
                ));
            }
        }
        None => {
            let mut groups: Vec<((PyStr, PyStr), Vec<Value>)> = Vec::new();
            for (index, file) in files.iter().enumerate() {
                let path = pystr_field(file, "path");
                let directory = match path.as_wtf8().iter().rposition(|&b| b == b'/') {
                    Some(cut) => PyStr::from_wtf8_unchecked(path.as_wtf8()[..cut].to_vec()),
                    None => PyStr::from("Root files"),
                };
                let key = (pystr_field(file, "repository"), directory);
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, members)) => members.push(Value::from(index)),
                    None => groups.push((key, vec![Value::from(index)])),
                }
            }
            for ((root, directory), members) in groups {
                let title = if repositories.len() > 1 {
                    let mut t = root.clone();
                    t.push_str(" \u{00B7} ");
                    t.push_pystr(&directory);
                    t
                } else {
                    directory
                };
                chapters.push(Value::Object(
                    Object::new()
                        .with("title", title)
                        .with("summary", "Grouped by directory, not by behavior.")
                        .with("file_indices", Value::Array(members)),
                ));
            }
        }
    }
    result.insert("chapters", Value::Array(chapters));
    match guide {
        Some(guide) => {
            for key in ["summary", "before", "after", "flow"] {
                if let Some(value) = guide.get(key) {
                    result.insert(key, value.clone());
                }
            }
        }
        None => result.insert(
            "summary",
            Value::from(
                "No agent-authored guide was supplied. These directory groups show every scoped file without inferring what the change does.",
            ),
        ),
    }
    Ok(Value::Object(result))
}

// ---------------------------------------------------------------------------
// document payload
// ---------------------------------------------------------------------------

fn mark_fence_languages(blocks: &mut [Object]) {
    for block in blocks {
        if !matches!(block.get("kind"), Some(k) if k.is_str("code")) {
            continue;
        }
        let Some(source) = block.get("source").and_then(Value::as_pystr).map(PyStr::display) else {
            continue;
        };
        let opening = py_strip(source.split('\n').next().unwrap_or(""));
        let info = py_strip(opening.trim_start_matches(['`', '~']));
        let language = info.split(crate::pystr::py_isspace_char).find(|w| !w.is_empty()).unwrap_or("");
        block.insert("language", Value::from(language));
    }
}

pub fn build_document_payload(request: &DocumentRequest) -> Result<Value> {
    let path = request.path_bytes()?;
    let raw = std::fs::read(os::os_path(&path))
        .map_err(|e| Error::Document(format!("the document could not be read: {}", os_error(&e, Some(&path)))))?;
    let decoded = String::from_utf8_lossy(&raw);
    // Text mode reading translates every line ending to "\n" first.
    let text = decoded.replace("\r\n", "\n").replace('\r', "\n");
    let mut blocks = markdown::blocks(&text);
    mark_fence_languages(&mut blocks);
    Ok(Value::Object(
        Object::new()
            .with("version", PROTOCOL_VERSION)
            .with("review_id", &request.review_id)
            .with("criterion", &request.criterion)
            .with("path", &request.path)
            .with("name", PyStr::decode_surrogateescape(&os::basename(&path)))
            .with("lines", py_splitlines(&text).len())
            .with("rows", Value::Array(markdown::rows(&text).into_iter().map(Value::Object).collect()))
            .with("blocks", Value::Array(blocks.into_iter().map(Value::Object).collect())),
    ))
}

// ---------------------------------------------------------------------------
// decisions
// ---------------------------------------------------------------------------

type LineKey = (PyStr, PyStr, &'static str);

enum Index {
    Diff(Vec<(LineKey, Vec<i128>)>),
    Document(Vec<i128>),
}

fn line_index(payload: &Value) -> Index {
    let mut index: Vec<(LineKey, Vec<i128>)> = Vec::new();
    let mut add = |key: LineKey, line: i128| match index.iter_mut().find(|(k, _)| *k == key) {
        Some((_, lines)) => lines.push(line),
        None => index.push((key, vec![line])),
    };
    for repository in payload.get("repositories").and_then(Value::as_array).into_iter().flatten() {
        for file in repository.get("files").and_then(Value::as_array).into_iter().flatten() {
            let Some(file) = file.as_object() else { continue };
            let (root, path) = (pystr_field(file, "repository"), pystr_field(file, "path"));
            for line in file.get("lines").and_then(Value::as_array).into_iter().flatten() {
                if let Some(n) = line.get("new_line").and_then(Value::as_int) {
                    add((root.clone(), path.clone(), "new"), n);
                }
                if let Some(n) = line.get("old_line").and_then(Value::as_int) {
                    add((root.clone(), path.clone(), "old"), n);
                }
            }
        }
    }
    Index::Diff(index)
}

fn document_line_index(payload: &Value) -> Index {
    let mut lines = Vec::new();
    for block in payload.get("blocks").and_then(Value::as_array).into_iter().flatten() {
        let start = block.get("start").and_then(Value::as_int).unwrap_or(0);
        let end = block.get("end").and_then(Value::as_int).unwrap_or(-1);
        lines.extend(start..=end);
    }
    Index::Document(lines)
}

#[derive(Debug)]
struct DecisionError(String);

fn semantic_message(error: Error) -> DecisionError {
    let message = error.message();
    DecisionError(match message.as_str() {
        "the reviewer requested changes without feedback" => "a request for changes needs at least one comment".into(),
        "the reviewer returned annotations for a cancelled review" => {
            "a cancelled review must not carry annotations".into()
        }
        _ => message,
    })
}

fn line_number(value: Option<&Value>) -> (Option<i128>, String) {
    match value {
        Some(Value::Int(n)) => (Some(*n), n.to_string()),
        Some(Value::BigInt(d)) => (None, d.clone()),
        other => (None, crate::request::value_repr(other)),
    }
}

fn parse_decision(payload: &Value, review_id: &PyStr, document: Option<&PyStr>, index: &Index) -> std::result::Result<ReviewResult, DecisionError> {
    let fail = |m: String| DecisionError(m);
    let Some(object) = payload.as_object() else {
        return Err(fail("the decision must be a JSON object".into()));
    };
    let decision = object.get("decision");
    let decision = match decision {
        Some(Value::Str(d)) if DECISIONS.iter().any(|k| d == k) => d.display(),
        other => return Err(fail(format!("unknown decision: {}", crate::request::value_repr(other)))),
    };
    let Some(raw) = object.get("annotations").and_then(Value::as_array) else {
        return Err(fail("the annotations must be a list".into()));
    };
    let mut annotations = Vec::new();
    for item in raw {
        let well_formed = match document {
            Some(path) => is_document_annotation(item, path),
            None => is_annotation(item),
        };
        if !well_formed {
            return Err(fail("malformed annotation".into()));
        }
        let text = item.get("text").and_then(Value::as_pystr).expect("checked").strip();
        if text.is_empty() {
            return Err(fail("an annotation must carry text".into()));
        }
        let target = item.get("target").and_then(Value::as_object).expect("checked");
        if !matches!(target.get("scope"), Some(s) if s.is_str("line")) {
            annotations.push(Value::Object(
                Object::new().with("text", text).with("target", Object::new().with("scope", "review")),
            ));
            continue;
        }
        match (document, index) {
            (Some(path), Index::Document(lines)) => {
                for field in ["start_line", "end_line"] {
                    let (number, shown) = line_number(target.get(field));
                    if !number.is_some_and(|n| lines.contains(&n)) {
                        return Err(fail(format!("{field} {shown} is not a line of {}", path.display())));
                    }
                }
                annotations.push(Value::Object(
                    Object::new().with("text", text).with(
                        "target",
                        Object::new()
                            .with("scope", "line")
                            .with("path", path)
                            .with("start_line", target.get("start_line").cloned().unwrap_or(Value::Null))
                            .with("end_line", target.get("end_line").cloned().unwrap_or(Value::Null)),
                    ),
                ));
            }
            (None, Index::Diff(entries)) => {
                let repository = pystr_field(target, "repository");
                let path = pystr_field(target, "path");
                let side = if matches!(target.get("side"), Some(s) if s.is_str("old")) { "old" } else { "new" };
                let key = (repository.clone(), path.clone(), side);
                let Some((_, available)) = entries.iter().find(|(k, _)| *k == key) else {
                    return Err(fail(format!("the comment target {} is not part of this review", py_repr(&path))));
                };
                for field in ["start_line", "end_line"] {
                    let (number, shown) = line_number(target.get(field));
                    if !number.is_some_and(|n| available.contains(&n)) {
                        return Err(fail(format!(
                            "{field} {shown} is not a line of {} in this review",
                            py_repr(&path)
                        )));
                    }
                }
                annotations.push(Value::Object(
                    Object::new().with("text", text).with(
                        "target",
                        Object::new()
                            .with("scope", "line")
                            .with("repository", repository)
                            .with("path", path)
                            .with("side", side)
                            .with("start_line", target.get("start_line").cloned().unwrap_or(Value::Null))
                            .with("end_line", target.get("end_line").cloned().unwrap_or(Value::Null)),
                    ),
                ));
            }
            _ => return Err(fail("malformed annotation".into())),
        }
    }
    let result = ReviewResult { decision, annotations, review_id: review_id.clone() };
    validate_decision_semantics(&result).map_err(semantic_message)?;
    Ok(result)
}

// ---------------------------------------------------------------------------
// the page and the server
// ---------------------------------------------------------------------------

/// One review, built and checked by the reviewer process, ready to be shown
/// either on its own page or in the inbox.
pub(crate) struct Prepared {
    pub(crate) title: PyStr,
    pub(crate) places: Vec<PyStr>,
    pub(crate) review_id: PyStr,
    pub(crate) document: Option<PyStr>,
    pub(crate) payload: Value,
    pub(crate) mode: &'static str,
}

pub fn render_page(token: &str, mode: &str, page: u64) -> Vec<u8> {
    let diff = mode == "diff";
    let boot = Object::new()
        .with("url", format!("/{token}/"))
        .with("page", Value::Int(page as i128))
        .with("maxLines", MAX_RENDERED_LINES_PER_FILE)
        .with("mode", mode)
        .with("reviewUrl", if diff { "api/review" } else { "api/doc" })
        .with("decisionUrl", if diff { "api/decision" } else { "api/doc-decision" });
    PAGE_TEMPLATE
        .replace("__HTML_REVIEW_BOOT__", &json::dumps(&Value::Object(boot), json::COMPACT))
        .replace("__HTML_DIFF_VIEW__", DIFF_VIEW)
        .into_bytes()
}

struct State {
    result: Option<ReviewResult>,
    page: u64,
    closing: Option<u64>,
}

pub(crate) struct Session {
    review_id: PyStr,
    document: Option<PyStr>,
    payload_json: Vec<u8>,
    index: Index,
    token: String,
    mode: &'static str,
    state: Mutex<State>,
    decided: Condvar,
    requests: AtomicU64,
    close_generation: AtomicU64,
    stopped: AtomicBool,
    close_grace: Option<Duration>,
}

impl Session {
    /// A session for one prepared payload. Without a close grace, closing the
    /// page never withdraws the review: the inbox keeps it until a decision.
    pub(crate) fn new(prepared: &Prepared, token: String, close_grace: Option<Duration>) -> Self {
        let index = match prepared.mode {
            "document" => document_line_index(&prepared.payload),
            _ => line_index(&prepared.payload),
        };
        Session {
            review_id: prepared.review_id.clone(),
            document: prepared.document.clone(),
            payload_json: json::dumps(&prepared.payload, json::COMPACT).into_bytes(),
            index,
            token,
            mode: prepared.mode,
            state: Mutex::new(State { result: None, page: 0, closing: None }),
            decided: Condvar::new(),
            requests: AtomicU64::new(0),
            close_generation: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            close_grace,
        }
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    /// A decision, cancellation or withdrawal has ended this review.
    pub(crate) fn settled(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// The page has been requested at least once.
    pub(crate) fn opened(&self) -> bool {
        self.requests.load(Ordering::SeqCst) > 0
    }

    fn cancelled(&self) -> ReviewResult {
        ReviewResult { decision: "cancelled".into(), annotations: Vec::new(), review_id: self.review_id.clone() }
    }

    /// Records the person's decision unless the review already ended. The
    /// reply goes out while the state is locked, so a withdrawal cannot land
    /// between it and the record, and the reply precedes the process exit.
    fn decide(&self, result: ReviewResult, reply: impl FnOnce(bool)) {
        let mut state = self.state.lock().expect("session state");
        let open = state.result.is_none();
        reply(open);
        if open {
            state.result = Some(result);
            self.stopped.store(true, Ordering::SeqCst);
            self.decided.notify_all();
        }
    }

    fn submit(&self, result: ReviewResult) {
        let mut state = self.state.lock().expect("session state");
        if state.result.is_none() {
            state.result = Some(result);
        }
        self.stopped.store(true, Ordering::SeqCst);
        self.decided.notify_all();
    }

    pub(crate) fn stop(&self) {
        self.submit(self.cancelled());
    }

    fn next_page(&self) -> u64 {
        let mut state = self.state.lock().expect("session state");
        state.page += 1;
        state.closing = None;
        self.close_generation.fetch_add(1, Ordering::SeqCst);
        state.page
    }

    /// A page is closing: withdraw the review unless a newer page exists or is
    /// requested within the grace period.
    fn begin_close(self: &Arc<Self>, page: u64) {
        let Some(grace) = self.close_grace else {
            return;
        };
        let mut state = self.state.lock().expect("session state");
        if state.result.is_some() || state.closing.is_some() || page != state.page {
            return;
        }
        let generation = self.close_generation.fetch_add(1, Ordering::SeqCst) + 1;
        state.closing = Some(generation);
        let session = Arc::clone(self);
        thread::spawn(move || {
            thread::sleep(grace);
            let still_closing = session.state.lock().map(|s| s.closing == Some(generation)).unwrap_or(false);
            if still_closing {
                session.stop();
            }
        });
    }

    pub(crate) fn wait(&self) -> ReviewResult {
        let mut state = self.state.lock().expect("session state");
        while state.result.is_none() {
            state = self.decided.wait(state).expect("session state");
        }
        state.result.take().expect("decided")
    }
}

pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
    if !a.is_ascii() || !b.is_ascii() || a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(crate) fn respond(request: tiny_http::Request, status: u16, body: Vec<u8>, content_type: &str) {
    let mut headers = vec![
        ("Content-Security-Policy", CONTENT_SECURITY_POLICY),
        ("X-Content-Type-Options", "nosniff"),
        ("Referrer-Policy", "no-referrer"),
        ("Cache-Control", "no-store"),
        ("Server", "html-review"),
    ];
    if status != 204 {
        headers.push(("Content-Type", content_type));
    }
    let mut response = tiny_http::Response::from_data(body).with_status_code(status);
    for (name, value) in headers {
        if let Ok(header) = tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response.add_header(header);
        }
    }
    let _ = request.respond(response);
}

pub(crate) fn fail(request: tiny_http::Request, status: u16, message: &str) {
    let body = json::dumps(&Value::Object(Object::new().with("error", message)), json::DEFAULT);
    respond(request, status, body.into_bytes(), "application/json; charset=utf-8");
}

/// The path and query of a request target, as `urllib.parse.urlsplit` reads it,
/// so an absolute-form target such as `http://host/<token>/` routes the same.
pub(crate) fn split_target(target: &str) -> (String, String) {
    let mut rest = target;
    if let Some(colon) = rest.find(':') {
        let scheme = &rest[..colon];
        if scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        {
            rest = &rest[colon + 1..];
        }
    }
    if let Some(after) = rest.strip_prefix("//") {
        rest = after.find(['/', '?', '#']).map_or("", |end| &after[end..]);
    }
    let rest = rest.split_once('#').map_or(rest, |(before, _)| before);
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    (path.to_string(), query.to_string())
}

/// The first non-blank value of `key`, decoded as `urllib.parse.parse_qs` does.
fn query_value(query: &str, key: &str) -> Option<String> {
    let decode = |part: &str| -> String {
        let bytes = part.replace('+', " ").into_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push((high * 16 + low) as u8);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (!value.is_empty() && decode(name) == key).then(|| decode(value))
    })
}

fn route(session: &Session, path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let first = parts.first()?;
    if !constant_time_eq(first, &session.token) {
        return None;
    }
    session.requests.fetch_add(1, Ordering::SeqCst);
    let rest = &parts[1..];
    Some(match rest {
        [] => "page".into(),
        ["api", "review"] => "review".into(),
        ["api", "decision"] => "decision".into(),
        ["api", "doc"] => "doc".into(),
        ["api", "doc-decision"] => "doc-decision".into(),
        ["api", "closing"] => "closing".into(),
        ["font", name] if FONTS.iter().any(|(n, _)| n == name) => "font".into(),
        ["vendor", name] if VENDOR.iter().any(|(n, _)| n == name) => "vendor".into(),
        _ => return None,
    })
}

pub(crate) fn handle(session: &Arc<Session>, mut request: tiny_http::Request) {
    let (path, query) = split_target(request.url());
    let route = route(session, &path);
    let last = path.rsplit('/').next().unwrap_or("").to_string();
    match request.method() {
        tiny_http::Method::Get => match route.as_deref() {
            Some("page") => {
                let page = session.next_page();
                respond(request, 200, render_page(&session.token, session.mode, page), "text/html; charset=utf-8");
            }
            Some("review" | "doc") => {
                respond(request, 200, session.payload_json.clone(), "application/json; charset=utf-8");
            }
            Some("font") => {
                let data = FONTS.iter().find(|(n, _)| *n == last).map(|(_, d)| d.to_vec()).unwrap_or_default();
                respond(request, 200, data, "font/woff2");
            }
            Some("vendor") => {
                let data = VENDOR.iter().find(|(n, _)| *n == last).map(|(_, d)| d.to_vec()).unwrap_or_default();
                respond(request, 200, data, "text/javascript; charset=utf-8");
            }
            None => fail(request, 404, "not found"),
            Some(other) => {
                let message = format!("{other} needs POST");
                fail(request, 405, &message)
            }
        },
        tiny_http::Method::Post => match route.as_deref() {
            Some(other @ ("page" | "review" | "doc" | "font" | "vendor")) => {
                let message = format!("{other} needs POST");
                fail(request, 405, &message)
            }
            Some("closing") => {
                let page = query_value(&query, "page").unwrap_or_default();
                let valid = !page.is_empty() && page.len() <= 9 && page.bytes().all(|b| b.is_ascii_digit());
                match page.parse::<u64>() {
                    Ok(n) if valid && n >= 1 => {
                        session.begin_close(n);
                        respond(request, 204, Vec::new(), "");
                    }
                    _ => fail(request, 400, "a closing report needs the number of the page that is closing"),
                }
            }
            Some(kind @ ("decision" | "doc-decision")) => {
                let kind = kind.to_string();
                let Some(size) = request.body_length() else {
                    return fail(request, 411, "a decision needs a Content-Length");
                };
                if size > MAX_BODY_BYTES {
                    return fail(request, 413, "the decision is too large");
                }
                let mut body = Vec::with_capacity(size);
                if request.as_reader().take(size as u64).read_to_end(&mut body).is_err() {
                    return fail(request, 400, "the decision is not valid JSON");
                }
                let decoded = std::str::from_utf8(&body).ok().and_then(|t| json::loads_str(t).ok());
                let Some(decoded) = decoded else {
                    return fail(request, 400, "the decision is not valid JSON");
                };
                let document = if kind == "doc-decision" { session.document.as_ref() } else { None };
                let parsed = match (&session.index, document) {
                    (Index::Document(_), Some(_)) | (Index::Diff(_), None) => {
                        parse_decision(&decoded, &session.review_id, document, &session.index)
                    }
                    _ => Err(DecisionError("malformed annotation".into())),
                };
                match parsed {
                    Ok(result) => session.decide(result, |open| {
                        if open {
                            respond(request, 204, Vec::new(), "");
                        } else {
                            fail(request, 409, "this review is no longer waiting for a decision");
                        }
                    }),
                    Err(DecisionError(message)) => fail(request, 400, &message),
                }
            }
            _ => fail(request, 404, "not found"),
        },
        _ => fail(request, 501, "unsupported method"),
    }
}

pub(crate) fn publish_url(path: Option<&[u8]>, url: &str) {
    let Some(path) = path.filter(|p| !p.is_empty()) else {
        return;
    };
    use std::os::unix::fs::OpenOptionsExt;
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(os::os_path(path))
        .and_then(|mut f| f.write_all(format!("{url}\n").as_bytes()));
    if let Err(error) = written {
        crate::diag::write(&format!(
            "{PRODUCER}: unable to publish the review URL to {} ({})\n",
            PyStr::decode_surrogateescape(path).display(),
            os_error(&error, Some(path))
        ));
    }
}

pub(crate) fn withdraw_url(path: Option<&[u8]>) {
    if let Some(path) = path.filter(|p| !p.is_empty()) {
        let _ = std::fs::remove_file(os::os_path(path));
    }
}

pub(crate) fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let spawned = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            thread::spawn(move || child.wait());
        }
        Err(error) => crate::diag::write(&format!("{PRODUCER}: unable to open a browser ({error}); open the URL above\n")),
    }
}

pub(crate) fn env_flag_enabled(name: &str) -> bool {
    let value = std::env::var(name).unwrap_or_default();
    matches!(py_strip(&value).to_lowercase().as_str(), "" | "0" | "false" | "no" | "off")
}

fn positive_seconds(name: &str, default: f64) -> f64 {
    let raw = std::env::var(name).unwrap_or_default();
    let raw = py_strip(&raw);
    if raw.is_empty() {
        return default;
    }
    match raw.replace('_', "").parse::<f64>() {
        Ok(v) if v > 0.0 => v,
        _ => default,
    }
}

fn duration(seconds: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(seconds).ok()
}

/// Serves the page until a decision arrives, then returns it. The review is
/// bounded: it ends if the client is gone, the lifetime is spent, or the page
/// is never opened.
fn serve_and_wait(session: Arc<Session>, lifetime: f64, idle: f64, browser: bool) -> Result<ReviewResult> {
    let server = tiny_http::Server::http(format!("{HOST}:0"))
        .map_err(|e| Error::Other(format!("unable to start the review page: {e}")))?;
    let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
    let url = format!("http://{HOST}:{port}/{}/", session.token);
    let server = Arc::new(server);
    let serving = {
        let server = Arc::clone(&server);
        let session = Arc::clone(&session);
        thread::spawn(move || {
            while !session.stopped.load(Ordering::SeqCst) {
                match server.recv_timeout(Duration::from_millis(100)) {
                    Ok(Some(request)) => {
                        let session = Arc::clone(&session);
                        thread::spawn(move || handle(&session, request));
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        })
    };
    crate::diag::write(&format!("{PRODUCER}: review page {url}\n"));
    let url_file = std::env::var_os(URL_FILE_ENV).map(|v| os::path_bytes(std::path::Path::new(&v)));
    publish_url(url_file.as_deref(), &url);
    {
        let session = Arc::clone(&session);
        // SAFETY: getppid has no preconditions.
        let parent = unsafe { libc::getppid() };
        let started = Instant::now();
        thread::spawn(move || {
            let lifetime = duration(lifetime);
            let idle = duration(idle);
            while !session.stopped.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(500));
                // SAFETY: getppid has no preconditions.
                if unsafe { libc::getppid() } != parent {
                    return session.stop();
                }
                let elapsed = started.elapsed();
                if lifetime.is_some_and(|l| elapsed >= l) {
                    return session.stop();
                }
                if idle.is_some_and(|i| elapsed >= i) && session.requests.load(Ordering::SeqCst) == 0 {
                    return session.stop();
                }
            }
        });
    }
    if browser {
        open_browser(&url);
    }
    let result = session.wait();
    withdraw_url(url_file.as_deref());
    let _ = serving.join();
    Ok(result)
}

fn read_request_text(argument: &PyStr) -> PyStr {
    if !argument.is_empty() {
        return argument.clone();
    }
    let mut buffer = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut buffer);
    PyStr::decode_surrogateescape(&buffer)
}

fn usage() -> String {
    "usage: senpai html-review [-h] [--review [JSON]] [--review-doc [JSON]]\n".into()
}

/// `senpai html-review --review [JSON] | --review-doc [JSON]`.
pub fn main(argv: &[PyStr]) -> i32 {
    let mut review: Option<PyStr> = None;
    let mut review_doc: Option<PyStr> = None;
    let mut index = 0;
    while index < argv.len() {
        let token = argv[index].display();
        index += 1;
        let (flag, inline) = match token.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(PyStr::from(v))),
            _ => (token.clone(), None),
        };
        let slot = match flag.as_str() {
            "-h" | "--help" => {
                print!("{}\nReview one task-review request in a local browser page and print one result line.\n", usage());
                return crate::EXIT_OK;
            }
            "--review" => &mut review,
            "--review-doc" => &mut review_doc,
            _ => {
                eprintln!("{}{PRODUCER}: error: unrecognized arguments: {token}", usage());
                return crate::EXIT_ERROR;
            }
        };
        let value = match inline {
            Some(v) => v,
            None => match argv.get(index) {
                Some(next) if !next.starts_with("-") || next == "-" => {
                    index += 1;
                    next.clone()
                }
                _ => PyStr::new(),
            },
        };
        *slot = Some(value);
    }
    if review.is_none() && review_doc.is_none() {
        eprintln!("{}{PRODUCER}: a review request is required: --review JSON or --review-doc JSON", usage());
        return crate::EXIT_ERROR;
    }
    let lifetime = positive_seconds(LIFETIME_ENV, LIFETIME_SECONDS);
    let idle = positive_seconds(IDLE_ENV, IDLE_SECONDS);
    let browser = env_flag_enabled(NO_BROWSER_ENV);
    let prepared = (|| -> Result<Prepared> {
        if let Some(text) = &review_doc {
            let request = parse_document_request(&read_request_text(text))?;
            let payload = build_document_payload(&request)?;
            Ok(Prepared {
                title: request.criterion.clone(),
                places: vec![request.path.clone()],
                review_id: request.review_id.clone(),
                document: Some(request.path.clone()),
                payload,
                mode: "document",
            })
        } else {
            let request = parse_request(&read_request_text(review.as_ref().expect("one request")))?;
            let payload = build_payload(&SystemGit, &request)?;
            Ok(Prepared {
                title: request.criterion.clone(),
                places: request.repositories.iter().map(|r| r.root.clone()).collect(),
                review_id: request.review_id.clone(),
                document: None,
                payload,
                mode: "diff",
            })
        }
    })();
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            crate::diag::write(&format!("{PRODUCER}: {}\n", error.message()));
            return crate::EXIT_ERROR;
        }
    };
    let outcome = if crate::inbox::enabled() {
        crate::inbox::submit_review(&prepared, browser, lifetime, idle)
    } else {
        let session = Session::new(&prepared, os::token_urlsafe(24), duration(CLOSE_GRACE_SECONDS));
        serve_and_wait(Arc::new(session), lifetime, idle, browser)
    };
    match outcome {
        Ok(result) => {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "{}", json::dumps(&result.to_value(), json::COMPACT));
            let _ = stdout.flush();
            crate::EXIT_OK
        }
        Err(error) => {
            crate::diag::write(&format!("{PRODUCER}: {}\n", error.message()));
            crate::EXIT_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches_number_both_sides() {
        let patch = b"diff --git a/a.py b/a.py\nindex 1..2 100644\n--- a/a.py\n+++ b/a.py\n@@ -1,4 +1,4 @@ def handler():\n keep\n-gone\n+fresh\n \n\\ No newline at end of file\n";
        let (lines, added, removed, binary) = parse_patch(patch);
        assert_eq!((added, removed, binary), (1, 1, false));
        let shape: Vec<(&str, Option<u64>, Option<u64>)> = lines.iter().map(|l| (l.kind, l.old_line, l.new_line)).collect();
        assert_eq!(
            &shape[4..],
            &[
                ("meta", None, None),
                ("context", Some(1), Some(1)),
                ("remove", Some(2), None),
                ("add", None, Some(2)),
                ("context", Some(3), Some(3)),
                ("meta", None, None)
            ]
        );
        let (_, _, _, binary) = parse_patch(b"diff --git a/x b/x\nBinary files a/x and b/x differ\n");
        assert!(binary);
    }

    #[test]
    fn languages_follow_extensions_and_names() {
        assert_eq!(language_for(&PyStr::from("src/Main.RS")), Some("rust"));
        assert_eq!(language_for(&PyStr::from("Makefile")), Some("makefile"));
        assert_eq!(language_for(&PyStr::from(".bashrc")), None);
        assert_eq!(language_for(&PyStr::from("notes.txt")), None);
    }

    #[test]
    fn page_carries_boot_json_and_diff_logic() {
        let page = String::from_utf8(render_page("tok", "document", 3)).unwrap();
        assert!(page.contains(r#""url":"/tok/","page":3,"maxLines":1500,"mode":"document","reviewUrl":"api/doc""#));
        assert!(!page.contains("__HTML_DIFF_VIEW__"));
    }
}

#[cfg(test)]
mod target_tests {
    use super::{build_guide, parse_patch, query_value, split_target};
    use crate::json::{self, Value};

    #[test]
    fn guides_validate_like_the_recorded_python_client() {
        let cases = json::loads_str(include_str!("../tests/golden/guides.json")).expect("golden JSON");
        let cases = cases.as_array().expect("a list of cases");
        assert!(!cases.is_empty());
        for case in cases {
            let args = case.get("args").and_then(Value::as_array).expect("args");
            let function = case.get("function").and_then(Value::as_pystr).expect("function").display();
            let produced = match function.as_str() {
                "normalize_guide" => crate::request::normalize_guide(&args[0]),
                "build_guide" => {
                    let repositories: Vec<json::Object> = args[1]
                        .as_array()
                        .expect("repositories")
                        .iter()
                        .map(|r| r.as_object().expect("repository").clone())
                        .collect();
                    let guide = (!matches!(args[0], Value::Null)).then_some(&args[0]);
                    build_guide(guide, &repositories)
                }
                other => panic!("unknown function {other}"),
            };
            let label = json::dumps(&Value::Array(args.clone()), json::SORTED);
            match (case.get("output"), case.get("error"), produced) {
                (Some(expected), None, Ok(value)) => {
                    assert_eq!(json::dumps(expected, json::SORTED), json::dumps(&value, json::SORTED), "{function} {label}")
                }
                (None, Some(expected), Err(error)) => {
                    assert_eq!(expected.as_pystr().expect("message").display(), error.message(), "{function} {label}")
                }
                (_, _, produced) => panic!("{function} {label}: expected {case:?}, produced {produced:?}"),
            }
        }
    }

    #[test]
    fn patches_parse_like_the_recorded_python_reviewer() {
        let cases = json::loads_str(include_str!("../tests/golden/patches.json")).expect("golden JSON");
        let cases = cases.as_array().expect("a list of cases");
        assert!(!cases.is_empty());
        for case in cases {
            let hex = case.get("input_hex").and_then(Value::as_pystr).expect("input").display();
            let patch: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
                .collect();
            let (lines, added, removed, binary) = parse_patch(&patch);
            let produced = Value::Object(
                json::Object::new()
                    .with("lines", Value::Array(lines.iter().map(|line| line.payload()).collect()))
                    .with("added", Value::Int(added as i128))
                    .with("removed", Value::Int(removed as i128))
                    .with("binary", binary),
            );
            let expected = Value::Object(
                json::Object::new()
                    .with("lines", case.get("lines").expect("lines").clone())
                    .with("added", case.get("added").expect("added").clone())
                    .with("removed", case.get("removed").expect("removed").clone())
                    .with("binary", case.get("binary").expect("binary").clone()),
            );
            assert_eq!(json::dumps(&expected, json::SORTED), json::dumps(&produced, json::SORTED), "{hex}");
        }
    }

    #[test]
    fn targets_split_like_urlsplit() {
        let split = |t: &str| split_target(t);
        assert_eq!(split("/tok/api/closing?page=2#x"), ("/tok/api/closing".into(), "page=2".into()));
        assert_eq!(split("http://127.0.0.1:9/tok/"), ("/tok/".into(), "".into()));
        assert_eq!(split("//host/tok/?a=1"), ("/tok/".into(), "a=1".into()));
        assert_eq!(split("/tok/a:b"), ("/tok/a:b".into(), "".into()));
    }

    #[test]
    fn query_values_decode_like_parse_qs() {
        assert_eq!(query_value("page=%32", "page").as_deref(), Some("2"));
        assert_eq!(query_value("page=&page=3", "page").as_deref(), Some("3"));
        assert_eq!(query_value("pa%67e=4", "page").as_deref(), Some("4"));
        assert_eq!(query_value("page=1+2", "page").as_deref(), Some("1 2"));
        assert_eq!(query_value("page", "page"), None);
        assert_eq!(query_value("page=%zz", "page").as_deref(), Some("%zz"));
    }
}
