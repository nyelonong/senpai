//! Reads a pull request through the GitHub CLI. senpai only reads here: it
//! never fetches, comments on or approves anything on GitHub.

use std::process::{Command, Stdio};

use crate::error::{os_error, Error, Result};
use crate::json::{self, Object, Value};
use crate::os;
use crate::pystr::PyStr;

const FIELDS: &str = "number,url,title,body,baseRefName,baseRefOid,headRefName,headRefOid,commits";

#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    pub oid: String,
    pub headline: PyStr,
    pub author: PyStr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PullRequest {
    pub number: i128,
    pub url: PyStr,
    pub title: PyStr,
    pub body: PyStr,
    pub base_ref: PyStr,
    pub base_oid: String,
    pub head_ref: PyStr,
    pub head_oid: String,
    pub commits: Vec<Commit>,
}

impl PullRequest {
    pub fn payload(&self) -> Value {
        let commits = self
            .commits
            .iter()
            .map(|c| {
                Value::Object(
                    Object::new().with("oid", c.oid.as_str()).with("headline", &c.headline).with("author", &c.author),
                )
            })
            .collect::<Vec<_>>();
        Value::Object(
            Object::new()
                .with("number", Value::Int(self.number))
                .with("url", &self.url)
                .with("title", &self.title)
                .with("body", &self.body)
                .with("base_ref", &self.base_ref)
                .with("head_ref", &self.head_ref)
                .with("commits", commits),
        )
    }
}

/// Runs `gh pr view` in `root`. Any failure, including a missing or
/// logged-out `gh`, is an error, so no review can be approved without it.
pub fn view(root: &[u8], pr: &str) -> Result<PullRequest> {
    let output = Command::new("gh")
        .args(["pr", "view", pr, "--json", FIELDS])
        .current_dir(os::os_path(root))
        .env_remove("GH_REPO")
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::Other(format!("unable to run gh: {}", os_error(&e, Some(b"gh")))))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let code = output.status.code().unwrap_or(-1);
        let detail = if detail.is_empty() { format!("exit code {code}") } else { detail };
        return Err(Error::Other(format!("gh pr view {pr} failed: {detail}")));
    }
    let pull_request = parse(&output.stdout)?;
    if pr.bytes().all(|b| b.is_ascii_digit()) && pull_request.number.to_string() != pr {
        return Err(Error::Other(format!("gh returned pull request #{} for #{pr}", pull_request.number)));
    }
    Ok(pull_request)
}

fn is_object_id(text: &str) -> bool {
    (40..=64).contains(&text.len()) && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn parse(data: &[u8]) -> Result<PullRequest> {
    let invalid = |what: &str| Error::Other(format!("gh returned an invalid pull request: {what}"));
    let text = std::str::from_utf8(data).map_err(|_| invalid("not UTF-8"))?;
    let value = json::loads_str(text).map_err(|_| invalid("not JSON"))?;
    let object = value.as_object().ok_or_else(|| invalid("not an object"))?;
    let text = |key: &str| -> Result<PyStr> {
        object.get(key).and_then(Value::as_pystr).cloned().ok_or_else(|| invalid(key))
    };
    let oid = |key: &str| -> Result<String> {
        let value = text(key)?.display();
        if is_object_id(&value) { Ok(value) } else { Err(invalid(key)) }
    };
    let number = object.get("number").filter(|v| v.is_positive_integer()).and_then(Value::as_int).ok_or_else(|| invalid("number"))?;
    let mut commits = Vec::new();
    for item in object.get("commits").and_then(Value::as_array).ok_or_else(|| invalid("commits"))? {
        let commit = item.as_object().ok_or_else(|| invalid("commit"))?;
        let oid = commit.get("oid").and_then(Value::as_pystr).map(PyStr::display).filter(|o| is_object_id(o));
        let author = commit
            .get("authors")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|a| a.get("login").filter(|l| !matches!(l, Value::Str(s) if s.is_empty())).or_else(|| a.get("name")))
            .and_then(Value::as_pystr)
            .cloned()
            .unwrap_or_default();
        commits.push(Commit {
            oid: oid.ok_or_else(|| invalid("commit oid"))?,
            headline: commit.get("messageHeadline").and_then(Value::as_pystr).cloned().unwrap_or_default(),
            author,
        });
    }
    let url = text("url")?;
    if !url.starts_with("https://") {
        return Err(invalid("url"));
    }
    Ok(PullRequest {
        number,
        url,
        title: text("title")?,
        body: text("body")?,
        base_ref: text("baseRefName")?,
        base_oid: oid("baseRefOid")?,
        head_ref: text("headRefName")?,
        head_oid: oid("headRefOid")?,
        commits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(head: &str) -> String {
        format!(
            r#"{{"number":12,"url":"https://github.com/o/r/pull/12","title":"Add x","body":"Why **x**",
            "baseRefName":"main","baseRefOid":"{}","headRefName":"feature","headRefOid":"{head}",
            "commits":[{{"oid":"{}","messageHeadline":"first","authors":[{{"login":"ana","name":"Ana"}}]}},
                       {{"oid":"{}","messageHeadline":"second","authors":[{{"login":"","name":"Bo"}}]}}]}}"#,
            "a".repeat(40),
            "c".repeat(40),
            "d".repeat(40)
        )
    }

    #[test]
    fn parses_gh_pr_view_output() {
        let pr = parse(sample(&"b".repeat(40)).as_bytes()).unwrap();
        assert_eq!(pr.number, 12);
        assert_eq!((pr.base_ref.display(), pr.head_oid.clone()), ("main".to_string(), "b".repeat(40)));
        assert_eq!(pr.commits.len(), 2);
        assert_eq!(pr.commits[0].author, PyStr::from("ana"));
        assert_eq!(pr.commits[1].author, PyStr::from("Bo"));
    }

    #[test]
    fn rejects_malformed_output() {
        assert!(parse(b"not json").is_err());
        assert!(parse(sample(&"b".repeat(40)).replace("https://github.com/o/r/pull/12", "javascript:alert(1)").as_bytes()).is_err());
        assert!(parse(sample("HEAD").as_bytes()).is_err(), "a ref name is not a commit id");
        assert!(parse(sample(&"b".repeat(40)).replace("\"number\":12", "\"number\":0").as_bytes()).is_err());
        assert!(parse(sample(&"b".repeat(40)).replace("\"title\":\"Add x\",", "").as_bytes()).is_err());
    }
}
