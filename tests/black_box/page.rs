//! The built-in review page: payloads, routes, decisions, serving and waiting,
//! the gate end to end, document reviews and the page's own markup and script.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fancy_regex::Regex;

use crate::support::*;

pub const TESTS: &[(&str, fn())] = &[
    // PayloadLineNumberingTests
    ("a_modified_file_numbers_every_line_after_a_form_feed", a_modified_file_numbers_every_line_after_a_form_feed),
    ("an_untracked_file_numbering_counts_its_real_lines", an_untracked_file_numbering_counts_its_real_lines),
    // GuidedPayloadTests
    ("guide_resolves_captured_scope_without_changing_actual_diff", guide_resolves_captured_scope_without_changing_actual_diff),
    // RequestValidationTests
    ("rejects_what_rdg_rejects", rejects_what_rdg_rejects),
    ("rejects_a_request_that_is_not_json", rejects_a_request_that_is_not_json),
    ("requires_a_request", requires_a_request),
    ("reads_the_request_from_stdin_when_the_value_is_absent", reads_the_request_from_stdin_when_the_value_is_absent),
    ("fails_closed_when_a_repository_is_not_a_worktree", fails_closed_when_a_repository_is_not_a_worktree),
    // ServerRouteTests
    ("serves_the_page_with_a_locked_down_policy", serves_the_page_with_a_locked_down_policy),
    ("every_route_is_behind_the_token", every_route_is_behind_the_token),
    ("serves_the_bundled_code_font", serves_the_bundled_code_font),
    ("the_page_uses_the_bundled_font_and_nerd_glyphs", the_page_uses_the_bundled_font_and_nerd_glyphs),
    ("serves_no_filesystem_and_no_directory_listing", serves_no_filesystem_and_no_directory_listing),
    ("answers_the_wrong_method", answers_the_wrong_method),
    ("publishes_the_criterion_the_repositories_and_the_diff", publishes_the_criterion_the_repositories_and_the_diff),
    ("reports_a_scope_and_a_deletion", reports_a_scope_and_a_deletion),
    ("keeps_a_binary_file_uncommentable", keeps_a_binary_file_uncommentable),
    ("refuses_a_decision_the_client_would_reject", refuses_a_decision_the_client_would_reject),
    ("refuses_a_decision_that_is_not_json", refuses_a_decision_that_is_not_json),
    ("records_a_decision_once_and_stops", records_a_decision_once_and_stops),
    ("carries_a_line_comment_with_both_of_its_ends", carries_a_line_comment_with_both_of_its_ends),
    // DecisionTests
    ("approve_carries_every_comment_as_a_line_target", approve_carries_every_comment_as_a_line_target),
    ("a_review_comment_is_a_scope_and_nothing_else", a_review_comment_is_a_scope_and_nothing_else),
    ("a_stale_page_cannot_point_at_a_line_it_never_saw", a_stale_page_cannot_point_at_a_line_it_never_saw),
    ("a_closed_window_cancels_once_the_grace_period_passes", a_closed_window_cancels_once_the_grace_period_passes),
    ("reloading_the_page_keeps_the_review_alive", reloading_the_page_keeps_the_review_alive),
    (
        "a_closing_report_that_arrives_before_the_reloaded_page_is_requested",
        a_closing_report_that_arrives_before_the_reloaded_page_is_requested,
    ),
    (
        "closing_is_behind_the_token_and_needs_post_and_a_page_number",
        closing_is_behind_the_token_and_needs_post_and_a_page_number,
    ),
    ("an_explicit_cancel_still_ends_the_review_at_once", an_explicit_cancel_still_ends_the_review_at_once),
    // ServeAndWaitTests
    ("opens_the_page_when_a_browser_is_allowed", opens_the_page_when_a_browser_is_allowed),
    ("browser_disabled_by_environment", browser_disabled_by_environment),
    ("prints_the_url_so_a_human_can_open_the_page_by_hand", prints_the_url_so_a_human_can_open_the_page_by_hand),
    ("publishes_and_then_withdraws_the_url", publishes_and_then_withdraws_the_url),
    ("a_browser_that_cannot_open_keeps_the_review_alive", a_browser_that_cannot_open_keeps_the_review_alive),
    ("an_opener_that_fails_keeps_the_review_alive", an_opener_that_fails_keeps_the_review_alive),
    // GateIntegrationTests
    ("an_approval_makes_the_gate_pass", an_approval_makes_the_gate_pass),
    ("an_approval_without_comments_makes_the_gate_pass", an_approval_without_comments_makes_the_gate_pass),
    ("requested_changes_leaves_the_gate_failing", requested_changes_leaves_the_gate_failing),
    ("open_line_comments_come_back_as_feedback", open_line_comments_come_back_as_feedback),
    ("feedback_needs_at_least_one_comment", feedback_needs_at_least_one_comment),
    ("a_closed_window_leaves_the_gate_failing", a_closed_window_leaves_the_gate_failing),
    ("a_change_under_the_reviewer_fails_the_gate_closed", a_change_under_the_reviewer_fails_the_gate_closed),
    ("a_refused_decision_leaves_the_review_open", a_refused_decision_leaves_the_review_open),
    ("the_published_url_is_gone_when_the_review_ends", the_published_url_is_gone_when_the_review_ends),
    ("a_killed_client_does_not_leave_a_stale_reviewer", a_killed_client_does_not_leave_a_stale_reviewer),
    ("the_reviewer_expires_on_its_own_lifetime", the_reviewer_expires_on_its_own_lifetime),
    ("junk_lifetime_values_fall_back_to_the_defaults", junk_lifetime_values_fall_back_to_the_defaults),
    // DocumentReviewTests
    ("renders_one_row_per_line_of_the_document", renders_one_row_per_line_of_the_document),
    ("inline_markup_is_rendered_and_source_html_is_escaped", inline_markup_is_rendered_and_source_html_is_escaped),
    ("a_line_comment_comes_back_anchored_to_the_document", a_line_comment_comes_back_anchored_to_the_document),
    ("a_comment_cannot_name_a_line_the_document_does_not_have", a_comment_cannot_name_a_line_the_document_does_not_have),
    ("a_diff_shaped_comment_is_refused", a_diff_shaped_comment_is_refused),
    ("feedback_without_a_comment_is_refused", feedback_without_a_comment_is_refused),
    ("the_document_routes_are_behind_the_token", the_document_routes_are_behind_the_token),
    ("the_document_page_carries_no_external_resource", the_document_page_carries_no_external_resource),
    // DocumentRequestValidationTests
    ("accepts_a_well_formed_document_request", accepts_a_well_formed_document_request),
    ("rejects_malformed_document_requests", rejects_malformed_document_requests),
    ("neither_flag_is_a_usage_error", neither_flag_is_a_usage_error),
    // DocumentBlockRangeTests
    ("the_payload_carries_blocks_that_tile_the_document", the_payload_carries_blocks_that_tile_the_document),
    ("a_comment_on_any_line_of_a_paragraph_is_accepted", a_comment_on_any_line_of_a_paragraph_is_accepted),
    ("a_line_no_block_covers_is_still_refused", a_line_no_block_covers_is_still_refused),
    // CodeBlockLanguageTests
    ("the_fence_language_reaches_the_block", the_fence_language_reaches_the_block),
    ("an_unknown_language_is_passed_through_not_guessed", an_unknown_language_is_passed_through_not_guessed),
    ("a_bare_fence_claims_no_language", a_bare_fence_claims_no_language),
    ("source_html_in_a_code_block_is_still_escaped", source_html_in_a_code_block_is_still_escaped),
    ("prose_around_a_code_block_is_untouched", prose_around_a_code_block_is_untouched),
    // PageSurfaceTests
    ("every_helper_the_page_needs_is_defined", every_helper_the_page_needs_is_defined),
    ("no_helper_is_defined_twice", no_helper_is_defined_twice),
    ("the_page_reaches_the_areas_the_markup_declares", the_page_reaches_the_areas_the_markup_declares),
    ("the_boot_config_names_the_endpoints_the_routes_serve", the_boot_config_names_the_endpoints_the_routes_serve),
    ("the_page_never_shadows_a_vendored_global", the_page_never_shadows_a_vendored_global),
    ("the_page_carries_no_stray_control_characters", the_page_carries_no_stray_control_characters),
    ("the_page_declares_no_top_level_binding_twice", the_page_declares_no_top_level_binding_twice),
    // DiffRowContentTests
    ("every_highlighted_row_shows_its_own_line", every_highlighted_row_shows_its_own_line),
    (
        "the_page_reports_a_closing_window_and_never_cancels_from_pagehide",
        the_page_reports_a_closing_window_and_never_cancels_from_pagehide,
    ),
];

const REVIEW_ID: &str = "0123456789abcdef0123456789abcdef";
const FONT_NAMES: [&str; 5] = [
    "jetbrains-mono-400.woff2",
    "jetbrains-mono-700.woff2",
    "inter-latin-var.woff2",
    "source-serif-4-latin-var.woff2",
    "nunito-latin-var.woff2",
];
/// The reviewer's close grace period is a fixed five seconds, so the decision
/// tests wait it out for real.
const GRACE: f64 = 5.0;
const EXTERNAL_RESOURCE: &str = r#"<(script|link|img|iframe)[^>]+(src|href)="(https?:)?//"#;

fn root_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn review_request(root: &str, overrides: Value) -> Value {
    let mut payload = json!({
        "version": PROTOCOL_VERSION,
        "review_id": REVIEW_ID,
        "criterion": "ship it",
        "repositories": [{"root": root}],
    });
    if let Value::Object(map) = overrides {
        for (key, value) in map {
            payload[key] = value;
        }
    }
    payload
}

fn unique() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("{nanos}-{}", COUNTER.fetch_add(1, Ordering::SeqCst))
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("bad pattern {pattern}: {e}"))
}

fn matches(pattern: &str, text: &str) -> bool {
    regex(pattern).is_match(text).expect("regex match")
}

/// Every first capture group of `pattern` in `text`.
fn find_all(pattern: &str, text: &str) -> Vec<String> {
    regex(pattern)
        .captures_iter(text)
        .map(|c| c.expect("regex match").get(1).expect("group 1").as_str().to_string())
        .collect()
}

fn duplicates(names: &[String]) -> BTreeSet<String> {
    names.iter().filter(|n| names.iter().filter(|m| m == n).count() > 1).cloned().collect()
}

fn at<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or_else(|| panic!("{key} is missing from {value}"))
}

/// The (type or text, old_line, new_line) of every non-meta row.
fn row_triples(lines: &Value, first: &str) -> Vec<(String, Value, Value)> {
    lines
        .as_array()
        .expect("lines")
        .iter()
        .filter(|row| row["type"] != "meta")
        .map(|row| {
            (
                at(row, first).as_str().expect("a string").to_string(),
                at(row, "old_line").clone(),
                at(row, "new_line").clone(),
            )
        })
        .collect()
}

fn triple(first: &str, old: Value, new: Value) -> (String, Value, Value) {
    (first.to_string(), old, new)
}

/// Sends a request and ignores whatever comes back, or fails to.
fn try_post(url: &str, payload: &Value, timeout: f64) {
    let (address, target) = split_url(url);
    let body = payload.to_string();
    let Ok(mut stream) = TcpStream::connect(&address) else { return };
    let _ = stream.set_read_timeout(Some(Duration::from_secs_f64(timeout)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs_f64(timeout)));
    let head = format!(
        "POST {target} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    if stream.write_all(head.as_bytes()).is_ok() {
        let mut sink = Vec::new();
        let _ = stream.read_to_end(&mut sink);
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|candidate| {
        use std::os::unix::fs::PermissionsExt;
        candidate.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    })
}

/// A child that is killed and reaped when dropped.
struct Guard(Option<Child>);

impl Guard {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("the process has been collected")
    }

    fn running(&mut self) -> bool {
        self.child().try_wait().expect("poll").is_none()
    }

    /// Waits up to `timeout` seconds; a process still running then fails the test.
    fn finish(&mut self, timeout: f64) -> Run {
        let child = self.0.take().expect("the process has been collected");
        let started = Instant::now();
        let run = finish(child, timeout.ceil() as u64);
        assert!(started.elapsed().as_secs_f64() < timeout, "the process did not finish within {timeout}s: {run:?}");
        run
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A running `senpai html-review` process and the page it serves.
struct Review {
    process: Guard,
    url: String,
    url_file: PathBuf,
    api: &'static str,
    base_url: String,
    token: String,
    payload: Option<Value>,
    finished: Option<Run>,
}

impl Review {
    fn new(process: Guard, url: String, url_file: PathBuf, api: &'static str) -> Self {
        let (address, target) = split_url(&url);
        Review {
            process,
            base_url: format!("http://{address}"),
            token: target.trim_matches('/').to_string(),
            url,
            url_file,
            api,
            payload: None,
            finished: None,
        }
    }

    fn payload(&mut self) -> Value {
        if self.payload.is_none() {
            let response = get(&format!("{}{}", self.url, self.api));
            assert_eq!(200, response.status, "the payload route answered {}", response.status);
            self.payload = Some(response.json());
        }
        self.payload.clone().expect("payload")
    }

    fn still_open(&mut self, seconds: f64) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        while Instant::now() < deadline {
            if !self.process.running() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.process.running()
    }

    /// Exit code, stdout and stderr.
    fn finish(&mut self, timeout: f64) -> &Run {
        if self.finished.is_none() {
            self.finished = Some(self.process.finish(timeout));
        }
        self.finished.as_ref().expect("finished")
    }

    fn result(&mut self, timeout: f64) -> Value {
        let run = self.finish(timeout);
        if run.code != EXIT_OK || run.stdout.lines().count() != 1 {
            panic!("expected one result line, got exit {}: {:?} {:?}", run.code, run.stdout, run.stderr);
        }
        run.json()
    }
}

/// A temporary Git repository with one committed file and two changes.
struct Case {
    kept: RefCell<Vec<Review>>,
    _tmp: TempDir,
    base: PathBuf,
    root: PathBuf,
    doc: PathBuf,
}

impl Case {
    fn new() -> Self {
        let tmp = TempDir::new();
        let base = tmp.path().to_path_buf();
        let root = make_repo(&base, "repo");
        write(&root.join("a.py"), "print('one')\nprint('two')\n");
        write(&root.join("untracked.txt"), "new file\n");
        let doc = root.join("docs/specs/2026-09-27-topic.md");
        Case { kept: RefCell::new(Vec::new()), _tmp: tmp, base, root, doc }
    }

    /// DocumentCase: a repository with a specification a human reviews in the browser.
    fn document() -> Self {
        let case = Case::new();
        write(
            &case.doc,
            "# Topic\n\n## Problem\n\nSomething is **wrong** with `parse`.\n\n- one goal\n- [ ] an unchecked task\n- [x] a checked task\n",
        );
        case
    }

    fn root_str(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }

    fn doc_str(&self) -> String {
        self.doc.to_string_lossy().into_owned()
    }

    /// Keeps a review alive until the test ends.
    fn keep(&self, review: Review) {
        self.kept.borrow_mut().push(review);
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.root, args)
    }

    fn request(&self) -> Value {
        review_request(&self.root_str(), json!({}))
    }

    fn reviewer_env(&self, command: &mut Command, browser: bool, extra: &[(&str, String)]) {
        for name in [NO_BROWSER_ENV, URL_FILE_ENV, LIFETIME_ENV, IDLE_ENV] {
            command.env_remove(name);
        }
        if !browser {
            command.env(NO_BROWSER_ENV, "1");
        }
        for (key, value) in extra {
            command.env(key, value);
        }
    }

    fn session(&self) -> Review {
        self.session_with(None, "--review", false, &[])
    }

    /// A served review the test can talk to over HTTP, with no browser unless asked.
    fn session_with(&self, request: Option<Value>, flag: &str, browser: bool, env: &[(&str, String)]) -> Review {
        let url_file = self.base.join(format!("review-url-{}", unique()));
        let request = request.unwrap_or_else(|| self.request());
        let mut command = reviewer([flag.to_string(), request.to_string()]);
        self.reviewer_env(&mut command, browser, env);
        command.env(URL_FILE_ENV, &url_file);
        let mut process = Guard(Some(spawn(&mut command)));
        let deadline = Instant::now() + Duration::from_secs(30);
        let text = loop {
            if !process.running() {
                let run = process.finish(30.0);
                panic!(
                    "the reviewer exited {} before it served a page: {:?} {:?}",
                    run.code, run.stdout, run.stderr
                );
            }
            let text = std::fs::read_to_string(&url_file).unwrap_or_default();
            if text.ends_with('\n') {
                break text;
            }
            assert!(Instant::now() < deadline, "the reviewer never published its page");
            std::thread::sleep(Duration::from_millis(20));
        };
        let api = if flag == "--review-doc" { "api/doc" } else { "api/review" };
        Review::new(process, text.trim().to_string(), url_file, api)
    }

    fn file(&self, payload: &Value, path: &str) -> Value {
        for repository in payload["repositories"].as_array().expect("repositories") {
            for item in repository["files"].as_array().expect("files") {
                if item["path"] == path {
                    return item.clone();
                }
            }
        }
        panic!("{path} is not in the payload");
    }

    fn run_reviewer(&self, args: &[&str], stdin: &str, env: &[(&str, String)]) -> Run {
        let mut command = reviewer(args);
        self.reviewer_env(&mut command, false, env);
        run_with(&mut command, Some(stdin.as_bytes()), 60)
    }

    fn fingerprint(&self, paths: &[&str]) -> String {
        let root = self.root_str();
        let mut args = vec!["preflight", "--criterion", "ship it", "--repo", &root];
        let joined = paths.join(",");
        if !paths.is_empty() {
            args.extend(["--paths", &joined]);
        }
        let mut command = senpai(&args);
        self.reviewer_env(&mut command, false, &[]);
        let completed = run_with(&mut command, Some(b""), 60);
        assert_eq!(EXIT_OK, completed.code, "{}", completed.stderr);
        completed.json()["fingerprint"].as_str().expect("a fingerprint").to_string()
    }

    // DocumentCase

    fn document_request(&self) -> Value {
        json!({
            "version": PROTOCOL_VERSION,
            "review_id": "doc-1",
            "criterion": "Fix the parser",
            "path": self.doc_str(),
        })
    }

    fn document_session(&self) -> Review {
        self.session_with(Some(self.document_request()), "--review-doc", false, &[])
    }

    // PageSurfaceTests

    fn page_script(&self) -> String {
        let session = self.session();
        let response = get(&session.url);
        assert_eq!(200, response.status);
        let page = response.text();
        let start = page.find("<script>\n\"use strict\";").expect("the page script");
        let end = start + page[start..].find("</script>").expect("the end of the page script");
        self.keep(session);
        page[start..end].to_string()
    }
}

// ---------------------------------------------------------------------------
// PayloadLineNumberingTests: real git output, so the numbering the page
// renders is git's own.
// ---------------------------------------------------------------------------

fn payload_rows(case: &Case, path: &str) -> Value {
    let mut session = case.session();
    let payload = session.payload();
    case.keep(session);
    case.file(&payload, path)["lines"].clone()
}

fn a_modified_file_numbers_every_line_after_a_form_feed() {
    let case = Case::new();
    write(&case.root.join("a.py"), "print('one')\npage\x0cbreak\nprint('three')\n");
    let lines = payload_rows(&case, "a.py");
    assert_eq!(
        vec![triple("context", json!(1), json!(1)), triple("add", Value::Null, json!(2)), triple("add", Value::Null, json!(3))],
        row_triples(&lines, "type"),
    );
    let rows: Vec<&Value> = lines.as_array().unwrap().iter().filter(|r| r["type"] != "meta").collect();
    assert_eq!("page\x0cbreak", rows[1]["text"]);
}

fn an_untracked_file_numbering_counts_its_real_lines() {
    let case = Case::new();
    write(&case.root.join("untracked.txt"), "form\x0cfeed\nsecond\n");
    let mut session = case.session();
    let item = case.file(&session.payload(), "untracked.txt");
    let lines = &item["lines"];
    assert_eq!(vec![triple("add", Value::Null, json!(1)), triple("add", Value::Null, json!(2))], row_triples(lines, "type"));
    assert_eq!((json!(2), json!(0)), (item["added"].clone(), item["removed"].clone()));
    let rows: Vec<&Value> = lines.as_array().unwrap().iter().filter(|r| r["type"] != "meta").collect();
    assert_eq!("form\x0cfeed", rows[0]["text"], "the hunk count and the row agree on one line");
}

// ---------------------------------------------------------------------------
// GuidedPayloadTests
// ---------------------------------------------------------------------------

fn guide_resolves_captured_scope_without_changing_actual_diff() {
    let case = Case::new();
    let mut plain_session = case.session();
    let plain = plain_session.payload();
    let root = case.root_str();
    let value = json!({
        "version": 1,
        "fingerprint": case.fingerprint(&[]),
        "summary": "<script>literal text</script>", "before": "One printed line", "after": "Two printed lines",
        "chapters": [{"title": "Core output", "summary": "Print the second value",
                      "files": [{"repository": root, "path": "a.py"}],
                      "steps": [{"title": "print", "note": "Second value", "repository": root, "path": "a.py", "lines": [1, 2]}]}],
    });
    let mut guided_session = case.session_with(Some(review_request(&root, json!({"guide": value}))), "--review", false, &[]);
    let guided = guided_session.payload();
    assert_eq!(plain["repositories"], guided["repositories"]);
    assert_eq!(plain["totals"], guided["totals"]);
    let indices: Vec<Value> =
        guided["guide"]["chapters"].as_array().expect("chapters").iter().map(|c| at(c, "file_indices").clone()).collect();
    assert_eq!(vec![json!([0]), json!([1])], indices);
    assert_eq!(
        json!([{"title": "print", "note": "Second value", "file_index": 0, "lines": [1, 2]}]),
        guided["guide"]["chapters"][0]["steps"]
    );
    assert_eq!(value["summary"], guided["guide"]["summary"]);
    write(&case.root.join("a.py"), "changed again\n");
    let request = review_request(&root, json!({"guide": value})).to_string();
    let stale = case.run_reviewer(&["--review", &request], "", &[]);
    assert_eq!(EXIT_ERROR, stale.code);
    assert_eq!("", stale.stdout);
    assert!(stale.stderr.contains("stale"), "{}", stale.stderr);
}

// ---------------------------------------------------------------------------
// RequestValidationTests: the request, exactly as RDG validates it
// ---------------------------------------------------------------------------

fn rejects_what_rdg_rejects() {
    let case = Case::new();
    let root = case.root_str();
    let cases: Vec<(&str, Value)> = vec![
        ("relative root", json!({"repositories": [{"root": "repo"}]})),
        ("parent traversal in root", json!({"repositories": [{"root": "/tmp/../etc"}]})),
        ("unknown protocol version", json!({"version": "2"})),
        ("whitespace review id", json!({"review_id": "   "})),
        ("whitespace criterion", json!({"criterion": "\t\n "})),
        ("no repositories", json!({"repositories": []})),
        ("duplicate roots", json!({"repositories": [{"root": root}, {"root": root}]})),
        ("empty scope list", json!({"repositories": [{"root": root, "paths": []}]})),
        ("scope with parent traversal", json!({"repositories": [{"root": root, "paths": ["src/../a.py"]}]})),
        ("absolute scope", json!({"repositories": [{"root": root, "paths": ["/a.py"]}]})),
    ];
    for (label, override_) in cases {
        let mut payload = json!({"version": "1", "review_id": REVIEW_ID, "criterion": "ship it"});
        for (key, value) in override_.as_object().unwrap() {
            payload[key] = value.clone();
        }
        let result = case.run_reviewer(&["--review", &payload.to_string()], "", &[]);
        assert_eq!(EXIT_ERROR, result.code, "case={label}: {}", result.stderr);
        assert_eq!("", result.stdout, "case={label}: a rejected request must print no result line");
        assert!(result.stderr.contains(REVIEWER_PRODUCER), "case={label}: {}", result.stderr);
    }
}

fn rejects_a_request_that_is_not_json() {
    let case = Case::new();
    let result = case.run_reviewer(&["--review", "not json"], "", &[]);
    assert_eq!(EXIT_ERROR, result.code);
    assert!(result.stderr.contains(REVIEWER_PRODUCER), "{}", result.stderr);
}

fn requires_a_request() {
    let case = Case::new();
    let result = case.run_reviewer(&[], "", &[]);
    assert_eq!(EXIT_ERROR, result.code);
    assert!(result.stderr.contains("--review"), "{}", result.stderr);
}

fn reads_the_request_from_stdin_when_the_value_is_absent() {
    let case = Case::new();
    let payload = json!({
        "version": "1", "review_id": REVIEW_ID, "criterion": " ",
        "repositories": [{"root": case.root_str()}],
    })
    .to_string();
    let result = case.run_reviewer(&["--review"], &payload, &[]);
    assert_eq!(EXIT_ERROR, result.code);
    assert!(result.stderr.contains("criterion must not be empty"), "{}", result.stderr);
}

fn fails_closed_when_a_repository_is_not_a_worktree() {
    let case = Case::new();
    let request = review_request(&case.base.join("nowhere").to_string_lossy(), json!({})).to_string();
    let result = case.run_reviewer(&["--review", &request], "", &[]);
    assert_eq!(EXIT_ERROR, result.code, "{}", result.stderr);
    assert_eq!("", result.stdout);
    assert!(result.stderr.contains(REVIEWER_PRODUCER), "{}", result.stderr);
}

// ---------------------------------------------------------------------------
// ServerRouteTests: the server
// ---------------------------------------------------------------------------

fn serves_the_page_with_a_locked_down_policy() {
    let case = Case::new();
    let session = case.session();
    let response = get(&session.url);
    assert_eq!(200, response.status);
    let page = response.text();
    let policy = response.header("Content-Security-Policy").expect("a Content-Security-Policy").to_string();
    assert!(policy.contains("default-src 'none'"), "{policy}");
    assert!(policy.contains("style-src 'unsafe-inline'"), "{policy}");
    // Same-origin vendored scripts are allowed now; nothing else is, and no
    // directive may be declared twice (the last one silently wins).
    assert!(policy.contains("script-src 'self' 'unsafe-inline'"), "{policy}");
    for directive in ["default-src", "style-src", "script-src", "connect-src", "font-src"] {
        assert_eq!(1, policy.matches(&format!("{directive} ")).count(), "{directive} must be declared once");
    }
    assert!(!policy.contains("http://"), "{policy}");
    assert!(!policy.contains("https://"), "{policy}");
    assert_eq!(Some("nosniff"), response.header("X-Content-Type-Options"));
    assert!(response.header("Access-Control-Allow-Origin").is_none());
    assert!(page.contains(&session.token), "the page needs its own token to reach its routes");
    assert!(!matches(EXTERNAL_RESOURCE, &page), "the page loads an external resource");
    assert!(policy.contains("font-src 'self'"), "{policy}");
    assert!(!page.contains("prefers-color-scheme: light"), "light is the bare :root default, so only dark is a media query");
    assert!(page.contains("prefers-color-scheme: dark"), "the dark theme is kept");
}

fn every_route_is_behind_the_token() {
    let case = Case::new();
    let mut session = case.session();
    let base_name = case.base.file_name().unwrap().to_string_lossy().into_owned();
    for path in ["/".to_string(), format!("/{}x/", session.token), "/api/review".into(), format!("/wrong-token{base_name}/api/review")] {
        let response = get(&format!("{}{path}", session.base_url));
        assert_eq!(404, response.status, "path={path}");
    }
    let response = post(&format!("{}/api/decision", session.base_url), &json!({"decision": "approve", "annotations": []}));
    assert_eq!(404, response.status, "a decision without the token must not be recorded");
    assert!(session.still_open(0.5));
}

fn serves_the_bundled_code_font() {
    // The code font ships with the reviewer and is served from the review's
    // own origin: no network, and no third party named in the CSP.
    let case = Case::new();
    let session = case.session();
    let fonts = root_dir().join("assets/fonts");
    for name in FONT_NAMES {
        assert!(fonts.join(name).is_file(), "{name} must ship with the reviewer");
    }
    for name in FONT_NAMES {
        let response = get(&format!("{}font/{name}", session.url));
        assert_eq!(200, response.status, "name={name}");
        assert_eq!(Some("font/woff2"), response.header("Content-Type"), "name={name}");
        assert!(std::fs::read(fonts.join(name)).expect("the font") == response.body, "name={name}: the bytes differ");
    }
    for path in ["/font/jetbrains-mono-400.woff2".to_string(), format!("/wrong{}/font/jetbrains-mono-400.woff2", session.token)] {
        let response = get(&format!("{}{path}", session.base_url));
        assert_eq!(404, response.status, "path={path}: a font must not be served without the review token");
    }
    let response = get(&format!("{}font/jetbrains-mono-400.woff2.map", session.url));
    assert_eq!(404, response.status, "the font route serves bundled files only");
}

fn the_page_uses_the_bundled_font_and_nerd_glyphs() {
    let case = Case::new();
    let session = case.session();
    let page = get(&session.url).text();
    assert!(page.contains("font-family:\"JetBrains Mono\""));
    assert!(page.contains("src:url(\"font/jetbrains-mono-400.woff2\")"));
    assert!(page.contains("src:url(\"font/nunito-latin-var.woff2\")"));
    assert!(page.contains("--nerd:"));
    assert!(page.contains("FILE_GLYPHS"), "file icons come from the nerd glyph map");
}

fn serves_no_filesystem_and_no_directory_listing() {
    let case = Case::new();
    let session = case.session();
    let token = &session.token;
    for path in [format!("/{token}/.."), format!("/{token}/../../etc/passwd"), format!("/{token}/scripts/")] {
        let response = get(&format!("{}{path}", session.base_url));
        assert_eq!(404, response.status, "path={path}");
    }
}

fn answers_the_wrong_method() {
    let case = Case::new();
    let session = case.session();
    let response = post(&format!("{}api/review", session.url), &json!({}));
    assert_eq!(405, response.status);
}

fn publishes_the_criterion_the_repositories_and_the_diff() {
    let case = Case::new();
    let session = case.session();
    let response = get(&format!("{}api/review", session.url));
    assert_eq!(200, response.status);
    assert!(response.header("Content-Type").expect("a Content-Type").contains("application/json"));
    let payload = response.json();
    assert_eq!("ship it", payload["criterion"]);
    assert_eq!(REVIEW_ID, payload["review_id"]);
    let roots: Vec<Value> = payload["repositories"].as_array().unwrap().iter().map(|r| at(r, "root").clone()).collect();
    assert_eq!(vec![json!(case.root_str())], roots);

    let changed = case.file(&payload, "a.py");
    assert_eq!("modified", changed["status"]);
    assert_eq!((json!(1), json!(0)), (changed["added"].clone(), changed["removed"].clone()));
    assert_eq!(
        vec![triple("print('one')", json!(1), json!(1)), triple("print('two')", Value::Null, json!(2))],
        row_triples(&changed["lines"], "text"),
    );

    let added = case.file(&payload, "untracked.txt");
    assert_eq!("added", added["status"]);
    assert_eq!(vec![triple("new file", Value::Null, json!(1))], row_triples(&added["lines"], "text"));
}

fn reports_a_scope_and_a_deletion() {
    let case = Case::new();
    case.git(&["rm", "-q", "other/b.txt"]);
    let root = case.root_str();
    let scoped = review_request(&root, json!({"repositories": [{"root": root, "paths": ["other"]}]}));
    let mut session = case.session_with(Some(scoped), "--review", false, &[]);
    let payload = session.payload();
    let paths: Vec<Value> = payload["repositories"].as_array().unwrap().iter().map(|r| at(r, "paths").clone()).collect();
    assert_eq!(vec![json!(["other"])], paths);
    let deleted = case.file(&payload, "other/b.txt");
    assert_eq!("deleted", deleted["status"]);
    assert_eq!((json!(0), json!(1)), (deleted["added"].clone(), deleted["removed"].clone()));
    assert_eq!(vec![triple("unchanged", json!(1), Value::Null)], row_triples(&deleted["lines"], "text"));
}

fn keeps_a_binary_file_uncommentable() {
    let case = Case::new();
    std::fs::write(case.root.join("logo.png"), b"\x89PNG\r\n\x1a\n\x00\x01\x02").expect("write");
    let mut session = case.session();
    let binary = case.file(&session.payload(), "logo.png");
    assert_eq!(json!(true), binary["binary"]);
    let rows: Vec<&Value> = binary["lines"].as_array().expect("lines").iter().filter(|l| l["type"] != "meta").collect();
    assert!(rows.is_empty(), "{rows:?}");
}

fn refuses_a_decision_the_client_would_reject() {
    let case = Case::new();
    let root = case.root_str();
    let cases: Vec<(&str, Value)> = vec![
        ("unknown decision", json!({"decision": "merge", "annotations": []})),
        ("changes without feedback", json!({"decision": "request-changes", "annotations": []})),
        (
            "changes with only blank text",
            json!({"decision": "request-changes", "annotations": [{"text": " ", "target": {"scope": "review"}}]}),
        ),
        ("cancelled with a comment", json!({"decision": "cancelled", "annotations": [{"text": "nope", "target": {"scope": "review"}}]})),
        ("malformed target", json!({"decision": "approve", "annotations": [{"text": "x", "target": {"scope": "line"}}]})),
        (
            "a line that is not in the diff",
            json!({
                "decision": "approve",
                "annotations": [{"text": "x", "target": {"scope": "line", "repository": root, "path": "a.py", "side": "new", "start_line": 900, "end_line": 900}}],
            }),
        ),
        (
            "a file that is not in the review",
            json!({
                "decision": "approve",
                "annotations": [{"text": "x", "target": {"scope": "line", "repository": root, "path": "ghost.py", "side": "new", "start_line": 1, "end_line": 1}}],
            }),
        ),
    ];
    let mut session = case.session();
    for (label, payload) in cases {
        let response = post(&format!("{}api/decision", session.url), &payload);
        assert_eq!(400, response.status, "case={label}: {}", response.text());
        assert!(response.json().get("error").is_some(), "case={label}: {}", response.text());
        assert!(session.still_open(0.2), "case={label}: a refused decision must leave the review open");
    }
}

fn refuses_a_decision_that_is_not_json() {
    let case = Case::new();
    let mut session = case.session();
    let response = post_bytes(&format!("{}api/decision", session.url), b"{not json");
    assert_eq!(400, response.status);
    assert!(session.still_open(0.2));
}

fn records_a_decision_once_and_stops() {
    let case = Case::new();
    let mut session = case.session();
    let response = post(&format!("{}api/decision", session.url), &json!({"decision": "cancelled", "annotations": []}));
    assert_eq!(204, response.status);
    try_post(&format!("{}api/decision", session.url), &json!({"decision": "approve", "annotations": []}), 2.0);
    assert_eq!(
        json!({"version": "1", "review_id": REVIEW_ID, "decision": "cancelled", "annotations": []}),
        session.result(30.0),
        "a decision is final",
    );
}

fn carries_a_line_comment_with_both_of_its_ends() {
    let case = Case::new();
    let mut session = case.session();
    let target = json!({
        "scope": "line", "repository": case.root_str(), "path": "a.py",
        "side": "new", "start_line": 1, "end_line": 2,
    });
    let response = post(
        &format!("{}api/decision", session.url),
        &json!({"decision": "request-changes", "annotations": [{"text": "guard this call", "target": target}]}),
    );
    assert_eq!(204, response.status);
    assert_eq!(
        json!({
            "version": "1", "review_id": REVIEW_ID, "decision": "request-changes",
            "annotations": [{"text": "guard this call", "target": target}],
        }),
        session.result(30.0),
    );
}

// ---------------------------------------------------------------------------
// DecisionTests: decisions, without a browser in the way
// ---------------------------------------------------------------------------

fn approve_with(case: &Case, annotations: Value) -> Value {
    let mut session = case.session();
    let response = post(&format!("{}api/decision", session.url), &json!({"decision": "approve", "annotations": annotations}));
    assert_eq!(204, response.status, "{}", response.text());
    let result = session.result(30.0);
    case.keep(session);
    result
}

fn page_number(session: &Review) -> u64 {
    let response = get(&session.url);
    assert_eq!(200, response.status);
    find_all(r#""page":(\d+)"#, &response.text()).first().expect("a page number").parse().expect("a number")
}

fn close_page(session: &Review, page: u64) -> u16 {
    post_bytes(&format!("{}api/closing?page={page}", session.url), b"").status
}

fn approve_carries_every_comment_as_a_line_target() {
    let case = Case::new();
    let annotation = json!({
        "text": "add a regression test",
        "target": {
            "scope": "line", "repository": case.root_str(), "path": "a.py",
            "side": "new", "start_line": 2, "end_line": 2,
        },
    });
    let result = approve_with(&case, json!([annotation]));
    assert_eq!("approve", result["decision"]);
    assert_eq!(json!([annotation]), result["annotations"]);
}

fn a_review_comment_is_a_scope_and_nothing_else() {
    let case = Case::new();
    let result = approve_with(&case, json!([{"text": "overall", "target": {"scope": "review", "path": "a.py"}}]));
    assert_eq!(json!({"scope": "review"}), result["annotations"][0]["target"]);
}

fn a_stale_page_cannot_point_at_a_line_it_never_saw() {
    let case = Case::new();
    let mut session = case.session();
    let response = post(
        &format!("{}api/decision", session.url),
        &json!({
            "decision": "approve",
            "annotations": [{
                "text": "x",
                "target": {
                    "scope": "line", "repository": case.root_str(), "path": "a.py",
                    "side": "old", "start_line": 2, "end_line": 2,
                },
            }],
        }),
    );
    assert_eq!(400, response.status);
    assert!(response.json()["error"].as_str().expect("an error").contains("a.py"), "{}", response.text());
    assert!(session.still_open(0.2));
}

fn a_closed_window_cancels_once_the_grace_period_passes() {
    let case = Case::new();
    let mut session = case.session();
    let page = page_number(&session);
    assert_eq!(204, close_page(&session, page));
    let result = session.result(GRACE + 10.0);
    assert_eq!("cancelled", result["decision"]);
    assert_eq!(json!([]), result["annotations"]);
}

fn reloading_the_page_keeps_the_review_alive() {
    // Chromium requests the reloaded page before the old page reports that it is closing.
    let case = Case::new();
    let mut session = case.session();
    let old = page_number(&session);
    page_number(&session);
    assert_eq!(204, close_page(&session, old));
    assert!(session.still_open(GRACE + 0.5), "a reload must not withdraw the review");
    let response = post(&format!("{}api/decision", session.url), &json!({"decision": "approve", "annotations": []}));
    assert_eq!(204, response.status);
    assert_eq!("approve", session.result(30.0)["decision"]);
}

fn a_closing_report_that_arrives_before_the_reloaded_page_is_requested() {
    let case = Case::new();
    let mut session = case.session();
    let old = page_number(&session);
    assert_eq!(204, close_page(&session, old));
    page_number(&session);
    assert!(session.still_open(GRACE + 0.5), "a reload must not withdraw the review");
    let response = post(&format!("{}api/decision", session.url), &json!({"decision": "approve", "annotations": []}));
    assert_eq!(204, response.status);
    assert_eq!("approve", session.result(30.0)["decision"]);
}

fn closing_is_behind_the_token_and_needs_post_and_a_page_number() {
    let case = Case::new();
    let mut session = case.session();
    let page = page_number(&session);
    let response = post_bytes(&format!("{}/api/closing?page={page}", session.base_url), b"");
    assert_eq!(404, response.status);
    let response = get(&format!("{}api/closing?page={page}", session.url));
    assert_eq!(405, response.status);
    let long = format!("?page={}", "9".repeat(5000));
    for query in ["", "?page=", "?page=x", "?page=-1", "?page=0", long.as_str()] {
        let response = post_bytes(&format!("{}api/closing{query}", session.url), b"");
        assert_eq!(400, response.status, "query={query}");
    }
    assert!(session.still_open(GRACE + 0.5), "none of these requests may withdraw the review");
}

fn an_explicit_cancel_still_ends_the_review_at_once() {
    let case = Case::new();
    let mut session = case.session();
    let page = page_number(&session);
    assert_eq!(204, close_page(&session, page));
    let started = Instant::now();
    let response = post(&format!("{}api/decision", session.url), &json!({"decision": "cancelled", "annotations": []}));
    assert_eq!(204, response.status);
    assert_eq!("cancelled", session.result(30.0)["decision"]);
    assert!(started.elapsed().as_secs_f64() < GRACE - 1.0, "a cancel must not wait for the grace period");
}

// ---------------------------------------------------------------------------
// ServeAndWaitTests: opening the page
// ---------------------------------------------------------------------------

/// A PATH whose `open` and `xdg-open` only record the URL they were given.
fn fake_opener(case: &Case, exit_code: i32) -> (Vec<(&'static str, String)>, PathBuf) {
    let bin_dir = case.base.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    let opened = case.base.join("opened");
    for name in ["open", "xdg-open"] {
        script(&bin_dir.join(name), &format!("#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\nexit {exit_code}\n", opened.display()));
    }
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());
    (vec![("PATH", path)], opened)
}

/// A PATH that holds git and nothing that could open a browser.
fn without_opener(case: &Case) -> Vec<(&'static str, String)> {
    let bin_dir = case.base.join("git-only");
    std::fs::create_dir(&bin_dir).expect("git-only dir");
    std::os::unix::fs::symlink(which("git").expect("git on PATH"), bin_dir.join("git")).expect("link git");
    vec![("PATH", bin_dir.to_string_lossy().into_owned())]
}

fn wait_written(path: &Path, seconds: f64) -> String {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if text.ends_with('\n') {
                return text;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{} was never written", path.file_name().unwrap().to_string_lossy());
}

fn cancel(session: &mut Review) -> Value {
    let response = post(&format!("{}api/decision", session.url), &json!({"decision": "cancelled", "annotations": []}));
    assert_eq!(204, response.status);
    session.result(30.0)
}

fn opens_the_page_when_a_browser_is_allowed() {
    let case = Case::new();
    let (env, opened) = fake_opener(&case, 0);
    let mut session = case.session_with(None, "--review", true, &env);
    let text = wait_written(&opened, 10.0);
    assert_eq!(vec![session.url.as_str()], text.lines().collect::<Vec<_>>());
    assert_eq!("cancelled", cancel(&mut session)["decision"]);
}

fn browser_disabled_by_environment() {
    let case = Case::new();
    let cases: [(Option<&str>, bool); 6] =
        [(None, true), (Some(""), true), (Some("0"), true), (Some("false"), true), (Some("1"), false), (Some("yes"), false)];
    for (value, expected) in cases {
        let (mut env, opened) = fake_opener(&case, 0);
        let _ = std::fs::remove_file(&opened);
        if let Some(value) = value {
            env.push((NO_BROWSER_ENV, value.to_string()));
        }
        let mut session = case.session_with(None, "--review", true, &env);
        if expected {
            let text = wait_written(&opened, 10.0);
            assert_eq!(vec![session.url.as_str()], text.lines().collect::<Vec<_>>(), "value={value:?}");
        }
        cancel(&mut session);
        if !expected {
            std::thread::sleep(Duration::from_millis(200));
            assert!(!opened.exists(), "value={value:?}: a disabled browser must not be opened");
        }
        case.keep(session);
    }
}

fn prints_the_url_so_a_human_can_open_the_page_by_hand() {
    let case = Case::new();
    let mut session = case.session();
    cancel(&mut session);
    let url = session.url.clone();
    let stderr = &session.finish(30.0).stderr;
    assert!(stderr.contains(&url), "{stderr}");
}

fn publishes_and_then_withdraws_the_url() {
    let case = Case::new();
    let mut session = case.session();
    cancel(&mut session);
    assert!(!session.url_file.exists(), "a token that no longer routes must not be left on disk");
}

fn a_browser_that_cannot_open_keeps_the_review_alive() {
    let case = Case::new();
    let env = without_opener(&case);
    let mut session = case.session_with(None, "--review", true, &env);
    assert!(session.still_open(0.5));
    assert_eq!("cancelled", cancel(&mut session)["decision"]);
    let stderr = &session.finish(30.0).stderr;
    assert!(stderr.contains("unable to open a browser"), "{stderr}");
}

fn an_opener_that_fails_keeps_the_review_alive() {
    let case = Case::new();
    let (env, opened) = fake_opener(&case, 1);
    let mut session = case.session_with(None, "--review", true, &env);
    wait_written(&opened, 10.0);
    assert!(session.still_open(0.5));
    assert_eq!("cancelled", cancel(&mut session)["decision"]);
}

// ---------------------------------------------------------------------------
// GateIntegrationTests: the real gate runs the real reviewer; this test plays
// the human.
// ---------------------------------------------------------------------------

fn gate_env(case: &Case, command: &mut Command) {
    for name in [LAUNCHER_ENV, TIMEOUT_ENV, NO_BROWSER_ENV, URL_FILE_ENV] {
        command.env_remove(name);
    }
    command
        .env(LAUNCHER_ENV, reviewer_launcher())
        .env(TIMEOUT_ENV, "60")
        .env(NO_BROWSER_ENV, "1")
        .env(URL_FILE_ENV, case.base.join("review-url"));
}

fn start_review(case: &Case) -> Guard {
    let root = case.root_str();
    let mut command = senpai(["review", "--criterion", "ship it", "--repo", &root]);
    gate_env(case, &mut command);
    Guard(Some(spawn(&mut command)))
}

/// The URL of the page the reviewer is actually serving.
fn review_page(case: &Case, process: &mut Guard, timeout: f64) -> String {
    let published = case.base.join("review-url");
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    while Instant::now() < deadline {
        if let Some(status) = process.child().try_wait().expect("poll") {
            panic!("the review ended before it served a page: exit {status}");
        }
        if let Ok(text) = std::fs::read_to_string(&published) {
            let url = text.trim();
            if !url.is_empty() {
                return url.to_string();
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the reviewer never published a review page");
}

fn gate_finish(process: &mut Guard) -> (i32, String) {
    let run = process.finish(60.0);
    (run.code, run.stdout)
}

/// Runs the gate to completion with one decision, the way a human would.
fn gate_review(case: &Case, decision: Value) -> (i32, Value) {
    let mut process = start_review(case);
    let page = review_page(case, &mut process, 30.0);
    let response = post(&format!("{page}api/decision"), &decision);
    assert_eq!(204, response.status, "{}", response.text());
    let (code, stdout) = gate_finish(&mut process);
    assert_eq!(1, stdout.lines().count(), "one result line, got {stdout:?}");
    (code, serde_json::from_str(&stdout).expect("a JSON result"))
}

fn verify(case: &Case) -> Run {
    let root = case.root_str();
    let mut command = senpai(["verify", "--repo", &root]);
    gate_env(case, &mut command);
    run_with(&mut command, Some(b""), 60)
}

fn an_approval_makes_the_gate_pass() {
    let case = Case::new();
    let (code, result) = gate_review(
        &case,
        json!({
            "decision": "approve",
            "annotations": [{
                "text": "the nit I am knowingly keeping",
                "target": {
                    "scope": "line", "repository": case.root_str(), "path": "a.py",
                    "side": "new", "start_line": 2, "end_line": 2,
                },
            }],
        }),
    );
    assert_eq!(EXIT_OK, code);
    assert_eq!("approve", result["decision"]);
    assert_eq!(1, result["annotations"].as_array().expect("annotations").len());
    let verified = verify(&case);
    assert_eq!(EXIT_OK, verified.code, "{}{}", verified.stdout, verified.stderr);
    assert_eq!(json!(true), verified.json()["approved"]);
}

fn an_approval_without_comments_makes_the_gate_pass() {
    let case = Case::new();
    let (code, result) = gate_review(&case, json!({"decision": "approve", "annotations": []}));
    assert_eq!(EXIT_OK, code);
    assert_eq!(json!([]), result["annotations"]);
    assert_eq!(EXIT_OK, verify(&case).code);
}

fn requested_changes_leaves_the_gate_failing() {
    let case = Case::new();
    let (code, result) = gate_review(
        &case,
        json!({"decision": "request-changes", "annotations": [{"text": "this needs a test", "target": {"scope": "review"}}]}),
    );
    assert_eq!(EXIT_NOT_APPROVED, code);
    assert_eq!("request-changes", result["decision"]);
    let verified = verify(&case);
    assert_eq!(EXIT_NOT_APPROVED, verified.code, "{}", verified.stdout);
    assert_eq!(json!(false), verified.json()["approved"]);
}

fn open_line_comments_come_back_as_feedback() {
    // An unresolved comment is the work the author still owes, so sending it
    // back must never be blocked and must never be dropped.
    let case = Case::new();
    let (code, result) = gate_review(
        &case,
        json!({
            "decision": "request-changes",
            "annotations": [{
                "text": "rename this, it shadows the module name",
                "target": {
                    "scope": "line", "repository": case.root_str(), "path": "a.py",
                    "side": "new", "start_line": 2, "end_line": 2,
                },
            }],
        }),
    );
    assert_eq!(EXIT_NOT_APPROVED, code);
    assert_eq!("request-changes", result["decision"]);
    assert_eq!(1, result["annotations"].as_array().expect("annotations").len(), "the open comment must reach the agent");
    assert_eq!(json!(2), result["annotations"][0]["target"]["start_line"]);
    assert_eq!(EXIT_NOT_APPROVED, verify(&case).code);
}

fn feedback_needs_at_least_one_comment() {
    let case = Case::new();
    let mut process = start_review(&case);
    let page = review_page(&case, &mut process, 30.0);
    let response = post(&format!("{page}api/decision"), &json!({"decision": "request-changes", "annotations": []}));
    assert_eq!(400, response.status, "{}", response.text());
    assert!(process.running(), "a refused decision must leave the review open");
    let response = post(&format!("{page}api/decision"), &json!({"decision": "cancelled", "annotations": []}));
    assert_eq!(204, response.status);
    gate_finish(&mut process);
}

fn a_closed_window_leaves_the_gate_failing() {
    let case = Case::new();
    let (code, result) = gate_review(&case, json!({"decision": "cancelled", "annotations": []}));
    assert_eq!(EXIT_NOT_APPROVED, code);
    assert_eq!("cancelled", result["decision"]);
    assert_eq!(json!([]), result["annotations"]);
    assert_eq!(EXIT_NOT_APPROVED, verify(&case).code);
}

fn a_change_under_the_reviewer_fails_the_gate_closed() {
    // The gate re-fingerprints after an approval: a reviewer that approved a
    // tree that then moved must not leave an approval behind.
    let case = Case::new();
    let mut process = start_review(&case);
    let page = review_page(&case, &mut process, 30.0);
    write(&case.root.join("a.py"), "print('one')\nprint('two')\nprint('three')\n");
    let response = post(&format!("{page}api/decision"), &json!({"decision": "approve", "annotations": []}));
    assert_eq!(204, response.status, "{}", response.text());
    let (code, stdout) = gate_finish(&mut process);
    assert_eq!(EXIT_ERROR, code, "{stdout}");
    assert_eq!("", stdout, "a state that moved under the reviewer must print no result");
    assert_eq!(EXIT_NOT_APPROVED, verify(&case).code);
}

fn a_refused_decision_leaves_the_review_open() {
    let case = Case::new();
    let mut process = start_review(&case);
    let page = review_page(&case, &mut process, 30.0);
    let response = post(&format!("{page}api/decision"), &json!({"decision": "nope", "annotations": []}));
    assert_eq!(400, response.status);
    assert!(process.running(), "a refused decision is not a decision");
    // The same page is still up, and a real decision still ends the review.
    let response = post(&format!("{page}api/decision"), &json!({"decision": "cancelled", "annotations": []}));
    assert_eq!(204, response.status);
    let (code, stdout) = gate_finish(&mut process);
    assert_eq!(EXIT_NOT_APPROVED, code);
    let result: Value = serde_json::from_str(&stdout).expect("a JSON result");
    assert_eq!("cancelled", result["decision"]);
}

fn the_published_url_is_gone_when_the_review_ends() {
    let case = Case::new();
    gate_review(&case, json!({"decision": "cancelled", "annotations": []}));
    assert!(!case.base.join("review-url").exists(), "a dead token must not be left on disk");
}

/// The reviewers this test's own client started, scoped to the client's
/// children so a review running elsewhere on the machine never matches.
fn reviewer_children(client: &mut Guard) -> Vec<i32> {
    let output = Command::new("pgrep").args(["-P", &client.child().id().to_string()]).output().expect("run pgrep");
    String::from_utf8_lossy(&output.stdout).split_whitespace().filter_map(|pid| pid.parse().ok()).collect()
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_for_exit(pids: &[i32], timeout: f64) -> Vec<i32> {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    while Instant::now() < deadline {
        if !pids.iter().any(|&pid| pid_alive(pid)) {
            return Vec::new();
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    pids.iter().copied().filter(|&pid| pid_alive(pid)).collect()
}

fn a_killed_client_does_not_leave_a_stale_reviewer() {
    // SIGKILL cannot be handled, so the reviewer has to notice on its own.
    let case = Case::new();
    let mut process = start_review(&case);
    review_page(&case, &mut process, 30.0);
    let children = reviewer_children(&mut process);
    assert!(!children.is_empty(), "the reviewer should be running while the page is up");
    let child = process.child();
    child.kill().expect("kill the client");
    child.wait().expect("reap the client");
    assert_eq!(Vec::<i32>::new(), wait_for_exit(&children, 20.0), "an orphaned reviewer must not outlive its client");
}

fn the_reviewer_expires_on_its_own_lifetime() {
    let case = Case::new();
    let request = case.request().to_string();
    let mut command = reviewer(["--review", &request]);
    gate_env(&case, &mut command);
    command.env(LIFETIME_ENV, "1").env(IDLE_ENV, "1");
    let completed = run_with(&mut command, None, 60);
    assert_eq!(EXIT_OK, completed.code, "{}", completed.stderr);
    let result = completed.json();
    assert_eq!("cancelled", result["decision"], "an expired review is never an approval");
    assert_eq!(json!([]), result["annotations"]);
}

fn junk_lifetime_values_fall_back_to_the_defaults() {
    let case = Case::new();
    let env = [(LIFETIME_ENV, "not a number".to_string()), (IDLE_ENV, "-5".to_string())];
    let mut session = case.session_with(None, "--review", false, &env);
    assert!(session.still_open(1.5), "junk limits must not end the review early");
}

// ---------------------------------------------------------------------------
// DocumentReviewTests
// ---------------------------------------------------------------------------

fn doc_decision(session: &Review, annotations: Value) -> Response {
    post(&format!("{}api/doc-decision", session.url), &json!({"decision": "request-changes", "annotations": annotations}))
}

fn renders_one_row_per_line_of_the_document() {
    let case = Case::document();
    let mut session = case.document_session();
    let source = read(&case.doc);
    let payload = session.payload();
    let rows = payload["rows"].as_array().expect("rows");
    assert_eq!(source.lines().count(), rows.len());
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(json!(index + 1), row["n"], "row n must be source line n");
    }
    assert_eq!("doc-1", payload["review_id"]);
}

fn inline_markup_is_rendered_and_source_html_is_escaped() {
    let case = Case::document();
    write(&case.doc, "# T\n\n**bold** and <script>x</script>\n");
    let mut session = case.document_session();
    let payload = session.payload();
    let rendered: Vec<&str> =
        payload["rows"].as_array().expect("rows").iter().map(|r| at(r, "html").as_str().expect("html")).collect();
    let rendered = rendered.join(" ");
    assert!(rendered.contains("<b>bold</b>"), "{rendered}");
    assert!(!rendered.contains("<script>"), "{rendered}");
    assert!(rendered.contains("&lt;script&gt;"), "{rendered}");
}

fn a_line_comment_comes_back_anchored_to_the_document() {
    let case = Case::document();
    let mut session = case.document_session();
    let response = doc_decision(
        &session,
        json!([{"text": "name the failure mode", "target": {"scope": "line", "path": case.doc_str(), "start_line": 5, "end_line": 5}}]),
    );
    assert_eq!(204, response.status, "{}", response.text());
    let result = session.result(30.0);
    let target = &result["annotations"][0]["target"];
    assert_eq!(json!(5), target["start_line"]);
    assert!(target.get("repository").is_none(), "a document has no repository");
    assert!(target.get("side").is_none(), "a document has no old side");
}

fn a_comment_cannot_name_a_line_the_document_does_not_have() {
    let case = Case::document();
    let mut session = case.document_session();
    let response = doc_decision(
        &session,
        json!([{"text": "invented", "target": {"scope": "line", "path": case.doc_str(), "start_line": 999, "end_line": 999}}]),
    );
    assert_eq!(400, response.status);
    assert!(response.json()["error"].as_str().expect("an error").contains("999"), "{}", response.text());
    assert!(session.still_open(0.3), "a refused decision must leave the review open");
}

fn a_diff_shaped_comment_is_refused() {
    let case = Case::document();
    let session = case.document_session();
    let response = doc_decision(
        &session,
        json!([{
            "text": "wrong shape",
            "target": {
                "scope": "line", "repository": case.root_str(), "path": "a.py",
                "side": "new", "start_line": 1, "end_line": 1,
            },
        }]),
    );
    assert_eq!(400, response.status, "a document review must not accept a diff annotation");
}

fn feedback_without_a_comment_is_refused() {
    let case = Case::document();
    let mut session = case.document_session();
    let response = doc_decision(&session, json!([]));
    assert_eq!(400, response.status);
    assert!(session.still_open(0.2));
}

fn the_document_routes_are_behind_the_token() {
    let case = Case::document();
    let session = case.document_session();
    for path in ["/api/doc", "/api/doc-decision"] {
        let response = get(&format!("{}{path}", session.base_url));
        assert_eq!(404, response.status, "path={path}");
        let response = post(&format!("{}{path}", session.base_url), &json!({"decision": "cancelled", "annotations": []}));
        assert_eq!(404, response.status, "path={path}");
    }
}

fn the_document_page_carries_no_external_resource() {
    let case = Case::document();
    let session = case.document_session();
    let response = get(&session.url);
    assert_eq!(200, response.status);
    let page = response.text();
    assert!(page.contains("\"mode\":\"document\""), "the page must know it is reviewing a document");
    assert!(!matches(EXTERNAL_RESOURCE, &page), "the page loads an external resource");
    assert!(response.header("Content-Security-Policy").expect("a Content-Security-Policy").contains("font-src 'self'"));
}

// ---------------------------------------------------------------------------
// DocumentRequestValidationTests
// ---------------------------------------------------------------------------

fn accepts_a_well_formed_document_request() {
    // A bounded lifetime, so an unattended run withdraws instead of waiting
    // for a decision nobody is there to make.
    let case = Case::document();
    let request = case.document_request().to_string();
    let completed = case.run_reviewer(&["--review-doc", &request], "", &[(LIFETIME_ENV, "1".to_string())]);
    assert_eq!(EXIT_OK, completed.code, "{}", completed.stderr);
    assert_eq!("cancelled", completed.json()["decision"]);
}

fn rejects_malformed_document_requests() {
    let case = Case::document();
    let version = PROTOCOL_VERSION;
    let path = case.doc_str();
    let missing = case.root.join("missing.md").to_string_lossy().into_owned();
    let bad_requests = [
        "not json".to_string(),
        json!({"review_id": "d", "criterion": "c", "path": path}).to_string(),
        json!({"version": "2", "review_id": "d", "criterion": "c", "path": path}).to_string(),
        json!({"version": version, "review_id": " ", "criterion": "c", "path": path}).to_string(),
        json!({"version": version, "review_id": "d", "criterion": "  ", "path": path}).to_string(),
        json!({"version": version, "review_id": "d", "criterion": "c"}).to_string(),
        json!({"version": version, "review_id": "d", "criterion": "c", "path": missing}).to_string(),
    ];
    for request in bad_requests {
        let completed = case.run_reviewer(&["--review-doc", &request], "", &[]);
        assert_eq!(EXIT_ERROR, completed.code, "request={request}");
        assert_eq!("", completed.stdout, "request={request}");
        assert!(!completed.stderr.trim().is_empty(), "request={request}: a refusal must say why");
    }
}

fn neither_flag_is_a_usage_error() {
    let case = Case::document();
    let completed = case.run_reviewer(&[], "", &[]);
    assert_eq!(EXIT_ERROR, completed.code);
    assert!(completed.stderr.contains("required"), "{}", completed.stderr);
}

// ---------------------------------------------------------------------------
// DocumentBlockRangeTests: a block renders once but spans several lines, so
// every line of it counts.
// ---------------------------------------------------------------------------

const PARAGRAPH: &str = "# Title\n\nfirst line of the paragraph\nsecond line of the paragraph\nthird line of the paragraph\n\n## Design\n\na second paragraph\n";

fn block_case() -> Case {
    let case = Case::document();
    write(&case.doc, PARAGRAPH);
    case
}

fn paragraph_range(case: &Case) -> (i64, i64) {
    let mut session = case.document_session();
    let payload = session.payload();
    case.keep(session);
    let block = payload["blocks"].as_array().expect("blocks").iter().find(|b| b["kind"] == "text").expect("a text block").clone();
    (block["start"].as_i64().expect("start"), block["end"].as_i64().expect("end"))
}

fn the_payload_carries_blocks_that_tile_the_document() {
    let case = block_case();
    let mut session = case.document_session();
    let mut covered = Vec::new();
    for block in session.payload()["blocks"].as_array().expect("blocks") {
        covered.extend(block["start"].as_i64().expect("start")..=block["end"].as_i64().expect("end"));
    }
    assert_eq!((1..=PARAGRAPH.lines().count() as i64).collect::<Vec<_>>(), covered);
}

fn a_comment_on_any_line_of_a_paragraph_is_accepted() {
    let case = block_case();
    let (start, end) = paragraph_range(&case);
    assert_eq!(3, start);
    assert_eq!(5, end);
    for line in [start, (start + end) / 2, end] {
        let mut session = case.document_session();
        let response = doc_decision(
            &session,
            json!([{
                "text": format!("comment on line {line}"),
                "target": {"scope": "line", "path": case.doc_str(), "start_line": line, "end_line": line},
            }]),
        );
        assert_eq!(204, response.status, "line={line}: {}", response.text());
        let result = session.result(30.0);
        assert_eq!(json!(line), result["annotations"][0]["target"]["start_line"], "line={line}: the range must survive the round trip");
        case.keep(session);
    }
}

fn a_line_no_block_covers_is_still_refused() {
    let case = block_case();
    let mut session = case.document_session();
    let response = doc_decision(
        &session,
        json!([{"text": "invented", "target": {"scope": "line", "path": case.doc_str(), "start_line": 999, "end_line": 999}}]),
    );
    assert_eq!(400, response.status);
    assert!(response.json()["error"].as_str().expect("an error").contains("999"), "{}", response.text());
    assert!(session.still_open(0.3));
}

// ---------------------------------------------------------------------------
// CodeBlockLanguageTests: the page tokenizes; the server only records which
// language a fence claims.
// ---------------------------------------------------------------------------

fn fenced(case: &Case, body: &str, language: &str) -> Review {
    write(&case.doc, &format!("# T\n\n```{language}\n{body}\n```\n\nAfter.\n"));
    case.document_session()
}

fn first_block(session: &mut Review, kind: &str) -> Value {
    session.payload()["blocks"].as_array().expect("blocks").iter().find(|b| b["kind"] == kind).expect("a block of that kind").clone()
}

fn the_fence_language_reaches_the_block() {
    let case = Case::document();
    let mut session = fenced(&case, "x = 1  # a count", "python");
    let block = first_block(&mut session, "code");
    assert_eq!("python", block["language"]);
    assert!(block["source"].as_str().expect("source").contains("x = 1  # a count"));
}

fn an_unknown_language_is_passed_through_not_guessed() {
    let case = Case::document();
    let mut session = fenced(&case, "value = 2", "brainfuck");
    let block = first_block(&mut session, "code");
    assert_eq!("brainfuck", block["language"], "the engine decides what it knows, not the server");
}

fn a_bare_fence_claims_no_language() {
    let case = Case::document();
    write(&case.doc, "# T\n\n```\nplain\n```\n");
    let mut session = case.document_session();
    let block = first_block(&mut session, "code");
    assert_eq!("", block["language"]);
}

fn source_html_in_a_code_block_is_still_escaped() {
    let case = Case::document();
    let mut session = fenced(&case, "x = '<script>alert(1)</script>'", "python");
    let block = first_block(&mut session, "code");
    let html = block["html"].as_str().expect("html");
    assert!(!html.contains("<script>"), "{html}");
    assert!(html.contains("&lt;script&gt;"), "{html}");
}

fn prose_around_a_code_block_is_untouched() {
    let case = Case::document();
    let mut session = fenced(&case, "value = 2", "python");
    let payload = session.payload();
    let kinds: Vec<&Value> = payload["blocks"].as_array().expect("blocks").iter().map(|b| at(b, "kind")).collect();
    assert_eq!(vec!["heading", "blank", "code", "blank", "text"], kinds);
    let text = first_block(&mut session, "text");
    assert!(text.get("language").is_none(), "{text}");
}

// ---------------------------------------------------------------------------
// PageSurfaceTests: the page is one inline script, and these tests read the
// script the page actually serves.
// ---------------------------------------------------------------------------

fn every_helper_the_page_needs_is_defined() {
    let case = Case::new();
    let script = case.page_script();
    for name in [
        "say", "unresolvedThreads", "resolvedThreads", "reviewText", "sideNumber",
        "sendDecision", "render", "renderFiles", "renderDiff", "makeRow",
        "visibleFiles", "renderDocument", "renderBlock", "renderOutline",
        "trackOutline", "jumpToRange", "onDocumentPress", "onRowPress",
        "paintSelection", "markThreadedRows", "markDocumentThreadedRows",
        "selectionTarget", "showComposer", "hideComposer", "refreshThreads",
        "saveComment", "openDialog", "closeDialog", "approve", "confirmApprove",
        "requestChanges", "returnToPlanning", "cancelReview", "jumpToThread",
        "threadLocation", "renderThreads", "renderFooter", "cycleThread",
        "stepFile", "loadReview",
    ] {
        let pattern = format!(r"(?:function|async function) {}\(", fancy_regex::escape(name));
        assert!(matches(&pattern, &script), "name={name}: the page does not define it");
    }
}

fn no_helper_is_defined_twice() {
    // A shadowed definition silently wins and is how the document-aware
    // selection path was lost once.
    let case = Case::new();
    let script = case.page_script();
    let names = find_all(r"(?:function|async function)\s+([A-Za-z_$][\w$]*)\s*\(", &script);
    assert_eq!(BTreeSet::new(), duplicates(&names), "these page functions are defined more than once");
}

fn the_page_reaches_the_areas_the_markup_declares() {
    let case = Case::new();
    let session = case.session();
    let page = get(&session.url).text();
    let script = case.page_script();
    let referenced: BTreeSet<String> = find_all(r#"\bel\("([\w-]+)"\)"#, &script).into_iter().collect();
    let declared: BTreeSet<String> = find_all(r#"id="([\w-]+)""#, &page).into_iter().collect();
    let missing: BTreeSet<&String> = referenced.difference(&declared).collect();
    assert!(missing.is_empty(), "the script reaches ids the page does not declare: {missing:?}");
}

fn the_boot_config_names_the_endpoints_the_routes_serve() {
    let case = Case::new();
    let session = case.session();
    let page = get(&session.url).text();
    for key in ["\"reviewUrl\":\"api/review\"", "\"decisionUrl\":\"api/decision\""] {
        assert!(page.contains(key), "{key}");
    }
    assert!(!page.contains("api/doc"), "a diff page must not point at the document routes");
}

fn the_page_never_shadows_a_vendored_global() {
    // highlight.js declares `hljs` and markdown-it declares `markdownit` at
    // global scope; a `const` of the same name in the inline script is a
    // SyntaxError that stops the whole page from rendering.
    let case = Case::new();
    let script = case.page_script();
    for name in ["hljs", "markdownit"] {
        assert!(!matches(&format!(r"(?:const|let|var)\s+{name}\b"), &script), "name={name}");
    }
}

fn the_page_carries_no_stray_control_characters() {
    // A backslash written for the browser that a template step unescapes
    // arrives as a control character.
    let case = Case::new();
    let session = case.session();
    let response = get(&session.url);
    assert_eq!(200, response.status);
    let text = String::from_utf8(response.body).expect("UTF-8");
    let strays: BTreeSet<String> =
        text.chars().filter(|&c| (c as u32) < 32 && c != '\n' && c != '\t').map(|c| format!("U+{:04X}", c as u32)).collect();
    assert_eq!(BTreeSet::new(), strays, "the page contains control characters from a mangled escape");
}

fn the_page_declares_no_top_level_binding_twice() {
    // A repeated `const` is a SyntaxError that stops the whole page.
    let case = Case::new();
    let script = case.page_script();
    let names = find_all(r"(?m)^(?:const|let)\s+([A-Za-z_$][\w$]*)", &script);
    assert_eq!(BTreeSet::new(), duplicates(&names), "a top-level binding is declared twice");
}

// ---------------------------------------------------------------------------
// These need node; without it on PATH they return early.
// ---------------------------------------------------------------------------

const NODE_ROW_CHECK: &str = r##"(() => {
const fs = require("fs");
const input = JSON.parse(fs.readFileSync(0, "utf8"));
const engine = require(input.hljs);
const window = {hljs: engine};
const NEWLINE = String.fromCharCode(10);
const tokenCache = new Map();
const state = {files: [input.file]};
const cell = () => ({className: "", dataset: {}, textContent: "", innerHTML: "",
  append(...kids) { this.kids = kids; }, addEventListener() {}});
const document = {createElement: cell};
%(functions)s
const shown = (code) => code.innerHTML
  ? code.innerHTML.replace(/<[^>]*>/g, "").replace(/&#x27;/g, "'").replace(/&quot;/g, '"')
      .replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&amp;/g, "&")
  : code.textContent;
const out = [];
input.file.lines.forEach((line, index) => {
  if (line.type === "meta") return;
  const code = makeRow(0, line, index).kids[3];
  out.push({type: line.type, text: line.text === "" ? " " : line.text, shown: shown(code)});
});
process.stdout.write(JSON.stringify(out));
})();
"##;

/// The gate already needs node, so a missing node fails rather than skips.
fn require_node() {
    assert!(which("node").is_some(), "node is needed to run the page's row code");
}

fn js_function(script: &str, name: &str) -> String {
    let start = script.find(&format!("function {name}(")).unwrap_or_else(|| panic!("{name} is not in the script"));
    let open = start + script[start..].find('{').expect("a body");
    let mut depth = 0i32;
    for (offset, byte) in script.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            return script[start..=open + offset].to_string();
        }
    }
    panic!("{name} is not closed");
}

fn rows_as_shown(case: &Case, path: &str) -> Vec<Value> {
    let script = case.page_script();
    let functions: Vec<String> =
        ["escapeHtml", "sideNumber", "splitTokenHtml", "tokenLines", "lineTokens", "makeRow", "onRowPress"]
            .iter()
            .map(|name| js_function(&script, name))
            .collect();
    let mut session = case.session();
    let request = json!({
        "hljs": root_dir().join("assets/vendor/highlight.min.js").to_string_lossy(),
        "file": case.file(&session.payload(), path),
    });
    case.keep(session);
    let program = NODE_ROW_CHECK.replace("%(functions)s", &functions.join("\n"));
    let mut command = Command::new("node");
    command.args(["-e", &program]);
    let completed = run_with(&mut command, Some(request.to_string().as_bytes()), 30);
    assert_eq!(0, completed.code, "{}", completed.stderr);
    serde_json::from_str(&completed.stdout).expect("node printed JSON")
}

fn every_highlighted_row_shows_its_own_line() {
    require_node();
    let case = Case::new();
    write(&case.root.join("a.py"), "import os\nprint('two')\n");
    let rows = rows_as_shown(&case, "a.py");
    let types: BTreeSet<&str> = rows.iter().map(|r| r["type"].as_str().expect("type")).collect();
    assert_eq!(BTreeSet::from(["add", "remove"]), types);
    for row in &rows {
        assert_eq!(row["text"], row["shown"], "type={} text={}", row["type"], row["text"]);
    }
}

fn the_page_reports_a_closing_window_and_never_cancels_from_pagehide() {
    let case = Case::new();
    let script = case.page_script();
    let handler = &script[script.find("addEventListener(\"pagehide\"").expect("a pagehide handler")..];
    let handler = &handler[..handler.find("});").expect("the end of the handler")];
    assert!(handler.contains("api/closing"), "{handler}");
    assert!(handler.contains("boot.page"), "the server needs to know which page is closing");
    assert!(!handler.contains("cancelled"), "a reload fires pagehide; only the server may decide it was a close");
}
