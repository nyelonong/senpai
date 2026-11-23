//! The fakes the tests put in place of real programs, played by this same test
//! binary. `install` writes a small wrapper script that runs this binary with
//! `--senpai-fake=ROLE` first; `run` acts it.
//!
//! - `reviewer`: the stub reviewer. It reads one request argument and prints a
//!   result chosen by the test through `STUB_*` variables (see `reviewer`).
//! - `rex`: records its arguments to `FAKE_RECORD`; `split --json` creates
//!   `block:page`, which `block ls` lists while `FAKE_PANE_STAYS` is `1`.
//! - `tern`: records each `tern browser` op as a JSON line to `FAKE_RECORD` and
//!   answers like Tern; `FAKE_MODE` is `ok`, `error` or `phantom` (opens a
//!   block that `blocks` does not list).

#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::support::{script, shell_quote, Value};

pub const ROLE_FLAG: &str = "--senpai-fake=";

/// Writes `dir/name`, a script that plays `role` with `env` set and keeps
/// the caller's `argv[0]`.
pub fn install(dir: &Path, name: &str, role: &str, env: &[(&str, String)]) -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary");
    let mut body = String::from("#!/bin/bash\n");
    for (key, value) in env {
        body.push_str(&format!("export {key}={}\n", shell_quote(value)));
    }
    // `exec -a "$0"` keeps the name the caller used, which some tests check.
    body.push_str(&format!(
        "exec -a \"$0\" {} {} \"$@\"\n",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&format!("{ROLE_FLAG}{role}"))
    ));
    script(&dir.join(name), &body)
}

/// Acts `role`; `argv` is the program name followed by the caller's arguments.
pub fn run(role: &str, argv: &[String]) -> i32 {
    match role {
        "reviewer" => reviewer(argv),
        "rex" => rex(&argv[1..]),
        "tern" => tern(&argv[1..]),
        other => {
            eprintln!("unknown fake role {other}");
            2
        }
    }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The stub reviewer:
/// `STUB_REQUEST` records argv and the request; `STUB_BLOCK` makes a
/// directory there (so a rename onto it fails); `STUB_SLEEP` waits;
/// `STUB_RESULT` is printed with `__REVIEW_ID__` replaced unless
/// `STUB_SUBSTITUTE=0`; `STUB_MODE` is `result`, `mutate` (append a line to
/// `STUB_MUTATE` first), `git` (run git with the tab-separated arguments in
/// `STUB_GIT` first), `garbage` (print non-JSON) or `fail` (exit 1).
fn reviewer(argv: &[String]) -> i32 {
    if !matches!(argv.get(1).map(String::as_str), Some("--review" | "--review-doc")) {
        eprintln!("the client must pass --review or --review-doc");
        return 1;
    }
    let request: Value = match argv.get(2).map(|a| serde_json::from_str(a)) {
        Some(Ok(value)) => value,
        _ => {
            eprintln!("the request argument is not JSON");
            return 1;
        }
    };
    if let Some(path) = var("STUB_REQUEST") {
        let record = serde_json::json!({"argv": argv, "request": request});
        std::fs::write(path, serde_json::to_string(&record).expect("encode")).expect("record the request");
    }
    if let Some(path) = var("STUB_BLOCK") {
        let path = PathBuf::from(path);
        if path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
        let _ = std::fs::create_dir(&path);
    }
    if let Some(seconds) = var("STUB_SLEEP").and_then(|s| s.parse::<f64>().ok()) {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    }
    let mut template = var("STUB_RESULT").unwrap_or_default();
    if var("STUB_SUBSTITUTE").as_deref().unwrap_or("1") == "1" {
        let id = request.get("review_id").and_then(Value::as_str).unwrap_or_default();
        template = template.replace("__REVIEW_ID__", id);
    }
    match var("STUB_MODE").as_deref().unwrap_or("result") {
        "mutate" => {
            let path = var("STUB_MUTATE").expect("STUB_MUTATE");
            let mut file = std::fs::OpenOptions::new().append(true).open(path).expect("open the mutated file");
            let _ = file.write_all(b"\nappended while the human was deciding\n");
        }
        "git" => {
            let args = var("STUB_GIT").expect("STUB_GIT");
            let status = std::process::Command::new("git").args(args.split('\t')).status().expect("run git");
            assert!(status.success(), "git {args} failed");
        }
        "garbage" => {
            println!("rdg: this is not json");
            return 0;
        }
        "fail" => {
            eprintln!("rdg: repository is not open or is not a Git worktree");
            return 1;
        }
        _ => {}
    }
    println!("{template}");
    0
}

fn append(path: &str, text: &str) {
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).expect("open the record");
    file.write_all(text.as_bytes()).expect("record");
}

fn rex(args: &[String]) -> i32 {
    if let Some(record) = var("FAKE_RECORD") {
        append(&record, &format!("{}\n---\n", args.join("\n")));
    }
    if args.first().map(String::as_str) == Some("split") && args.iter().any(|a| a == "--json") {
        println!(r#"{{"block_ids":["block:page"]}}"#);
    } else if args.len() >= 2 && args[0] == "block" && args[1] == "ls" {
        let mut blocks = vec![serde_json::json!({"block_id": "block:agent"})];
        if var("FAKE_PANE_STAYS").as_deref().unwrap_or("1") == "1" {
            blocks.push(serde_json::json!({"block_id": "block:page"}));
        }
        println!("{}", serde_json::json!({ "blocks": blocks }));
    }
    var("FAKE_EXIT").and_then(|c| c.parse().ok()).unwrap_or(0)
}

fn tern(args: &[String]) -> i32 {
    let op: Value = args.get(1).and_then(|a| serde_json::from_str(a).ok()).unwrap_or(Value::Null);
    if let Some(record) = var("FAKE_RECORD") {
        append(&record, &format!("{}\n", serde_json::to_string(&op).expect("encode")));
    }
    let mode = var("FAKE_MODE").unwrap_or_else(|| "ok".into());
    if mode == "error" {
        println!(r#"{{"error":{{"kind":"no_window","message":"no Tern window is open"}}}}"#);
        return 1;
    }
    match op.get("op").and_then(Value::as_str) {
        Some("open") => println!("{}", serde_json::json!({"ok": {"block": 42, "url": op.get("url")}})),
        Some("blocks") => {
            let blocks = if mode == "phantom" { vec![] } else { vec![serde_json::json!({"block": 42, "url": "x", "shown": true})] };
            println!("{}", serde_json::json!({"ok": {"blocks": blocks}}));
        }
        _ => println!(r#"{{"ok":{{}}}}"#),
    }
    0
}

/// The calls a fake `rex` recorded, each as its argument list.
pub fn rex_calls(record: &Path) -> Vec<Vec<String>> {
    let Ok(text) = std::fs::read_to_string(record) else { return Vec::new() };
    text.split("---\n")
        .filter(|call| !call.trim().is_empty())
        .map(|call| call.trim_end_matches('\n').split('\n').map(str::to_string).collect())
        .collect()
}

/// The ops a fake `tern` recorded.
pub fn tern_ops(record: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(record) else { return Vec::new() };
    text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}
