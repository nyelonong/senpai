//! Validation of the single JSON result a reviewer prints. Anything the client
//! cannot fully understand is an error, never an approval.

use crate::error::{Error, Result};
use crate::json::{self, Object, Value};
use crate::ledger::DECISIONS;
use crate::pystr::PyStr;
use crate::request::{value_repr, PROTOCOL_VERSION};

fn line_range_ok(target: &Object) -> bool {
    let (Some(start), Some(end)) = (target.get("start_line"), target.get("end_line")) else {
        return false;
    };
    if !start.is_positive_integer() || !end.is_positive_integer() {
        return false;
    }
    match (start, end) {
        (Value::Int(s), Value::Int(e)) => e >= s,
        (Value::Int(_), Value::BigInt(_)) => true,
        (Value::BigInt(_), Value::Int(_)) => false,
        (Value::BigInt(s), Value::BigInt(e)) => (e.len(), e) >= (s.len(), s),
        _ => false,
    }
}

fn text_and_target(value: &Value) -> Option<&Object> {
    let object = value.as_object()?;
    if !matches!(object.get("text"), Some(Value::Str(_))) {
        return None;
    }
    object.get("target")?.as_object()
}

/// A diff comment: review-scoped, or anchored to lines on one side of a file.
pub fn is_annotation(value: &Value) -> bool {
    let Some(target) = text_and_target(value) else {
        return false;
    };
    match target.get("scope") {
        Some(v) if v.is_str("review") => true,
        Some(v) if v.is_str("line") => {
            matches!(target.get("repository"), Some(Value::Str(_)))
                && matches!(target.get("path"), Some(Value::Str(_)))
                && matches!(target.get("side"), Some(v) if v.is_str("old") || v.is_str("new"))
                && line_range_ok(target)
        }
        _ => false,
    }
}

/// A document comment: review-scoped, or anchored to real lines of the one
/// file that was reviewed. A document has no repository and no old side.
pub fn is_document_annotation(value: &Value, path: &PyStr) -> bool {
    let Some(target) = text_and_target(value) else {
        return false;
    };
    match target.get("scope") {
        Some(v) if v.is_str("review") => true,
        Some(v) if v.is_str("line") => {
            let absent = |key| matches!(target.get(key), None | Some(Value::Null));
            absent("repository")
                && absent("side")
                && matches!(target.get("path"), Some(Value::Str(p)) if p == path)
                && line_range_ok(target)
        }
        _ => false,
    }
}

pub struct ReviewResult {
    pub decision: String,
    pub annotations: Vec<Value>,
    pub review_id: PyStr,
}

impl ReviewResult {
    pub fn to_value(&self) -> Value {
        Value::Object(
            Object::new()
                .with("version", PROTOCOL_VERSION)
                .with("review_id", &self.review_id)
                .with("decision", self.decision.as_str())
                .with("annotations", Value::Array(self.annotations.clone())),
        )
    }
}

pub fn parse_review_result(stdout: &[u8], review_id: &PyStr, check: &dyn Fn(&Value) -> bool) -> Result<ReviewResult> {
    let fail = |m: String| Error::Result(m);
    let text = std::str::from_utf8(stdout).map_err(|_| fail("the reviewer returned a non-UTF-8 result".into()))?;
    let payload = json::loads_str(text).map_err(|_| fail("the reviewer returned a non-JSON result".into()))?;
    let Some(object) = payload.as_object() else {
        return Err(fail("the reviewer returned an invalid result".into()));
    };
    let version = object.get("version");
    if !matches!(version, Some(v) if v.is_str(PROTOCOL_VERSION)) {
        return Err(fail(format!("the reviewer returned an unsupported protocol version: {}", value_repr(version))));
    }
    if !matches!(object.get("review_id"), Some(Value::Str(id)) if id == review_id) {
        return Err(fail("the reviewer returned a mismatched review id".into()));
    }
    let decision = object.get("decision");
    let decision = match decision {
        Some(Value::Str(d)) if DECISIONS.iter().any(|k| d == k) => d.as_str().expect("ascii").to_string(),
        _ => return Err(fail(format!("the reviewer returned an unknown decision: {}", value_repr(decision)))),
    };
    let annotations = match object.get("annotations") {
        Some(Value::Array(items)) if items.iter().all(check) => items.clone(),
        _ => return Err(fail("the reviewer returned malformed annotations".into())),
    };
    Ok(ReviewResult { decision, annotations, review_id: review_id.clone() })
}

pub fn validate_decision_semantics(result: &ReviewResult) -> Result<()> {
    if result.decision == "request-changes"
        && !result
            .annotations
            .iter()
            .any(|a| matches!(a.get("text"), Some(Value::Str(t)) if !t.is_blank()))
    {
        return Err(Error::Result("the reviewer requested changes without feedback".into()));
    }
    if result.decision == "cancelled" && !result.annotations.is_empty() {
        return Err(Error::Result("the reviewer returned annotations for a cancelled review".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(text: &str) -> Value {
        json::loads_str(text).unwrap()
    }

    #[test]
    fn diff_annotations_need_a_side_and_an_ordered_range() {
        assert!(is_annotation(&value(r#"{"text":"x","target":{"scope":"review"}}"#)));
        let line = r#"{"text":"x","target":{"scope":"line","repository":"/r","path":"a","side":"new","start_line":2,"end_line":3}}"#;
        assert!(is_annotation(&value(line)));
        assert!(!is_annotation(&value(&line.replace("\"end_line\":3", "\"end_line\":1"))));
        assert!(!is_annotation(&value(&line.replace("\"start_line\":2", "\"start_line\":true"))));
        assert!(!is_annotation(&value(&line.replace("new", "both"))));
    }

    #[test]
    fn document_annotations_name_only_the_reviewed_file() {
        let path = PyStr::from("/doc.md");
        let ok = value(r#"{"text":"x","target":{"scope":"line","path":"/doc.md","start_line":1,"end_line":1}}"#);
        assert!(is_document_annotation(&ok, &path));
        let other = value(r#"{"text":"x","target":{"scope":"line","path":"/other.md","start_line":1,"end_line":1}}"#);
        assert!(!is_document_annotation(&other, &path));
        let sided = value(r#"{"text":"x","target":{"scope":"line","path":"/doc.md","side":"new","start_line":1,"end_line":1}}"#);
        assert!(!is_document_annotation(&sided, &path));
    }

    #[test]
    fn results_fail_closed() {
        let id = PyStr::from("r");
        let check: &dyn Fn(&Value) -> bool = &is_annotation;
        for (bad, message) in [
            (&b"\xff"[..], "non-UTF-8"),
            (b"nope", "non-JSON"),
            (b"[]", "invalid result"),
            (br#"{"version":"2"}"#, "unsupported protocol version: '2'"),
            (br#"{"version":"1","review_id":"x"}"#, "mismatched review id"),
            (br#"{"version":"1","review_id":"r","decision":"maybe"}"#, "unknown decision: 'maybe'"),
            (br#"{"version":"1","review_id":"r","decision":"approve","annotations":[1]}"#, "malformed annotations"),
        ] {
            let error = parse_review_result(bad, &id, check).err().expect("must fail");
            assert!(error.message().contains(message), "{}", error.message());
        }
        let ok = parse_review_result(br#"{"version":"1","review_id":"r","decision":"cancelled","annotations":[]}"#, &id, check).unwrap();
        assert!(validate_decision_semantics(&ok).is_ok());
    }
}
