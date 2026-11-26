//! The on-demand inbox, with reviews and
//! questions from many clients and one service.
//!
//! Every test owns a `Case`, whose private `SENPAI_HOME` lives in its own
//! directory, and every process it starts is a `Proc`. Dropping a `Proc` kills
//! its child; dropping the `Case` stops the service, so nothing is left running
//! even when an assertion fails.

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, UNIX_EPOCH};

use crate::fakes;
use crate::support::*;

pub const TESTS: &[(&str, fn())] = &[
    ("review_is_decided_through_the_inbox", review_is_decided_through_the_inbox),
    ("two_reviews_are_decided_independently", two_reviews_are_decided_independently),
    ("document_review_through_the_inbox", document_review_through_the_inbox),
    ("closing_the_page_keeps_the_review_waiting", closing_the_page_keeps_the_review_waiting),
    (
        "invalid_decision_is_rejected_and_the_review_keeps_waiting",
        invalid_decision_is_rejected_and_the_review_keeps_waiting,
    ),
    ("page_lifetime_still_applies", page_lifetime_still_applies),
    ("unopened_page_is_cancelled_after_the_idle_limit", unopened_page_is_cancelled_after_the_idle_limit),
    ("decision_after_the_review_ended_is_refused", decision_after_the_review_ended_is_refused),
    ("home_owned_by_another_user_is_refused", home_owned_by_another_user_is_refused),
    ("interrupted_client_withdraws_its_review", interrupted_client_withdraws_its_review),
    ("killed_client_withdraws_its_review", killed_client_withdraws_its_review),
    ("socket_path_too_long_fails_closed", socket_path_too_long_fails_closed),
    ("timed_out_client_withdraws_its_review", timed_out_client_withdraws_its_review),
    ("stopped_service_fails_the_review_closed", stopped_service_fails_the_review_closed),
    ("killed_service_fails_the_review_closed", killed_service_fails_the_review_closed),
    ("concurrent_starts_share_one_service", concurrent_starts_share_one_service),
    ("service_files_are_private", service_files_are_private),
    ("stale_socket_is_replaced", stale_socket_is_replaced),
    ("idle_service_exits", idle_service_exits),
    ("service_from_another_build_restarts_when_idle", service_from_another_build_restarts_when_idle),
    (
        "service_from_another_build_with_pending_items_is_kept",
        service_from_another_build_with_pending_items_is_kept,
    ),
    ("inbox_page_is_served", inbox_page_is_served),
    ("question_answered_with_an_option", question_answered_with_an_option),
    ("question_answered_in_free_text", question_answered_in_free_text),
    ("question_dismissed", question_dismissed),
    ("cancelled_question_is_withdrawn", cancelled_question_is_withdrawn),
    ("question_times_out", question_times_out),
    ("question_usage_errors", question_usage_errors),
    ("review_decided_over_the_socket", review_decided_over_the_socket),
    ("terminal_view_counts_as_opening_the_page", terminal_view_counts_as_opening_the_page),
    ("withdrawn_review_cannot_be_decided", withdrawn_review_cannot_be_decided),
    ("question_answered_over_the_socket", question_answered_over_the_socket),
    ("review_opens_beside_the_agent_in_rex", review_opens_beside_the_agent_in_rex),
    ("failed_split_leaves_the_review_waiting", failed_split_leaves_the_review_waiting),
    ("question_opens_in_rex_and_surface_none_does_not", question_opens_in_rex_and_surface_none_does_not),
    (
        "review_page_opens_beside_the_agent_in_tern_and_closes_after",
        review_page_opens_beside_the_agent_in_tern_and_closes_after,
    ),
    ("tern_wins_over_rex_and_surface_can_choose", tern_wins_over_rex_and_surface_can_choose),
    ("failed_or_phantom_tern_page_leaves_the_review_waiting", failed_or_phantom_tern_page_leaves_the_review_waiting),
    ("killed_client_closes_the_tern_page", killed_client_closes_the_tern_page),
    ("question_shows_the_inbox_in_tern", question_shows_the_inbox_in_tern),
    ("rex_shows_the_real_page_with_terminal_browser", rex_shows_the_real_page_with_terminal_browser),
    (
        "rex_falls_back_to_the_terminal_view_when_the_page_pane_closes",
        rex_falls_back_to_the_terminal_view_when_the_page_pane_closes,
    ),
    ("killed_client_closes_the_rex_page_pane", killed_client_closes_the_rex_page_pane),
    ("timed_out_review_closes_its_page_pane", timed_out_review_closes_its_page_pane),
    ("interrupted_question_closes_its_tern_page", interrupted_question_closes_its_tern_page),
    ("tui_needs_a_terminal", tui_needs_a_terminal),
];

/// Git settings for the repositories these tests build.
const INBOX_GIT_ENV: [(&str, &str); 7] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_AUTHOR_NAME", "Inbox"),
    ("GIT_AUTHOR_EMAIL", "inbox@example.com"),
    ("GIT_COMMITTER_NAME", "Inbox"),
    ("GIT_COMMITTER_EMAIL", "inbox@example.com"),
];

fn sleep(seconds: f64) {
    std::thread::sleep(Duration::from_secs_f64(seconds));
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// `str(value)` for an item id.
fn id_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// A GET whose JSON body is returned; a non-success status fails the test.
fn get_json(url: &str) -> Value {
    let response = get(url);
    assert!((200..300).contains(&response.status), "GET {url}: {} {}", response.status, response.text());
    response.json()
}

/// The status of a JSON POST.
fn post_status(url: &str, payload: &Value) -> u16 {
    post(url, payload).status
}

/// A started process, killed and reaped when dropped.
struct Proc {
    child: Option<Child>,
}

impl Proc {
    fn child(&mut self) -> &mut Child {
        self.child.as_mut().expect("the process is still owned")
    }

    fn pid(&mut self) -> i32 {
        self.child().id() as i32
    }

    /// `process.poll() is None`.
    fn running(&mut self) -> bool {
        self.child().try_wait().expect("poll").is_none()
    }

    fn signal(&mut self, signal: i32) {
        let pid = self.pid();
        // SAFETY: kill(2) with a pid this test owns and has not reaped.
        unsafe {
            libc::kill(pid, signal);
        }
    }

    fn kill(&mut self) {
        let _ = self.child().kill();
    }

    /// Waits up to 30 seconds and collects the output.
    fn communicate(&mut self) -> Run {
        finish(self.child.take().expect("the process is still owned"), 30)
    }

    /// The exit code, the JSON on stdout if any, and stderr.
    fn finish(&mut self) -> (i32, Option<Value>, String) {
        let run = self.communicate();
        let result = if run.stdout.trim().is_empty() { None } else { Some(run.json()) };
        (run.code, result, run.stderr)
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A private directory and home, and the environment every command gets.
struct Case {
    home: PathBuf,
    env: BTreeMap<String, String>,
    stop_with: Vec<PathBuf>,
    temp: TempDir,
    _slot: Slot,
}

/// A test whose few-second timeout must outlast the surface opening runs
/// alone among these tests, so the load of the others running in parallel
/// cannot make it miss that window.
static SCHEDULE: RwLock<()> = RwLock::new(());

enum Slot {
    Shared(#[allow(dead_code)] RwLockReadGuard<'static, ()>),
    Alone(#[allow(dead_code)] RwLockWriteGuard<'static, ()>),
}

impl Case {
    fn new() -> Self {
        Self::with_slot(Slot::Shared(SCHEDULE.read().unwrap_or_else(|e| e.into_inner())))
    }

    fn alone() -> Self {
        Self::with_slot(Slot::Alone(SCHEDULE.write().unwrap_or_else(|e| e.into_inner())))
    }

    fn with_slot(slot: Slot) -> Self {
        let temp = TempDir::new();
        let home = temp.path().join("h");
        let mut env = BTreeMap::new();
        for (name, value) in INBOX_GIT_ENV {
            env.insert(name.to_string(), value.to_string());
        }
        env.insert(INBOX_ENV.into(), "1".into());
        env.insert(HOME_ENV.into(), home.to_string_lossy().into_owned());
        env.insert(NO_BROWSER_ENV.into(), "1".into());
        env.insert(TIMEOUT_ENV.into(), "60".into());
        Case { home, env, stop_with: Vec::new(), temp, _slot: slot }
    }

    fn temp(&self) -> &Path {
        self.temp.path()
    }

    fn set(&mut self, name: &str, value: impl Into<String>) {
        self.env.insert(name.to_string(), value.into());
    }

    fn command(&self, binary: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(binary);
        command.args(args);
        isolate(&mut command);
        command.envs(&self.env);
        command
    }

    fn run_bounded(&self, binary: &Path, args: &[&str], timeout: u64) -> Run {
        run_with(&mut self.command(binary, args), None, timeout)
    }

    fn stop_service(&self, binary: &Path) {
        let _ = self.run_bounded(binary, &["serve", "--stop"], 20);
    }

    fn git(&self, root: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        command.args(args).current_dir(root);
        isolate(&mut command);
        command.envs(&self.env);
        let run = run_with(&mut command, None, 60);
        assert_eq!(run.code, 0, "git {args:?}: {}", run.stderr);
    }

    fn repo(&self, name: &str) -> PathBuf {
        let root = self.temp().join(name);
        std::fs::create_dir(&root).expect("create the repository");
        self.git(&root, &["init", "-q"]);
        std::fs::write(root.join("notes.txt"), "one\n").expect("write");
        self.git(&root, &["add", "."]);
        self.git(&root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("notes.txt"), "one\ntwo\n").expect("write");
        root
    }

    fn start_with(&self, args: &[&str], url_file: Option<&Path>, binary: &Path) -> Proc {
        let mut command = self.command(binary, args);
        if let Some(url_file) = url_file {
            command.env(URL_FILE_ENV, url_file);
        }
        Proc { child: Some(spawn(&mut command)) }
    }

    fn start(&self, args: &[&str], url_file: Option<&Path>) -> Proc {
        self.start_with(args, url_file, &binary())
    }

    fn review(&self, root: &Path) -> (Proc, String) {
        self.review_with(root, "ship it")
    }

    fn review_with(&self, root: &Path, criterion: &str) -> (Proc, String) {
        let name = root.file_name().expect("a name").to_string_lossy().into_owned();
        let url_file = self.temp().join(format!("{name}.url"));
        let repo = root.to_string_lossy().into_owned();
        let mut process = self.start(&["review", "--criterion", criterion, "--repo", &repo], Some(&url_file));
        let url = wait_for(10.0, "the review URL", || {
            if !process.running() {
                panic!("{}", process.communicate().stderr);
            }
            std::fs::read_to_string(&url_file).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
        });
        (process, url)
    }

    fn inbox(&self) -> Value {
        self.inbox_with(&binary())
    }

    fn inbox_with(&self, binary: &Path) -> Value {
        let result = self.run_bounded(binary, &["inbox"], 30);
        assert_eq!(result.code, 0, "{}", result.stderr);
        result.json()
    }

    fn inbox_url(&self) -> String {
        self.inbox()["inbox"].as_str().expect("an inbox URL").to_string()
    }

    fn items(&self, inbox: &str) -> Vec<Value> {
        get_json(&format!("{inbox}api/items"))["items"].as_array().expect("items").clone()
    }

    fn verify(&self, root: &Path) -> i32 {
        let repo = root.to_string_lossy().into_owned();
        self.run_bounded(&binary(), &["verify", "--repo", &repo], 30).code
    }

    fn ask(&self, args: &[&str]) -> (Proc, String, Value) {
        let mut all = vec!["ask"];
        all.extend_from_slice(args);
        let process = self.start(&all, None);
        let socket = self.home.join("senpai.sock");
        let inbox = wait_for(10.0, "the service", || {
            if !socket.exists() {
                return None;
            }
            self.inbox()["inbox"].as_str().filter(|s| !s.is_empty()).map(str::to_string)
        });
        let item = wait_for(10.0, "the question", || self.items(&inbox).first().cloned());
        (process, inbox, item)
    }

    /// One request on the inbox socket, as `TerminalSurfaceTest.call`.
    fn call(&self, message: Value) -> Value {
        socket_call(&self.home, message)
    }

    fn prepend_path(&mut self, dir: &Path) {
        let current = self.env.get("PATH").cloned().unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
        self.set("PATH", format!("{}:{current}", dir.display()));
    }

    fn bin_dir(&self) -> PathBuf {
        let dir = self.temp().join("bin");
        std::fs::create_dir_all(&dir).expect("create bin");
        dir
    }

    /// A `rex` that records its arguments; `split --json` makes block:page.
    fn fake_rex(&mut self, exit_code: i32, pane_stays: bool) -> PathBuf {
        let bin_dir = self.bin_dir();
        let record = self.temp().join("rex-calls");
        let stays = if pane_stays { "1" } else { "0" };
        fakes::install(
            &bin_dir,
            "rex",
            "rex",
            &[
                ("FAKE_RECORD", record.to_string_lossy().into_owned()),
                ("FAKE_EXIT", exit_code.to_string()),
                ("FAKE_PANE_STAYS", stays.to_string()),
            ],
        );
        self.prepend_path(&bin_dir);
        self.set("REX_SESSION", "session:test");
        self.set("REX_BLOCK", "block:agent");
        record
    }

    /// A `tern` that records each browser op and answers like Tern does.
    fn fake_tern(&mut self, mode: &str) -> PathBuf {
        let bin_dir = self.bin_dir();
        let record = self.temp().join("tern-ops");
        fakes::install(
            &bin_dir,
            "tern",
            "tern",
            &[("FAKE_RECORD", record.to_string_lossy().into_owned()), ("FAKE_MODE", mode.to_string())],
        );
        self.prepend_path(&bin_dir);
        self.set(TERN_ENV, "1234");
        record
    }

    fn fake_terminal_browser(&mut self) -> PathBuf {
        let program = script(&self.bin_dir().join("terminal-browser"), "#!/bin/sh\nexit 0\n");
        self.set(TERMINAL_BROWSER_ENV, program.to_string_lossy().into_owned());
        program
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        for binary in &self.stop_with {
            self.stop_service(binary);
        }
        self.stop_service(&binary());
    }
}

fn approve() -> Value {
    json!({"decision": "approve", "annotations": []})
}

// -- reviews -----------------------------------------------------------------

fn review_is_decided_through_the_inbox() {
    let case = Case::new();
    let root = case.repo("one");
    let (mut process, url) = case.review(&root);
    let inbox = case.inbox_url();
    let items = case.items(&inbox);
    assert_eq!(items.len(), 1, "{items:?}");
    let item = &items[0];
    assert_eq!(item["kind"], "diff");
    assert_eq!(item["title"], "ship it");
    assert_eq!(item["places"], json!([root.to_string_lossy()]));
    assert!(url.ends_with(item["url"].as_str().expect("a url")), "{url} {item}");
    let payload = get_json(&format!("{url}api/review"));
    assert_eq!(payload["repositories"][0]["files"][0]["path"], "notes.txt");
    assert_eq!(post_status(&format!("{url}api/decision"), &approve()), 204);
    let (code, result, err) = process.finish();
    assert_eq!(code, EXIT_OK, "{err}");
    assert_eq!(result.expect("a result")["decision"], "approve");
    assert_eq!(case.verify(&root), EXIT_OK);
    assert_eq!(case.items(&inbox), Vec::<Value>::new());
}

fn two_reviews_are_decided_independently() {
    let case = Case::new();
    let (first, second) = (case.repo("first"), case.repo("second"));
    let (mut first_process, first_url) = case.review_with(&first, "first change");
    let (mut second_process, second_url) = case.review_with(&second, "second change");
    let inbox = case.inbox_url();
    assert_eq!(first_url.split('/').nth(2), second_url.split('/').nth(2), "one service hosts both");
    let mut titles: Vec<String> = case.items(&inbox).iter().map(|i| i["title"].as_str().unwrap_or("").to_string()).collect();
    titles.sort();
    assert_eq!(titles, strings(&["first change", "second change"]));
    let feedback = json!({"decision": "request-changes",
                          "annotations": [{"text": "split this", "target": {"scope": "review"}}]});
    assert_eq!(post_status(&format!("{second_url}api/decision"), &feedback), 204);
    let (code, result, _) = second_process.finish();
    assert_eq!((code, result.expect("a result")["decision"].clone()), (EXIT_NOT_APPROVED, json!("request-changes")));
    let titles: Vec<Value> = case.items(&inbox).iter().map(|i| i["title"].clone()).collect();
    assert_eq!(titles, vec![json!("first change")]);
    assert_eq!(post_status(&format!("{first_url}api/decision"), &approve()), 204);
    let (code, result, _) = first_process.finish();
    assert_eq!((code, result.expect("a result")["decision"].clone()), (EXIT_OK, json!("approve")));
    assert_eq!(case.verify(&first), EXIT_OK);
    assert_ne!(case.verify(&second), EXIT_OK);
}

fn document_review_through_the_inbox() {
    let case = Case::new();
    let document = case.temp().join("plan.md");
    write(&document, "# Plan\n\nDo the thing.\n");
    let url_file = case.temp().join("doc.url");
    let doc = document.to_string_lossy().into_owned();
    let mut process =
        case.start(&["review-doc", "--doc", &doc, "--criterion", "approve the plan"], Some(&url_file));
    let url = wait_for(10.0, "the document URL", || {
        std::fs::read_to_string(&url_file).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
    });
    let inbox = case.inbox_url();
    let items = case.items(&inbox);
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!((items[0]["kind"].clone(), items[0]["places"].clone()), (json!("document"), json!([doc])));
    let comment = json!({"text": "say which thing", "target": {"scope": "line", "path": doc,
                                                               "start_line": 3, "end_line": 3}});
    assert_eq!(
        post_status(&format!("{url}api/doc-decision"), &json!({"decision": "request-changes", "annotations": [comment]})),
        204
    );
    let (code, result, err) = process.finish();
    assert_eq!(code, EXIT_NOT_APPROVED, "{err}");
    assert_eq!(result.expect("a result")["annotations"][0]["target"]["start_line"], 3);
}

fn closing_the_page_keeps_the_review_waiting() {
    let case = Case::new();
    let (mut process, url) = case.review(&case.repo("one"));
    let page = get(&url);
    assert_eq!(page.status, 200, "{}", page.text());
    assert_eq!(post_status(&format!("{url}api/closing?page=1"), &json!({})), 204);
    sleep(6.0);
    assert!(process.running());
    assert_eq!(case.items(&case.inbox_url()).len(), 1);
}

fn invalid_decision_is_rejected_and_the_review_keeps_waiting() {
    let case = Case::new();
    let (mut process, url) = case.review(&case.repo("one"));
    assert_eq!(
        post_status(&format!("{url}api/decision"), &json!({"decision": "request-changes", "annotations": []})),
        400
    );
    assert!(process.running());
}

fn page_lifetime_still_applies() {
    let mut case = Case::new();
    case.set(TIMEOUT_ENV, "0");
    case.set(LIFETIME_ENV, "1");
    let (mut process, url) = case.review(&case.repo("one"));
    let page = get(&url);
    assert_eq!(page.status, 200, "{}", page.text());
    let (code, result, _) = process.finish();
    assert_eq!((code, result.expect("a result")["decision"].clone()), (EXIT_NOT_APPROVED, json!("cancelled")));
}

fn unopened_page_is_cancelled_after_the_idle_limit() {
    let mut case = Case::new();
    case.set(TIMEOUT_ENV, "0");
    case.set(IDLE_ENV, "1");
    let (mut process, _) = case.review(&case.repo("one"));
    let (code, result, _) = process.finish();
    assert_eq!((code, result.expect("a result")["decision"].clone()), (EXIT_NOT_APPROVED, json!("cancelled")));
}

fn decision_after_the_review_ended_is_refused() {
    let case = Case::new();
    let (mut process, url) = case.review(&case.repo("one"));
    assert_eq!(post_status(&format!("{url}api/decision"), &approve()), 204);
    process.finish();
    let status = post_status(&format!("{url}api/decision"), &approve());
    assert!([404, 409].contains(&status), "{status}");
}

fn home_owned_by_another_user_is_refused() {
    let mut case = Case::new();
    case.set(HOME_ENV, "/usr");
    let result = case.run_bounded(&binary(), &["inbox"], 10);
    assert_eq!(result.code, EXIT_ERROR);
    assert!(result.stderr.contains("belongs to another user"), "{}", result.stderr);
    let stop = case.run_bounded(&binary(), &["serve", "--stop"], 10);
    assert_eq!(stop.code, EXIT_ERROR);
    assert!(stop.stderr.contains("belongs to another user"), "{}", stop.stderr);
}

fn interrupted_client_withdraws_its_review() {
    let case = Case::new();
    let (mut process, _) = case.review(&case.repo("one"));
    let inbox = case.inbox_url();
    assert_eq!(case.items(&inbox).len(), 1);
    process.signal(libc::SIGINT);
    let (code, result, _) = process.finish();
    assert_eq!((code, result), (EXIT_ERROR, None));
    wait_for(10.0, "the review to be withdrawn", || case.items(&inbox).is_empty().then_some(()));
}

fn killed_client_withdraws_its_review() {
    let case = Case::new();
    let (mut process, _) = case.review(&case.repo("one"));
    let inbox = case.inbox_url();
    process.kill();
    process.communicate();
    wait_for(10.0, "the review to be withdrawn", || case.items(&inbox).is_empty().then_some(()));
}

fn socket_path_too_long_fails_closed() {
    let mut case = Case::new();
    let home = case.temp().join("d".repeat(120));
    case.set(HOME_ENV, home.to_string_lossy().into_owned());
    let result = case.run_bounded(&binary(), &["inbox"], 10);
    assert_eq!(result.code, EXIT_ERROR);
    assert!(result.stderr.contains("too long"), "{}", result.stderr);
}

fn timed_out_client_withdraws_its_review() {
    let mut case = Case::new();
    case.set(TIMEOUT_ENV, "2");
    let (mut process, _) = case.review(&case.repo("one"));
    let inbox = case.inbox_url();
    let (code, _, err) = process.finish();
    assert_eq!(code, EXIT_ERROR);
    assert!(err.contains("timed out"), "{err}");
    wait_for(10.0, "the review to be withdrawn", || case.items(&inbox).is_empty().then_some(()));
}

fn stopped_service_fails_the_review_closed() {
    let case = Case::new();
    let root = case.repo("one");
    let (mut process, _) = case.review(&root);
    let stopped = case.run_bounded(&binary(), &["serve", "--stop"], 20);
    assert_eq!(stopped.json(), json!({"stopped": true}));
    let (code, result, err) = process.finish();
    assert_eq!((code, result), (EXIT_ERROR, None));
    assert!(err.contains("stopped before a decision"), "{err}");
    assert_ne!(case.verify(&root), EXIT_OK);
}

fn killed_service_fails_the_review_closed() {
    let case = Case::new();
    let root = case.repo("one");
    let (mut process, url) = case.review(&root);
    let port: u16 = url.split(':').nth(2).and_then(|r| r.split('/').next()).and_then(|p| p.parse().ok()).expect("a port");
    let lsof = run(Command::new("lsof").args(["-t", "-sTCP:LISTEN", &format!("-iTCP:{port}")]));
    let pid: i32 = lsof.stdout.split_whitespace().next().and_then(|p| p.parse().ok()).expect("the service pid");
    // SAFETY: kill(2) on the service this test started.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let (code, result, _) = process.finish();
    assert_eq!((code, result), (EXIT_ERROR, None));
    assert_ne!(case.verify(&root), EXIT_OK);
    assert_eq!(case.inbox()["pending"], 0, "a new service starts over the stale socket");
}

// -- the service -------------------------------------------------------------

fn concurrent_starts_share_one_service() {
    let case = Case::new();
    let mut processes: Vec<Proc> = (0..5).map(|_| case.start(&["inbox"], None)).collect();
    let urls: std::collections::BTreeSet<String> = processes
        .iter_mut()
        .map(|p| p.communicate().json()["inbox"].as_str().expect("an inbox URL").to_string())
        .collect();
    assert_eq!(urls.len(), 1, "{urls:?}");
}

fn service_files_are_private() {
    let case = Case::new();
    case.inbox();
    let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode();
    assert_eq!(mode(&case.home) & 0o777, 0o700);
    assert_eq!(mode(&case.home.join("senpai.sock")) & 0o077, 0);
}

fn stale_socket_is_replaced() {
    let case = Case::new();
    std::fs::DirBuilder::new().mode(0o700).create(&case.home).expect("create home");
    let stale = UnixListener::bind(case.home.join("senpai.sock")).expect("bind");
    drop(stale);
    assert_eq!(case.inbox()["pending"], 0);
}

fn idle_service_exits() {
    let mut case = Case::new();
    case.set(SERVE_IDLE_ENV, "1");
    case.inbox();
    let socket = case.home.join("senpai.sock");
    wait_for(10.0, "the idle service to exit", || (!socket.exists()).then_some(()));
}

/// `shutil.copy2`: the binary with its permissions and modification time.
fn copy_binary(target: &Path) {
    std::fs::copy(binary(), target).expect("copy the binary");
    let modified = std::fs::metadata(binary()).and_then(|m| m.modified()).expect("binary mtime");
    set_mtime(target, modified);
}

fn set_mtime(path: &Path, time: std::time::SystemTime) {
    let file = std::fs::File::options().write(true).open(path).expect("open the copy");
    file.set_modified(time).expect("set the mtime");
}

fn old_time() -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_000_000_000)
}

fn service_from_another_build_restarts_when_idle() {
    let mut case = Case::new();
    let old = case.temp().join("old-senpai");
    let new = case.temp().join("new-senpai");
    copy_binary(&old);
    copy_binary(&new);
    set_mtime(&old, old_time());
    case.stop_with = vec![old.clone(), new.clone()];
    let first = case.inbox_with(&old)["inbox"].clone();
    let second = case.inbox_with(&new)["inbox"].clone();
    assert_ne!(first, second);
}

fn service_from_another_build_with_pending_items_is_kept() {
    let mut case = Case::new();
    let old = case.temp().join("old-senpai");
    copy_binary(&old);
    set_mtime(&old, old_time());
    case.stop_with = vec![old.clone()];
    let mut asking = case.start_with(&["ask", "--question", "keep me?"], None, &old);
    let socket = case.home.join("senpai.sock");
    let inbox = wait_for(10.0, "the old service", || {
        if !(case.home.exists() && socket.exists()) {
            return None;
        }
        case.inbox_with(&old)["inbox"].as_str().filter(|s| !s.is_empty()).map(str::to_string)
    });
    wait_for(10.0, "the question", || (!case.items(&inbox).is_empty()).then_some(()));
    let result = case.run_bounded(&binary(), &["inbox"], 30);
    assert_eq!(result.code, EXIT_ERROR);
    assert!(result.stderr.contains("another build"), "{}", result.stderr);
    asking.kill();
    asking.communicate();
    case.stop_service(&old);
}

fn inbox_page_is_served() {
    let case = Case::new();
    let inbox = case.inbox_url();
    let response = get(&inbox);
    assert_eq!(response.status, 200);
    let page = response.text();
    assert!(page.contains("senpai inbox"), "{page}");
    assert!(page.contains(r#""itemsUrl":"api/items""#), "{page}");
    let policy = response.header("Content-Security-Policy").expect("a Content-Security-Policy");
    assert!(policy.contains("default-src 'none'"), "{policy}");
    let base = inbox.rsplitn(3, '/').nth(2).expect("a base URL");
    for path in ["/", "/wrong-token/", "/wrong-token/api/items", "/wrong-token/font/nunito-latin-var.woff2"] {
        assert_eq!(get(&format!("{base}{path}")).status, 404, "{path}");
    }
    assert!(page.contains(r#"src:url("font/nunito-latin-var.woff2")"#), "the inbox loads the bundled font");
    let font = get(&format!("{inbox}font/nunito-latin-var.woff2"));
    assert_eq!(font.status, 200);
    assert_eq!(font.header("Content-Type"), Some("font/woff2"));
    let bundled = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/nunito-latin-var.woff2")).expect("the bundled font");
    assert_eq!(font.body, bundled, "the inbox serves the bundled font unchanged");
    assert_eq!(get(&format!("{inbox}font/missing.woff2")).status, 404);
}

// -- questions ---------------------------------------------------------------

fn question_answered_with_an_option() {
    let case = Case::new();
    let (mut process, inbox, item) =
        case.ask(&["--question", "Which port?", "--option", "8080", "--option", "9090", "--context", "/srv/app"]);
    assert_eq!(
        (item["kind"].clone(), item["title"].clone(), item["options"].clone(), item["places"].clone()),
        (json!("question"), json!("Which port?"), json!(["8080", "9090"]), json!(["/srv/app"]))
    );
    let answer = format!("{inbox}api/answer");
    let id = item["id"].clone();
    assert_eq!(post_status(&answer, &json!({"id": id, "option": "7070", "text": ""})), 400);
    assert_eq!(post_status(&answer, &json!({"id": id, "option": null, "text": "  "})), 400);
    assert_eq!(post_status(&answer, &json!({"id": id, "option": "9090", "text": " free one "})), 204);
    assert_eq!(post_status(&answer, &json!({"id": id, "option": "9090", "text": ""})), 400);
    let (code, result, err) = process.finish();
    assert_eq!(code, EXIT_OK, "{err}");
    assert_eq!(result, Some(json!({"version": "1", "question_id": id, "option": "9090", "text": "free one"})));
}

fn question_answered_in_free_text() {
    let case = Case::new();
    let (mut process, inbox, item) = case.ask(&["--question", "Name for the flag?"]);
    assert_eq!(post_status(&format!("{inbox}api/answer"), &json!({"id": item["id"], "text": "dry-run"})), 204);
    let (code, result, _) = process.finish();
    let result = result.expect("a result");
    assert_eq!((code, result["option"].clone(), result["text"].clone()), (EXIT_OK, Value::Null, json!("dry-run")));
}

fn question_dismissed() {
    let case = Case::new();
    let (mut process, inbox, item) = case.ask(&["--question", "Delete the cache?"]);
    assert_eq!(post_status(&format!("{inbox}api/answer"), &json!({"id": item["id"], "dismissed": true})), 204);
    let (code, result, _) = process.finish();
    assert_eq!((code, result.expect("a result")["dismissed"].clone()), (EXIT_NOT_APPROVED, json!(true)));
}

fn cancelled_question_is_withdrawn() {
    let case = Case::new();
    let (mut process, inbox, _) = case.ask(&["--question", "Still there?"]);
    process.signal(libc::SIGINT);
    let run = process.communicate();
    assert_eq!(run.code, EXIT_ERROR, "SIGINT ends the question: {run:?}");
    wait_for(10.0, "the question to be withdrawn", || case.items(&inbox).is_empty().then_some(()));
}

fn question_times_out() {
    let mut case = Case::new();
    case.set(TIMEOUT_ENV, "1");
    let (mut process, inbox, _) = case.ask(&["--question", "Anyone?"]);
    let (code, result, err) = process.finish();
    assert_eq!((code, result), (EXIT_ERROR, None));
    assert!(err.contains("timed out"), "{err}");
    wait_for(10.0, "the question to be withdrawn", || case.items(&inbox).is_empty().then_some(()));
}

fn question_usage_errors() {
    let case = Case::new();
    let cases: [&[&str]; 4] = [&[], &["--question", "  "], &["--question"], &["--bogus", "x"]];
    for args in cases {
        let mut all = vec!["ask"];
        all.extend_from_slice(args);
        let result = case.run_bounded(&binary(), &all, 10);
        assert_eq!(result.code, EXIT_ERROR, "{args:?}");
        assert!(result.stderr.contains("usage: senpai ask"), "{args:?}: {}", result.stderr);
    }
    assert!(!case.home.exists(), "usage errors start no service");
}

// -- the terminal surface: `senpai tui` over the socket, Rex and Tern -------

fn review_decided_over_the_socket() {
    let case = Case::new();
    let root = case.repo("one");
    let (mut process, _) = case.review(&root);
    let items = case.call(json!({"op": "items"}))["items"].as_array().expect("items").clone();
    assert_eq!(items.len(), 1, "{items:?}");
    let id = items[0]["id"].clone();
    let payload = case.call(json!({"op": "payload", "id": id}));
    assert_eq!((payload["kind"].clone(), payload["title"].clone()), (json!("diff"), json!("ship it")));
    let key = payload["key"].clone();
    let wrong = case.call(json!({"op": "decide", "id": id, "key": "not-the-key", "decision": approve()}));
    assert!(wrong["error"].as_str().unwrap_or("").contains("no longer waiting"), "{wrong}");
    assert_eq!(payload["payload"]["repositories"][0]["files"][0]["path"], "notes.txt");
    let refused = case.call(json!({"op": "decide", "id": id, "key": key,
                                   "decision": {"decision": "request-changes", "annotations": []}}));
    assert!(refused["error"].as_str().unwrap_or("").contains("comment"), "{refused}");
    let bad_line = json!({"text": "x", "target": {"scope": "line", "repository": root.to_string_lossy(),
                                                  "path": "notes.txt", "side": "new",
                                                  "start_line": 99, "end_line": 99}});
    let refused = case.call(json!({"op": "decide", "id": id, "key": key,
                                   "decision": {"decision": "approve", "annotations": [bad_line]}}));
    assert!(refused["error"].as_str().unwrap_or("").contains("is not a line"), "{refused}");
    assert!(process.running());
    let mut good_line = bad_line.clone();
    good_line["target"]["start_line"] = json!(2);
    good_line["target"]["end_line"] = json!(2);
    assert_eq!(
        case.call(json!({"op": "decide", "id": id, "key": key,
                         "decision": {"decision": "approve", "annotations": [good_line]}})),
        json!({"decided": true})
    );
    let (code, result, err) = process.finish();
    assert_eq!(code, EXIT_OK, "{err}");
    assert_eq!(result.expect("a result")["annotations"][0]["target"]["start_line"], 2);
    assert_eq!(case.verify(&root), EXIT_OK);
    let late = case.call(json!({"op": "decide", "id": id, "key": key, "decision": approve()}));
    assert!(late["error"].as_str().unwrap_or("").contains("no longer waiting"), "{late}");
}

fn terminal_view_counts_as_opening_the_page() {
    let mut case = Case::new();
    case.set(TIMEOUT_ENV, "0");
    case.set(IDLE_ENV, "2");
    let (mut process, _) = case.review(&case.repo("one"));
    let items = case.call(json!({"op": "items"}))["items"].as_array().expect("items").clone();
    assert_eq!(items.len(), 1, "{items:?}");
    let id = items[0]["id"].clone();
    let key = case.call(json!({"op": "payload", "id": id}))["key"].clone();
    sleep(3.0);
    assert!(process.running(), "the idle limit must not cancel a review open in the terminal");
    case.call(json!({"op": "decide", "id": id, "key": key, "decision": approve()}));
    assert_eq!(process.finish().0, EXIT_OK);
}

fn withdrawn_review_cannot_be_decided() {
    let case = Case::new();
    let (mut process, _) = case.review(&case.repo("one"));
    let items = case.call(json!({"op": "items"}))["items"].as_array().expect("items").clone();
    assert_eq!(items.len(), 1, "{items:?}");
    let id = items[0]["id"].clone();
    let key = case.call(json!({"op": "payload", "id": id}))["key"].clone();
    process.kill();
    process.communicate();
    wait_for(10.0, "withdrawal", || (case.call(json!({"op": "items"}))["items"] == json!([])).then_some(()));
    let reply = case.call(json!({"op": "decide", "id": id, "key": key, "decision": approve()}));
    assert!(reply["error"].as_str().unwrap_or("").contains("no longer waiting"), "{reply}");
}

fn question_answered_over_the_socket() {
    let case = Case::new();
    let (mut process, _, item) = case.ask(&["--question", "Which port?", "--option", "8080"]);
    let id = item["id"].clone();
    let mut payload = case.call(json!({"op": "payload", "id": id}));
    let key = payload.as_object_mut().expect("an object").remove("key").expect("a key");
    assert_eq!(payload, json!({"kind": "question", "title": "Which port?", "options": ["8080"]}));
    let wrong = case.call(json!({"op": "decide", "id": id, "key": key, "decision": {"option": "1"}}));
    assert!(wrong["error"].as_str().unwrap_or("").contains("not one of"), "{wrong}");
    assert_eq!(
        case.call(json!({"op": "decide", "id": id, "key": key, "decision": {"option": "8080", "text": ""}})),
        json!({"decided": true})
    );
    let (code, result, _) = process.finish();
    assert_eq!((code, result.expect("a result")["option"].clone()), (EXIT_OK, json!("8080")));
}

fn review_opens_beside_the_agent_in_rex() {
    let mut case = Case::new();
    let record = case.fake_rex(0, true);
    let (mut process, _) = case.review(&case.repo("one"));
    let items = case.items(&case.inbox_url());
    assert_eq!(items.len(), 1, "{items:?}");
    let calls = wait_for(10.0, "the rex split", || Some(fakes::rex_calls(&record)).filter(|c| !c.is_empty()));
    assert_eq!(calls.len(), 1, "{calls:?}");
    let call = &calls[0];
    assert_eq!(
        call[..11].to_vec(),
        strings(&["split", "-s", "session:test", "-b", "block:agent", "--split=right", "--focus=false", "--label",
                  "senpai", "--shell=none", "--"])
    );
    assert_eq!(call[11..13].to_vec(), vec!["/usr/bin/env".to_string(), format!("{HOME_ENV}={}", case.home.display())]);
    assert_eq!(call[14..].to_vec(), vec!["tui".to_string(), "--item".to_string(), id_text(&items[0]["id"])]);
    process.kill();
}

fn failed_split_leaves_the_review_waiting() {
    let mut case = Case::new();
    let record = case.fake_rex(3, true);
    let url_file = case.temp().join("one.url");
    let repo = case.repo("one").to_string_lossy().into_owned();
    let mut process = case.start(&["review", "--criterion", "ship it", "--repo", &repo], Some(&url_file));
    wait_for(10.0, "the rex split", || (!fakes::rex_calls(&record).is_empty()).then_some(()));
    sleep(0.5);
    assert!(process.running());
    assert_eq!(case.items(&case.inbox_url()).len(), 1);
    process.kill();
    let err = process.communicate().stderr;
    assert!(err.contains("unable to open the review in Rex"), "{err}");
}

fn question_opens_in_rex_and_surface_none_does_not() {
    let mut case = Case::new();
    let record = case.fake_rex(0, true);
    let (mut process, _, item) = case.ask(&["--question", "Ready?"]);
    let calls = wait_for(10.0, "the rex split", || Some(fakes::rex_calls(&record)).filter(|c| !c.is_empty()));
    assert_eq!(calls.len(), 1, "{calls:?}");
    let call = &calls[0];
    assert_eq!(call[call.len() - 3..].to_vec(), vec!["tui".to_string(), "--item".to_string(), id_text(&item["id"])]);
    process.kill();
    process.communicate();
    case.set(SURFACE_ENV, "none");
    let (mut process, _, _) = case.ask(&["--question", "Quiet?"]);
    sleep(0.5);
    assert_eq!(fakes::rex_calls(&record).len(), 1);
    process.kill();
}

fn review_page_opens_beside_the_agent_in_tern_and_closes_after() {
    let mut case = Case::new();
    let record = case.fake_tern("ok");
    let (mut process, url) = case.review(&case.repo("one"));
    let ops = wait_for(10.0, "the tern page", || Some(fakes::tern_ops(&record)).filter(|o| o.len() >= 2));
    assert_eq!(ops[0], json!({"op": "open", "owner": 1234, "url": url, "how": "beside"}));
    assert_eq!(ops[1], json!({"op": "blocks"}));
    assert_eq!(post_status(&format!("{url}api/decision"), &approve()), 204);
    let (code, _, err) = process.finish();
    assert_eq!(code, EXIT_OK, "{err}");
    assert_eq!(fakes::tern_ops(&record).last(), Some(&json!({"op": "close", "block": 42})));
}

fn tern_wins_over_rex_and_surface_can_choose() {
    let mut case = Case::new();
    let tern = case.fake_tern("ok");
    let rex = case.fake_rex(0, true);
    let (mut process, _) = case.review(&case.repo("one"));
    wait_for(10.0, "the tern page", || (!fakes::tern_ops(&tern).is_empty()).then_some(()));
    assert_eq!(fakes::rex_calls(&rex), Vec::<Vec<String>>::new());
    process.kill();
    process.communicate();
    case.set(SURFACE_ENV, "rex");
    let (mut process, _) = case.review(&case.repo("two"));
    wait_for(10.0, "the rex split", || (!fakes::rex_calls(&rex).is_empty()).then_some(()));
    assert_eq!(fakes::tern_ops(&tern).iter().filter(|op| op["op"] == "open").count(), 1);
    process.kill();
    process.communicate();
}

fn failed_or_phantom_tern_page_leaves_the_review_waiting() {
    let mut case = Case::new();
    for (mode, reason) in [("error", "no Tern window is open"), ("phantom", "tern answered without opening the page")] {
        let record = case.fake_tern(mode);
        let _ = std::fs::remove_file(&record);
        let url_file = case.temp().join(format!("{mode}.url"));
        let repo = case.repo(mode).to_string_lossy().into_owned();
        let mut process = case.start(&["review", "--criterion", "ship it", "--repo", &repo], Some(&url_file));
        wait_for(10.0, &format!("the tern call (mode={mode})"), || (!fakes::tern_ops(&record).is_empty()).then_some(()));
        sleep(0.5);
        assert!(process.running(), "mode={mode}");
        process.kill();
        let err = process.communicate().stderr;
        assert!(err.contains(&format!("unable to open the review in Tern ({reason}")), "mode={mode}: {err}");
        let closes: Vec<Value> = fakes::tern_ops(&record).into_iter().filter(|op| op["op"] == "close").collect();
        let expected = if mode == "error" { vec![] } else { vec![json!({"op": "close", "block": 42})] };
        assert_eq!(closes, expected, "mode={mode}: a page that failed the check is closed");
    }
}

fn killed_client_closes_the_tern_page() {
    let mut case = Case::new();
    let record = case.fake_tern("ok");
    let (mut process, _) = case.review(&case.repo("one"));
    wait_for(10.0, "the tern page", || (fakes::tern_ops(&record).len() >= 2).then_some(()));
    process.kill();
    process.communicate();
    wait_for(10.0, "the page to close", || {
        (fakes::tern_ops(&record).last() == Some(&json!({"op": "close", "block": 42}))).then_some(())
    });
}

fn question_shows_the_inbox_in_tern() {
    let mut case = Case::new();
    let record = case.fake_tern("ok");
    let (mut process, inbox, item) = case.ask(&["--question", "Ready?"]);
    let ops = wait_for(10.0, "the tern page", || Some(fakes::tern_ops(&record)).filter(|o| !o.is_empty()));
    assert_eq!(ops[0]["url"], json!(inbox));
    assert_eq!(post_status(&format!("{inbox}api/answer"), &json!({"id": item["id"], "text": "yes"})), 204);
    assert_eq!(process.finish().0, EXIT_OK);
    assert_eq!(fakes::tern_ops(&record).last(), Some(&json!({"op": "close", "block": 42})));
}

fn rex_shows_the_real_page_with_terminal_browser() {
    let mut case = Case::new();
    let record = case.fake_rex(0, true);
    let program = case.fake_terminal_browser();
    let (mut process, url) = case.review(&case.repo("one"));
    let calls = wait_for(10.0, "the page pane", || Some(fakes::rex_calls(&record)).filter(|c| c.len() >= 2));
    let expected = strings(&["split", "-s", "session:test", "-b", "block:agent", "--split=right", "--focus=false",
                                 "--label", "senpai", "--shell=none", "--json", "--", "/bin/sh", "-c"]);
    assert_eq!(calls[0][..expected.len()], expected[..]);
    assert_eq!(calls[0][calls[0].len() - 3..], [String::from("sh"), program.to_string_lossy().into_owned(), url.clone()]);
    assert_eq!(calls[1], strings(&["block", "ls", "-s", "session:test", "--json"]));
    assert_eq!(post_status(&format!("{url}api/decision"), &approve()), 204);
    assert_eq!(process.finish().0, EXIT_OK);
    assert_eq!(
        fakes::rex_calls(&record).last().cloned(),
        Some(strings(&["api", "call", "-s", "session:test", "block.close", r#"{"block_id":"block:page"}"#]))
    );
}

fn rex_falls_back_to_the_terminal_view_when_the_page_pane_closes() {
    let mut case = Case::new();
    let record = case.fake_rex(0, false);
    case.fake_terminal_browser();
    let (mut process, _) = case.review(&case.repo("one"));
    let calls = wait_for(10.0, "the fallback", || Some(fakes::rex_calls(&record)).filter(|c| c.len() >= 4));
    assert_eq!(calls[2][..3].to_vec(), strings(&["api", "call", "-s"]), "the dead page pane is closed");
    let last = &calls[3];
    assert_eq!(last[last.len() - 3..last.len() - 1].to_vec(), strings(&["tui", "--item"]));
    process.kill();
    let err = process.communicate().stderr;
    assert!(
        err.contains("unable to open the page in Rex (the browser pane closed right away); using the terminal view"),
        "{err}"
    );
}

fn killed_client_closes_the_rex_page_pane() {
    let mut case = Case::new();
    let record = case.fake_rex(0, true);
    case.fake_terminal_browser();
    let (mut process, _) = case.review(&case.repo("one"));
    wait_for(10.0, "the page pane", || (fakes::rex_calls(&record).len() >= 2).then_some(()));
    process.kill();
    process.communicate();
    let close = strings(&["api", "call", "-s", "session:test", "block.close"]);
    wait_for(10.0, "the pane to close", || {
        fakes::rex_calls(&record).last().is_some_and(|c| c.len() >= 5 && c[..5] == close[..]).then_some(())
    });
}

fn timed_out_review_closes_its_page_pane() {
    let mut case = Case::alone();
    case.set(TIMEOUT_ENV, "3");
    let record = case.fake_rex(0, true);
    case.fake_terminal_browser();
    let (mut process, _) = case.review(&case.repo("one"));
    let (code, _, err) = process.finish();
    assert_eq!(code, EXIT_ERROR, "{err}");
    let calls = fakes::rex_calls(&record);
    let last = calls.last().unwrap_or_else(|| panic!("a rex call; stderr: {err}"));
    assert_eq!(last[..5.min(last.len())].to_vec(), strings(&["api", "call", "-s", "session:test", "block.close"]));
}

fn interrupted_question_closes_its_tern_page() {
    let mut case = Case::new();
    let record = case.fake_tern("ok");
    let (mut process, _, _) = case.ask(&["--question", "Still there?"]);
    wait_for(10.0, "the tern page", || (fakes::tern_ops(&record).len() >= 2).then_some(()));
    process.signal(libc::SIGINT);
    let run = process.communicate();
    assert_eq!(run.code, EXIT_ERROR);
    assert_eq!(fakes::tern_ops(&record).last(), Some(&json!({"op": "close", "block": 42})));
}

fn tui_needs_a_terminal() {
    let case = Case::new();
    let result = case.run_bounded(&binary(), &["tui"], 10);
    assert_eq!(result.code, EXIT_ERROR);
    let bad = case.run_bounded(&binary(), &["tui", "--item", "x"], 10);
    assert!(bad.stderr.contains("--item needs a number"), "{}", bad.stderr);
}
