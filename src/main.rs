//! senpai: human-in-the-loop review for coding agents.
//!
//! The client fails closed: any error, malformed reviewer output, mismatched
//! review id, or change to the reviewed state during the human review yields
//! no approval and a non-approval exit code.

mod cli;
mod commands;
mod diag;
mod error;
mod git;
mod github;
mod html;
mod inbox;
mod json;
mod ledger;
mod markdown;
mod os;
mod pystr;
mod recovery;
mod request;
mod skill;
mod result;
mod reviewer;
mod snapshot;
mod tui;

use std::process::ExitCode;

use crate::cli::{Args, Parsed};
use crate::error::{Error, Result};
use crate::git::{Git, SystemGit};
use crate::json::{Object, Value};
use crate::pystr::PyStr;
use crate::recovery::Recovery;
use crate::request::{normalize_paths, Repository, ReviewRequest};

pub const EXIT_OK: i32 = 0;
pub const EXIT_NOT_APPROVED: i32 = 1;
pub const EXIT_ERROR: i32 = 2;

fn main() -> ExitCode {
    let argv: Vec<PyStr> = std::env::args_os().skip(1).map(|a| PyStr::from_os(&a)).collect();
    if argv.len() == 1 && matches!(argv[0].display().as_str(), "--version" | "-V") {
        println!("senpai {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::from(EXIT_OK as u8);
    }
    let service_command: Option<fn(&[PyStr]) -> i32> = match argv.first().map(PyStr::display).as_deref() {
        Some("html-review") => Some(html::main),
        Some("serve") => Some(inbox::serve_main),
        Some("inbox") => Some(inbox::inbox_main),
        Some("ask") => Some(inbox::ask_main),
        Some("tui") => Some(tui::main),
        Some("skill") => Some(skill::main),
        _ => None,
    };
    if let Some(command) = service_command {
        return ExitCode::from(command(&argv[1..]) as u8);
    }
    let args = match cli::parse(&argv) {
        Parsed::Run(args) => args,
        Parsed::Help(text) => {
            print!("{text}");
            return ExitCode::from(EXIT_OK as u8);
        }
        Parsed::Error(text) => {
            eprint!("{text}");
            return ExitCode::from(EXIT_ERROR as u8);
        }
    };
    let code = match dispatch(&args, &SystemGit, &mut commands::ProcessLauncher) {
        Ok(code) => code,
        Err(Error::Interrupted) => {
            diag::write("task_review: the review was interrupted; no approval was returned\n");
            EXIT_ERROR
        }
        Err(error) => {
            diag::write(&format!("task_review: {}\n", error.message()));
            EXIT_ERROR
        }
    };
    ExitCode::from(code as u8)
}

fn default_root() -> Result<PyStr> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| Error::Other(format!("unable to run git: {}", error::os_error(&e, Some(b"git")))))?;
    if !output.status.success() {
        return Err(Error::Usage("not inside a Git worktree: pass --repo ROOT".into()));
    }
    Ok(PyStr::from(pystr::py_strip(&String::from_utf8_lossy(&output.stdout)).to_string()))
}

fn repositories_from_options(args: &Args) -> Result<Vec<Repository>> {
    let roots: Vec<PyStr> = if args.list("--repo").is_empty() { vec![default_root()?] } else { args.list("--repo").to_vec() };
    let raw_paths = args.value("--paths");
    if roots.len() > 1 && raw_paths.is_some() {
        return Err(Error::Usage("--paths is only allowed with exactly one --repo".into()));
    }
    let revision = |name: &str| -> Result<Option<String>> {
        match args.value(name) {
            None => Ok(None),
            Some(raw) => request::normalize_revision(Some(&Value::Str(raw.clone())), &name[2..]).map_err(|e| Error::Usage(e.message())),
        }
    };
    let (base, head) = (revision("--base")?, revision("--head")?);
    let pr = match args.value("--pr") {
        None => None,
        Some(raw) => request::normalize_pr(Some(&Value::Str(raw.clone()))).map_err(|e| Error::Usage(e.message()))?,
    };
    if roots.len() > 1 && (base.is_some() || pr.is_some()) {
        return Err(Error::Usage("--base, --head and --pr are only allowed with exactly one --repo".into()));
    }
    if head.is_some() && base.is_none() {
        return Err(Error::Usage("--head needs --base".into()));
    }
    if pr.is_some() && base.is_some() {
        return Err(Error::Usage("--pr cannot be combined with --base or --head".into()));
    }
    let paths = match raw_paths {
        Some(raw) if !raw.is_empty() => {
            normalize_paths(Some(&Value::Array(raw.split_byte(b',').into_iter().map(Value::Str).collect())))?
        }
        _ => None,
    };
    request::scoped_repositories(&roots, &paths, &request::Revisions { base, head, pr })
}

fn require_no_json_with_flags(args: &Args, names: &[&str]) -> Result<()> {
    let used: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| match *n {
            "--repo" => !args.list(n).is_empty(),
            _ => args.value(n).is_some_and(|v| !v.is_empty()),
        })
        .collect();
    if args.value("--json").is_some() && !used.is_empty() {
        return Err(Error::Usage(format!("--json cannot be combined with {}", used.join(" or "))));
    }
    Ok(())
}

fn recovery_dir(args: &Args) -> Option<&PyStr> {
    args.value("--recovery-dir").filter(|v| !v.is_empty())
}

/// Runs a review command with optional recovery artifacts, recording any
/// failure in them before it is reported.
fn with_recovery(
    recovery: Option<Recovery>,
    run: impl FnOnce(Option<&mut Recovery>) -> Result<i32>,
) -> Result<i32> {
    let Some(mut recovery) = recovery else {
        return run(None);
    };
    recovery.open_log()?;
    let outcome = run(Some(&mut recovery));
    recovery.close(outcome.as_ref().err());
    outcome
}

pub fn dispatch(args: &Args, git: &dyn Git, launcher: &mut dyn commands::Launcher) -> Result<i32> {
    let command = args.command.as_str();
    if matches!(command, "review-doc" | "verify-doc" | "consume-doc") {
        return dispatch_document(args, git, launcher);
    }
    if matches!(command, "review" | "preflight") {
        require_no_json_with_flags(args, &["--criterion", "--repo", "--paths", "--base", "--head", "--pr", "--guide"])?;
        let request: ReviewRequest = if let Some(json) = args.value("--json") {
            request::parse_request_json(json, true)?
        } else {
            let criterion = match args.value("--criterion") {
                Some(c) if !c.is_empty() => c.clone(),
                _ => return Err(Error::Usage("review needs --criterion TEXT or --json REQUEST".into())),
            };
            if args.list("--repo").is_empty() {
                return Err(Error::Usage("review needs --repo ROOT".into()));
            }
            let mut request = request::build_request(&criterion, &repositories_from_options(args)?)?;
            if let Some(guide) = args.value("--guide") {
                request.guide = Some(request::read_guide(&request::encode_path(guide)?)?);
            }
            request
        };
        let launcher_name = reviewer::resolve_reviewer();
        if command == "preflight" {
            let snapshot = commands::review_snapshot(git, &request, &launcher_name)?;
            commands::emit(&Value::Object(
                Object::new()
                    .with("valid", true)
                    .with("fingerprint", snapshot.fingerprint.as_str())
                    .with("repositories", Value::Array(request.repositories.iter().map(Repository::payload).collect())),
            ))?;
            return Ok(EXIT_OK);
        }
        let recovery = match recovery_dir(args) {
            Some(dir) => {
                let roots: Vec<Vec<u8>> = request.repositories.iter().map(Repository::root_bytes).collect::<Result<_>>()?;
                Some(Recovery::create(dir, &roots, &request.review_id, &request.payload())?)
            }
            None => None,
        };
        return with_recovery(recovery, |recovery| {
            let timeout = reviewer::resolve_timeout()?;
            commands::command_review(git, &request, &launcher_name, launcher, timeout, recovery)
        });
    }
    let repositories = if let Some(json) = args.value("--json") {
        require_no_json_with_flags(args, &["--repo", "--paths", "--base", "--head", "--pr"])?;
        request::parse_request_json(json, false)?.repositories
    } else {
        repositories_from_options(args)?
    };
    match command {
        "verify" => commands::command_verify(git, &repositories),
        "status" => commands::command_status(git, &repositories),
        "consume" => commands::command_close(git, "consumed", &repositories),
        _ => commands::command_close(git, "superseded", &repositories),
    }
}

fn dispatch_document(args: &Args, git: &dyn Git, launcher: &mut dyn commands::Launcher) -> Result<i32> {
    let empty = PyStr::new();
    let path = commands::normalize_document(args.value("--doc").unwrap_or(&empty))?;
    let criterion = args.value("--criterion").cloned().unwrap_or_default();
    if args.command == "review-doc" {
        let request = commands::build_document_request(&criterion, &path)?;
        let recovery = match recovery_dir(args) {
            Some(dir) => {
                let parent = os::dirname(&request.path_bytes()?);
                Some(Recovery::create(dir, &[parent], &request.review_id, &request.payload())?)
            }
            None => None,
        };
        let launcher_name = reviewer::resolve_reviewer();
        return with_recovery(recovery, |recovery| {
            let timeout = reviewer::resolve_timeout()?;
            commands::command_review_doc(git, &request, &launcher_name, launcher, timeout, recovery)
        });
    }
    // verify and consume need the criterion the approval was recorded with,
    // because it is part of the fingerprint; take it from the ledger when the
    // caller did not repeat it.
    let criterion = if criterion.is_empty() {
        let recorded = ledger::newest_entry(&ledger::document_ledger_path(git, &request::encode_path(&path)?)?)?;
        match recorded.as_ref().and_then(|r| r.get("criterion")) {
            None | Some(Value::Null) | Some(Value::Bool(false)) | Some(Value::Int(0)) => PyStr::from("document"),
            Some(Value::Str(s)) if s.is_empty() => PyStr::from("document"),
            Some(Value::Str(s)) => s.clone(),
            Some(Value::Array(a)) if a.is_empty() => PyStr::from("document"),
            Some(Value::Object(o)) if o.is_empty() => PyStr::from("document"),
            Some(Value::Float(f)) if *f == 0.0 => PyStr::from("document"),
            Some(_) => return Err(Error::Usage("a document review needs --criterion TEXT".into())),
        }
    } else {
        criterion
    };
    let request = commands::build_document_request(&criterion, &path)?;
    if args.command == "verify-doc" {
        return commands::command_verify_doc(git, &request);
    }
    commands::command_close_doc(git, &request, "consumed")
}
