//! The client commands: review, preflight, verify, status, consume, revoke and
//! their document counterparts.

use std::io::Write;

use crate::error::{Error, Result};
use crate::git::Git;
use crate::json::{self, Object, Value};
use crate::ledger::{self, approval_state, carried, newest_entry, Entry, APPROVING_DECISION};
use crate::os;
use crate::pystr::PyStr;
use crate::recovery::Recovery;
use crate::request::{DocumentRequest, Repository, ReviewRequest, PROTOCOL_VERSION};
use crate::result::{self, ReviewResult};
use crate::reviewer::{self, Launch, HTML_REVIEWER};
use crate::snapshot::{collect_snapshot, Snapshot};
use crate::{EXIT_NOT_APPROVED, EXIT_OK};

pub fn emit(payload: &Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{}", json::dumps(payload, json::SORTED));
    let _ = stdout.flush();
}

/// Runs a reviewer for one request: the configured process, or a test double.
pub trait Launcher {
    fn launch(&mut self, launcher: &str, flag: &str, request_json: &str, timeout: Option<u64>, recovery: Option<&mut Recovery>) -> Result<Vec<u8>>;
}

pub struct ProcessLauncher;

impl Launcher for ProcessLauncher {
    fn launch(&mut self, launcher: &str, flag: &str, request_json: &str, timeout: Option<u64>, recovery: Option<&mut Recovery>) -> Result<Vec<u8>> {
        let timed_out = if flag == "--review-doc" {
            "the document review timed out before a human decided"
        } else {
            "the review timed out before a human decided"
        };
        let url_file = recovery.as_ref().map(|r| r.url_file());
        let launch = Launch { launcher, flag, request_json, timeout, timed_out, url_file };
        match recovery {
            Some(recovery) => reviewer::run_reviewer(&launch, |pid, argv| recovery.reviewer_started(pid, argv)),
            None => reviewer::run_reviewer(&launch, |_, _| Ok(())),
        }
    }
}

pub fn review_snapshot(git: &dyn Git, request: &ReviewRequest, launcher: &str) -> Result<Snapshot> {
    let snapshot = collect_snapshot(git, &request.repositories)?;
    if let Some(guide) = &request.guide {
        if launcher != HTML_REVIEWER {
            return Err(Error::Usage("guided reviews require TASK_REVIEW_REVIEWER=html".into()));
        }
        validate_guide_snapshot(guide, &snapshot)?;
    }
    Ok(snapshot)
}

pub fn validate_guide_snapshot(guide: &Value, snapshot: &Snapshot) -> Result<()> {
    if !matches!(guide.get("fingerprint"), Some(v) if v.is_str(&snapshot.fingerprint)) {
        return Err(Error::Result(
            "guide is stale or has a different scope; regenerate it for the current scoped diff".into(),
        ));
    }
    let available: Vec<(&PyStr, &PyStr)> = snapshot
        .repositories
        .iter()
        .flat_map(|repo| repo.files.iter().map(move |file| (&repo.root, &file.path)))
        .collect();
    for chapter in guide.get("chapters").and_then(Value::as_array).into_iter().flatten() {
        for reference in chapter.get("files").and_then(Value::as_array).into_iter().flatten() {
            let repository = reference.get("repository").and_then(Value::as_pystr);
            let path = reference.get("path").and_then(Value::as_pystr);
            let found = matches!((repository, path), (Some(r), Some(p)) if available.contains(&(r, p)));
            if !found {
                return Err(Error::Result(format!(
                    "guide references a file outside the captured diff: {}",
                    path.map(PyStr::display).unwrap_or_default()
                )));
            }
        }
    }
    Ok(())
}

fn interrupted_check() -> Result<()> {
    if reviewer::interrupted() {
        return Err(Error::Interrupted);
    }
    Ok(())
}

pub fn command_review(
    git: &dyn Git,
    request: &ReviewRequest,
    launcher_name: &str,
    launcher: &mut dyn Launcher,
    timeout: Option<u64>,
    mut recovery: Option<&mut Recovery>,
) -> Result<i32> {
    let paths = ledger::ledger_paths(git, &request.repositories)?;
    // Revoke any prior approval first: nothing counts as approved until this
    // review completes and the reviewed state is verified again.
    let previous = newest_entry(&paths[0])?;
    ledger::append_ledger(
        &paths,
        &Entry::new(
            "superseded",
            carried(previous.as_ref(), "decision", Value::str("superseded")),
            carried(previous.as_ref(), "fingerprint", Value::str("")),
            Value::from(&request.criterion),
            &request.repositories,
        ),
    )?;
    let outcome = (|| -> Result<i32> {
        let snapshot = review_snapshot(git, request, launcher_name)?;
        interrupted_check()?;
        if let Some(recovery) = recovery.as_deref() {
            recovery.state("reviewing", Object::new().with("fingerprint", snapshot.fingerprint.as_str()))?;
        }
        let stdout = launcher.launch(launcher_name, "--review", &request.to_json(), timeout, recovery.as_deref_mut())?;
        let result = result::parse_review_result(&stdout, &request.review_id, &result::is_annotation)?;
        result::validate_decision_semantics(&result)?;
        if result.decision == APPROVING_DECISION {
            let recaptured = collect_snapshot(git, &request.repositories)?;
            if recaptured.fingerprint != snapshot.fingerprint {
                return Err(Error::Result(
                    "the reviewed repository state changed while the human was deciding; \
                     run the review again for the current state"
                        .into(),
                ));
            }
        }
        let mut entry = Entry::new(
            ledger::decision_state(&result.decision),
            Value::from(result.decision.as_str()),
            Value::from(snapshot.fingerprint.as_str()),
            Value::from(&request.criterion),
            &request.repositories,
        );
        entry.annotations = Value::Array(result.annotations.clone());
        ledger::append_ledger(&paths, &entry)?;
        let code = if result.decision == APPROVING_DECISION { EXIT_OK } else { EXIT_NOT_APPROVED };
        if let Some(recovery) = recovery.as_deref() {
            recovery.finish(&result, code)?;
        }
        emit(&result.to_value());
        Ok(code)
    })();
    if let Err(error) = &outcome {
        let mut entry = Entry::new("error", Value::str("error"), Value::str(""), Value::from(&request.criterion), &request.repositories);
        entry.error = Some(error.message());
        ledger::append_ledger(&paths, &entry)?;
    }
    outcome
}

pub fn command_verify(git: &dyn Git, repositories: &[Repository]) -> Result<i32> {
    let paths = ledger::ledger_paths(git, repositories)?;
    let snapshot = collect_snapshot(git, repositories)?;
    let mut entry: Option<Object> = None;
    for path in &paths {
        entry = newest_entry(path)?;
        let (approved, reason) = approval_state(entry.as_ref(), &snapshot.fingerprint);
        if !approved {
            emit(&Value::Object(
                Object::new()
                    .with("approved", false)
                    .with("fingerprint", snapshot.fingerprint.as_str())
                    .with("state", state_of(entry.as_ref()))
                    .with("reason", reason),
            ));
            return Ok(EXIT_NOT_APPROVED);
        }
    }
    emit(&Value::Object(
        Object::new()
            .with("approved", true)
            .with("fingerprint", snapshot.fingerprint.as_str())
            .with("state", state_of(entry.as_ref()))
            .with("reason", "the approval matches the current state"),
    ));
    Ok(EXIT_OK)
}

fn state_of(entry: Option<&Object>) -> Value {
    entry.and_then(|e| e.get("state")).cloned().unwrap_or(Value::Null)
}

fn field_of(entry: Option<&Object>, key: &str) -> Value {
    entry.and_then(|e| e.get(key)).cloned().unwrap_or(Value::Null)
}

pub fn command_status(git: &dyn Git, repositories: &[Repository]) -> Result<i32> {
    let paths = ledger::ledger_paths(git, repositories)?;
    let snapshot = collect_snapshot(git, repositories)?;
    let mut reports = Vec::new();
    let mut approved = true;
    let mut first: Option<Option<Object>> = None;
    let mut first_failure: Option<String> = None;
    for (repository, path) in repositories.iter().zip(&paths) {
        let entry = newest_entry(path)?;
        let (matches, reason) = approval_state(entry.as_ref(), &snapshot.fingerprint);
        approved = approved && matches;
        if !matches && first_failure.is_none() {
            first_failure = Some(reason.clone());
        }
        reports.push(Value::Object(
            Object::new()
                .with("root", &repository.root)
                .with("ledger", PyStr::decode_surrogateescape(path))
                .with("entry", entry.clone().map_or(Value::Null, Value::Object))
                .with("state", state_of(entry.as_ref()))
                .with("matches", matches)
                .with("reason", reason),
        ));
        if first.is_none() {
            first = Some(entry);
        }
    }
    let first = first.flatten();
    let reason = if approved {
        "the approval matches the current state".to_string()
    } else {
        first_failure.unwrap_or_default()
    };
    emit(&Value::Object(
        Object::new()
            .with("approved", approved)
            .with("fingerprint", snapshot.fingerprint.as_str())
            .with("recordedFingerprint", field_of(first.as_ref(), "fingerprint"))
            .with("state", state_of(first.as_ref()))
            .with("reason", reason)
            .with("entry", first.clone().map_or(Value::Null, Value::Object))
            .with("repositories", Value::Array(reports)),
    ));
    Ok(if approved { EXIT_OK } else { EXIT_NOT_APPROVED })
}

/// Records a terminal non-approval state (`consumed` or `superseded`).
pub fn command_close(git: &dyn Git, state: &str, repositories: &[Repository]) -> Result<i32> {
    let paths = ledger::ledger_paths(git, repositories)?;
    let snapshot = collect_snapshot(git, repositories)?;
    let previous = newest_entry(&paths[0])?;
    let mut entry = Entry::new(
        state,
        carried(previous.as_ref(), "decision", Value::str(state)),
        carried(previous.as_ref(), "fingerprint", Value::from(snapshot.fingerprint.as_str())),
        carried(previous.as_ref(), "criterion", Value::str("")),
        repositories,
    );
    entry.annotations = carried(previous.as_ref(), "annotations", Value::Array(Vec::new()));
    ledger::append_ledger(&paths, &entry)?;
    emit(&Value::Object(
        Object::new()
            .with("approved", false)
            .with("fingerprint", snapshot.fingerprint.as_str())
            .with("state", state)
            .with("reason", format!("recorded {state} in the approval ledger")),
    ));
    Ok(EXIT_OK)
}

// ---------------------------------------------------------------------------
// documents
// ---------------------------------------------------------------------------

/// A document is a real file on this machine, not a scope in a repository.
pub fn normalize_document(value: &PyStr) -> Result<PyStr> {
    if value.is_blank() {
        return Err(Error::Document("a document review needs a document path".into()));
    }
    let raw = crate::request::encode_path(value)?;
    let path = os::abspath(&os::expanduser(&raw));
    if !std::fs::metadata(os::os_path(&path)).map(|m| m.is_file()).unwrap_or(false) {
        return Err(Error::Document(format!("the document does not exist: {}", value.display())));
    }
    Ok(PyStr::decode_surrogateescape(&path))
}

pub fn build_document_request(criterion: &PyStr, path: &PyStr) -> Result<DocumentRequest> {
    if criterion.is_blank() {
        return Err(Error::Usage("a document review needs --criterion TEXT".into()));
    }
    Ok(DocumentRequest {
        version: PROTOCOL_VERSION.into(),
        review_id: PyStr::from(os::uuid4_hex()),
        criterion: criterion.clone(),
        path: path.clone(),
    })
}

fn document_entry(state: &str, decision: Value, fingerprint: Value, request: &DocumentRequest) -> Entry {
    let mut entry = Entry::new(state, decision, fingerprint, Value::from(&request.criterion), &[]);
    entry.document = Some(request.path.clone());
    entry
}

pub fn command_review_doc(
    git: &dyn Git,
    request: &DocumentRequest,
    launcher_name: &str,
    launcher: &mut dyn Launcher,
    timeout: Option<u64>,
    mut recovery: Option<&mut Recovery>,
) -> Result<i32> {
    let path = request.path_bytes()?;
    let ledger_file = vec![ledger::document_ledger_path(git, &path)?];
    let previous = newest_entry(&ledger_file[0])?;
    ledger::append_ledger(
        &ledger_file,
        &document_entry(
            "superseded",
            carried(previous.as_ref(), "decision", Value::str("superseded")),
            carried(previous.as_ref(), "fingerprint", Value::str("")),
            request,
        ),
    )?;
    let outcome = (|| -> Result<i32> {
        let before = ledger::document_fingerprint(&path, &request.criterion)?;
        interrupted_check()?;
        if let Some(recovery) = recovery.as_deref() {
            recovery.state("reviewing", Object::new().with("fingerprint", before.as_str()))?;
        }
        let stdout = launcher.launch(launcher_name, "--review-doc", &request.to_json(), timeout, recovery.as_deref_mut())?;
        let check = |item: &Value| result::is_document_annotation(item, &request.path);
        let result: ReviewResult = result::parse_review_result(&stdout, &request.review_id, &check)?;
        if !result.annotations.iter().all(check) {
            return Err(Error::Result("a document comment named a line the document does not have".into()));
        }
        result::validate_decision_semantics(&result)?;
        if result.decision == APPROVING_DECISION && ledger::document_fingerprint(&path, &request.criterion)? != before {
            return Err(Error::Result(
                "the document changed while the human was deciding; run the review again for the current text".into(),
            ));
        }
        let mut entry = document_entry(
            ledger::decision_state(&result.decision),
            Value::from(result.decision.as_str()),
            Value::from(before.as_str()),
            request,
        );
        entry.annotations = Value::Array(result.annotations.clone());
        ledger::append_ledger(&ledger_file, &entry)?;
        let code = if result.decision == APPROVING_DECISION { EXIT_OK } else { EXIT_NOT_APPROVED };
        if let Some(recovery) = recovery.as_deref() {
            recovery.finish(&result, code)?;
        }
        emit(&result.to_value());
        Ok(code)
    })();
    if let Err(error) = &outcome {
        let mut entry = document_entry("error", Value::str("error"), Value::str(""), request);
        entry.error = Some(error.message());
        ledger::append_ledger(&ledger_file, &entry)?;
    }
    outcome
}

pub fn command_verify_doc(git: &dyn Git, request: &DocumentRequest) -> Result<i32> {
    let path = request.path_bytes()?;
    let fingerprint = ledger::document_fingerprint(&path, &request.criterion)?;
    let entry = newest_entry(&ledger::document_ledger_path(git, &path)?)?;
    let (approved, reason) = approval_state(entry.as_ref(), &fingerprint);
    emit(&Value::Object(
        Object::new()
            .with("approved", approved)
            .with("fingerprint", fingerprint.as_str())
            .with("recordedFingerprint", field_of(entry.as_ref(), "fingerprint"))
            .with("state", state_of(entry.as_ref()))
            .with("document", &request.path)
            .with("reason", reason),
    ));
    Ok(if approved { EXIT_OK } else { EXIT_NOT_APPROVED })
}

/// Retires a document approval once the next phase has used it.
pub fn command_close_doc(git: &dyn Git, request: &DocumentRequest, state: &str) -> Result<i32> {
    let path = request.path_bytes()?;
    let ledger_file = vec![ledger::document_ledger_path(git, &path)?];
    let entry = newest_entry(&ledger_file[0])?;
    let fingerprint = ledger::document_fingerprint(&path, &request.criterion)?;
    ledger::append_ledger(
        &ledger_file,
        &document_entry(
            state,
            carried(entry.as_ref(), "decision", Value::str(APPROVING_DECISION)),
            Value::from(fingerprint.as_str()),
            request,
        ),
    )?;
    Ok(EXIT_OK)
}
