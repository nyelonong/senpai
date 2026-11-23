//! The client: requests, fingerprints, approvals, fail-closed results, the
//! ledger, reviewer launch and timeouts, documents, guides, recovery and
//! preflight.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::fakes::ROLE_FLAG;
use crate::support::*;

const APPROVE: &str =
    r#"{"version":"1","review_id":"__REVIEW_ID__","decision":"approve","annotations":[]}"#;
const REJECT: &str = r#"{"version":"1","review_id":"__REVIEW_ID__","decision":"request-changes","annotations":[{"text":"needs work","target":{"scope":"review"}}]}"#;

pub const TESTS: &[(&str, fn())] = &[
    // RequestValidationTests
    ("rejects_invalid_requests", rejects_invalid_requests),
    ("rejects_non_json_request", rejects_non_json_request),
    ("rejects_non_worktree_root", rejects_non_worktree_root),
    ("usage_errors", usage_errors),
    ("canonicalises_symlinked_root", canonicalises_symlinked_root),
    // FingerprintTests
    (
        "fingerprint_is_stable_for_an_unchanged_tree",
        fingerprint_is_stable_for_an_unchanged_tree,
    ),
    (
        "fingerprint_changes_with_content",
        fingerprint_changes_with_content,
    ),
    (
        "fingerprint_changes_with_an_untracked_file",
        fingerprint_changes_with_an_untracked_file,
    ),
    (
        "fingerprint_changes_with_a_binary_untracked_file",
        fingerprint_changes_with_a_binary_untracked_file,
    ),
    (
        "fingerprint_changes_with_a_symlink_target",
        fingerprint_changes_with_a_symlink_target,
    ),
    (
        "fingerprint_changes_with_a_rename",
        fingerprint_changes_with_a_rename,
    ),
    (
        "fingerprint_changes_with_a_new_baseline",
        fingerprint_changes_with_a_new_baseline,
    ),
    ("fingerprint_changes_with_the_baseline_alone", fingerprint_changes_with_the_baseline_alone),
    (
        "fingerprint_uses_the_empty_tree_for_an_unborn_repository",
        fingerprint_uses_the_empty_tree_for_an_unborn_repository,
    ),
    ("scope_limits_the_snapshot", scope_limits_the_snapshot),
    (
        "scope_matches_a_directory_prefix_only",
        scope_matches_a_directory_prefix_only,
    ),
    // ApprovalFlowTests
    (
        "approve_records_a_verifiable_approval",
        approve_records_a_verifiable_approval,
    ),
    ("ledger_is_private", ledger_is_private),
    (
        "request_changes_and_cancellation_do_not_approve",
        request_changes_and_cancellation_do_not_approve,
    ),
    (
        "mid_review_change_is_rejected_after_approval",
        mid_review_change_is_rejected_after_approval,
    ),
    (
        "status_reports_the_newest_entry",
        status_reports_the_newest_entry,
    ),
    ("status_before_any_review", status_before_any_review),
    ("review_from_a_json_request", review_from_a_json_request),
    ("verify_from_a_json_request", verify_from_a_json_request),
    // FailClosedResultTests
    ("non_json_launcher_output", non_json_launcher_output),
    ("launcher_failure", launcher_failure),
    ("two_json_documents", two_json_documents),
    ("json_array_result", json_array_result),
    ("review_id_mismatch", review_id_mismatch),
    ("wrong_protocol_version", wrong_protocol_version),
    ("missing_protocol_version", missing_protocol_version),
    ("unknown_decision", unknown_decision),
    ("missing_decision", missing_decision),
    ("missing_annotations", missing_annotations),
    ("request_changes_without_text", request_changes_without_text),
    (
        "request_changes_without_annotations",
        request_changes_without_annotations,
    ),
    ("cancelled_with_annotations", cancelled_with_annotations),
    ("malformed_annotation_target", malformed_annotation_target),
    ("annotation_without_a_target", annotation_without_a_target),
    // LedgerTests
    ("newest_entry_wins", newest_entry_wins),
    (
        "malformed_ledger_lines_are_skipped",
        malformed_ledger_lines_are_skipped,
    ),
    (
        "producer_mismatch_never_validates",
        producer_mismatch_never_validates,
    ),
    (
        "fingerprint_mismatch_never_validates",
        fingerprint_mismatch_never_validates,
    ),
    (
        "verify_fails_after_any_worktree_change",
        verify_fails_after_any_worktree_change,
    ),
    (
        "consume_after_approve_makes_verify_fail",
        consume_after_approve_makes_verify_fail,
    ),
    (
        "revoke_after_approve_makes_verify_fail",
        revoke_after_approve_makes_verify_fail,
    ),
    (
        "a_failing_review_revokes_a_previous_approval",
        a_failing_review_revokes_a_previous_approval,
    ),
    (
        "scope_narrows_what_invalidates_an_approval",
        scope_narrows_what_invalidates_an_approval,
    ),
    (
        "every_repository_needs_an_approval",
        every_repository_needs_an_approval,
    ),
    (
        "consume_and_revoke_accept_every_reviewed_repository",
        consume_and_revoke_accept_every_reviewed_repository,
    ),
    (
        "status_falls_back_to_the_working_directory",
        status_falls_back_to_the_working_directory,
    ),
    // LauncherTests
    (
        "the_reviewer_receives_the_request_and_drives_the_review",
        the_reviewer_receives_the_request_and_drives_the_review,
    ),
    (
        "reviewer_failure_is_recorded_as_an_error",
        reviewer_failure_is_recorded_as_an_error,
    ),
    // TimeoutTests
    ("timeout_environment", timeout_environment),
    (
        "a_reviewer_that_outlives_the_timeout_is_an_error",
        a_reviewer_that_outlives_the_timeout_is_an_error,
    ),
    (
        "launcher_failure_on_a_missing_binary",
        launcher_failure_on_a_missing_binary,
    ),
    // PackagingTests
    ("client_is_an_executable_file", client_is_an_executable_file),
    (
        "client_reports_usage_without_a_subcommand",
        client_reports_usage_without_a_subcommand,
    ),
    // ReviewerResolutionTests
    ("html_is_the_default_reviewer", html_is_the_default_reviewer),
    (
        "reviewer_named_html_runs_the_bundled_page",
        reviewer_named_html_runs_the_bundled_page,
    ),
    (
        "rdg_reviewer_resolves_to_the_binary_on_the_path",
        rdg_reviewer_resolves_to_the_binary_on_the_path,
    ),
    (
        "reviewer_environment_wins_over_the_legacy_variable",
        reviewer_environment_wins_over_the_legacy_variable,
    ),
    (
        "legacy_reviewer_variable_is_still_honored",
        legacy_reviewer_variable_is_still_honored,
    ),
    (
        "blank_reviewer_values_fall_through",
        blank_reviewer_values_fall_through,
    ),
    (
        "an_explicit_launcher_path_still_wins",
        an_explicit_launcher_path_still_wins,
    ),
    // DocumentCommandTests
    (
        "an_approved_spec_verifies_and_then_stops_counting",
        an_approved_spec_verifies_and_then_stops_counting,
    ),
    (
        "document_recovery_preserves_the_document_request_and_verified_decision",
        document_recovery_preserves_the_document_request_and_verified_decision,
    ),
    (
        "editing_the_spec_after_approval_invalidates_it",
        editing_the_spec_after_approval_invalidates_it,
    ),
    (
        "a_different_criterion_does_not_match_the_approval",
        a_different_criterion_does_not_match_the_approval,
    ),
    (
        "verify_takes_the_criterion_from_the_ledger_when_it_is_not_repeated",
        verify_takes_the_criterion_from_the_ledger_when_it_is_not_repeated,
    ),
    (
        "requested_changes_leaves_no_approval",
        requested_changes_leaves_no_approval,
    ),
    (
        "a_diff_shaped_result_is_rejected_for_a_document_review",
        a_diff_shaped_result_is_rejected_for_a_document_review,
    ),
    (
        "the_ledger_names_the_document",
        the_ledger_names_the_document,
    ),
    (
        "a_document_outside_a_repository_keeps_its_own_ledger",
        a_document_outside_a_repository_keeps_its_own_ledger,
    ),
    // GuidedReviewTests
    (
        "status_accepts_paths_and_returns_the_scoped_guide_fingerprint",
        status_accepts_paths_and_returns_the_scoped_guide_fingerprint,
    ),
    (
        "cli_captures_guide_once_and_existing_verification_still_checks_code",
        cli_captures_guide_once_and_existing_verification_still_checks_code,
    ),
    (
        "inline_guide_round_trip_and_mid_review_mutation_fails_closed",
        inline_guide_round_trip_and_mid_review_mutation_fails_closed,
    ),
    (
        "stale_unknown_and_non_html_guides_never_reach_a_launcher",
        stale_unknown_and_non_html_guides_never_reach_a_launcher,
    ),
    (
        "bad_files_and_conflicting_flags_report_local_errors",
        bad_files_and_conflicting_flags_report_local_errors,
    ),
    // RecoveryFlowTests
    (
        "validated_result_and_exact_request_are_retained",
        validated_result_and_exact_request_are_retained,
    ),
    (
        "nonapproval_is_a_finished_decision_not_a_launcher_error",
        nonapproval_is_a_finished_decision_not_a_launcher_error,
    ),
    (
        "malformed_result_retains_error_without_an_approval_artifact",
        malformed_result_retains_error_without_an_approval_artifact,
    ),
    (
        "approval_for_changed_inputs_is_not_retained_as_a_result",
        approval_for_changed_inputs_is_not_retained_as_a_result,
    ),
    (
        "recovery_storage_failure_cannot_leave_a_verifiable_approval",
        recovery_storage_failure_cannot_leave_a_verifiable_approval,
    ),
    (
        "complete_multi_repository_scope_is_retained_and_verified_together",
        complete_multi_repository_scope_is_retained_and_verified_together,
    ),
    (
        "existing_run_is_not_overwritten_or_relaunched",
        existing_run_is_not_overwritten_or_relaunched,
    ),
    (
        "recovery_inside_reviewed_tree_or_public_directory_is_rejected",
        recovery_inside_reviewed_tree_or_public_directory_is_rejected,
    ),
    (
        "live_diagnostics_arrive_before_decision_and_cancellation_reaps_reviewer",
        live_diagnostics_arrive_before_decision_and_cancellation_reaps_reviewer,
    ),
    // PreflightTests
    (
        "valid_guide_preflight_does_not_change_existing_approval",
        valid_guide_preflight_does_not_change_existing_approval,
    ),
    (
        "duplicate_stale_and_out_of_scope_guides_fail_without_ledger_mutation",
        duplicate_stale_and_out_of_scope_guides_fail_without_ledger_mutation,
    ),
    // RangeTests
    ("base_review_includes_committed_and_uncommitted_changes", base_review_includes_committed_and_uncommitted_changes),
    ("base_approval_verifies_only_with_the_same_base", base_approval_verifies_only_with_the_same_base),
    ("head_review_ignores_the_working_tree", head_review_ignores_the_working_tree),
    ("head_moving_during_review_denies_approval", head_moving_during_review_denies_approval),
    ("range_usage_errors_and_unknown_revisions_fail_closed", range_usage_errors_and_unknown_revisions_fail_closed),
    ("base_moving_during_review_denies_approval", base_moving_during_review_denies_approval),
    ("range_reads_pathspec_like_names_literally", range_reads_pathspec_like_names_literally),
    ("range_scope_keeps_renames_out_of_scope", range_scope_keeps_renames_out_of_scope),
    ("range_shows_ignored_submodule_changes_and_deletions", range_shows_ignored_submodule_changes_and_deletions),
];

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Writes the stub reviewer at `path`; it records the path it was started by
/// as `argv[0]`.
fn install_stub(path: &Path) {
    let dir = path.parent().expect("a directory");
    let name = path.file_name().expect("a name").to_string_lossy();
    crate::fakes::install(dir, &name, "reviewer", &[]);
}

type Env<'a> = &'a [(&'a str, Option<&'a str>)];

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o7777
}

/// `ReviewCase`: a temporary Git repository plus a stub launcher.
struct Case {
    _tmp: TempDir,
    base: PathBuf,
    launcher: PathBuf,
    root: PathBuf,
}

impl Case {
    fn new() -> Self {
        let tmp = TempDir::new();
        let base = tmp.path().to_path_buf();
        let launcher = base.join("fake-rdg");
        install_stub(&launcher);
        let root = make_repo(&base, "repo");
        let case = Case {
            _tmp: tmp,
            base,
            launcher,
            root,
        };
        case.write("a.py", "print('two')\n");
        case.write("other/b.txt", "changed\n");
        case.write("untracked.txt", "new file\n");
        case
    }

    fn write(&self, relative: &str, content: &str) {
        write(&self.root.join(relative), content);
    }

    fn make_repo(&self, name: &str) -> PathBuf {
        make_repo(&self.base, name)
    }

    fn commit(&self, message: &str) {
        commit(&self.root, message);
    }

    /// The client environment; an extra value of `None` removes that variable.
    fn env<'c>(&self, command: &'c mut Command, extra: Env) -> &'c mut Command {
        command
            .env_remove(REVIEWER_ENV)
            .env_remove(LAUNCHER_ENV)
            .env_remove(TIMEOUT_ENV);
        command
            .env(LAUNCHER_ENV, &self.launcher)
            .env(TIMEOUT_ENV, "60")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("STUB_RESULT", APPROVE);
        for (key, value) in extra {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        command
    }

    fn run_client_in(&self, args: &[&str], cwd: Option<&Path>, extra: Env) -> Run {
        let mut command = senpai(args);
        self.env(&mut command, extra);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        run_with(&mut command, None, 120)
    }

    fn run_client(&self, args: &[&str], extra: Env) -> Run {
        self.run_client_in(args, None, extra)
    }

    fn review(&self, args: &[&str], extra: Env) -> Run {
        let root = s(&self.root);
        let mut all = vec!["review", "--criterion", "ship it", "--repo", &root];
        all.extend_from_slice(args);
        self.run_client(&all, extra)
    }

    fn verify(&self, args: &[&str]) -> Run {
        let root = s(&self.root);
        let mut all = vec!["verify", "--repo", &root];
        all.extend_from_slice(args);
        self.run_client(&all, &[])
    }

    fn ledger_path(&self) -> PathBuf {
        self.root.join(".git").join(LEDGER_NAME)
    }

    fn entries(&self) -> Vec<Value> {
        read_entries(&self.ledger_path())
    }

    fn states(&self) -> Vec<Value> {
        self.entries()
            .iter()
            .map(|e| e.get("state").cloned().unwrap_or(Value::Null))
            .collect()
    }

    fn assert_no_approval(&self, result: &Run) {
        assert_eq!(result.code, EXIT_ERROR, "{}", result.stderr);
        assert_eq!(
            result.stdout, "",
            "a failing review must not print an approval"
        );
        let states = self.states();
        assert_ne!(
            states.last(),
            Some(&json!("approved")),
            "an approval was recorded: {:?}",
            self.entries()
        );
        assert_eq!(self.verify(&[]).code, EXIT_NOT_APPROVED);
    }

    fn fingerprint(&self, root: Option<&Path>, paths: Option<&[&str]>) -> String {
        let root = s(root.unwrap_or(&self.root));
        let joined = paths.map(|p| p.join(","));
        let mut args = vec!["preflight", "--criterion", "ship it", "--repo", &root];
        if let Some(joined) = &joined {
            args.extend(["--paths", joined.as_str()]);
        }
        let result = self.run_client(&args, &[]);
        assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
        let payload = single_json_line(&result);
        assert_eq!(payload["valid"], json!(true));
        payload["fingerprint"]
            .as_str()
            .expect("a fingerprint")
            .to_string()
    }

    /// Starts `command` with the bundled HTML reviewer and returns it with its
    /// page URL.
    fn start_html(&self, mut command: Command, extra: Env) -> (Proc, String) {
        let url_file = self.base.join(format!("url-{}", nanos()));
        let url_file_text = s(&url_file);
        let mut settings: Vec<(&str, Option<&str>)> = vec![
            (REVIEWER_ENV, Some("html")),
            (NO_BROWSER_ENV, Some("1")),
            (URL_FILE_ENV, Some(&url_file_text)),
        ];
        settings.extend_from_slice(extra);
        self.env(&mut command, &settings);
        let mut process = Proc(Some(spawn(&mut command)));
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(text) = std::fs::read_to_string(&url_file) {
                if !text.trim().is_empty() {
                    return (process, text.trim().to_string());
                }
            }
            if process.child().try_wait().expect("poll").is_some() {
                let run = process.communicate(30);
                panic!("the HTML reviewer never published its page: {run:?}");
            }
            assert!(
                Instant::now() < deadline,
                "the HTML reviewer never published its page"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn served_files(&self, root: Option<&Path>, paths: Option<&[&str]>) -> Vec<Value> {
        let mut repository = json!({"root": s(root.unwrap_or(&self.root))});
        if let Some(paths) = paths {
            repository["paths"] = json!(paths);
        }
        self.served_repository(repository)["files"].as_array().expect("files").clone()
    }

    /// The repository object the HTML reviewer serves for `repository`.
    fn served_repository(&self, repository: Value) -> Value {
        let request = json!({"version": "1", "review_id": "files", "criterion": "ship it", "repositories": [repository]});
        let (process, url) =
            self.start_html(reviewer(["--review".to_string(), request.to_string()]), &[]);
        let served = served(&url)["repositories"][0].clone();
        decide(&url, "cancelled");
        process.communicate(30);
        served
    }

    fn guide(&self) -> Value {
        json!({
            "version": 1, "fingerprint": self.fingerprint(None, Some(&["a.py"])),
            "summary": "Print a different value", "before": "Print one", "after": "Print two",
            "chapters": [{"title": "Output", "summary": "Change the printed value",
                          "files": [{"repository": s(&self.root), "path": "a.py"}]}],
        })
    }

    fn guided_request(&self, guide: Option<Value>) -> Value {
        let mut request = json!({
            "version": "1", "review_id": "guided-review", "criterion": "ship it",
            "repositories": [{"root": s(&self.root), "paths": ["a.py"]}],
        });
        if let Some(guide) = guide {
            request["guide"] = guide;
        }
        request
    }

    fn recovery(&self) -> PathBuf {
        self.base.join("private recovery")
    }

    fn read_recovery(&self, name: &str) -> Value {
        serde_json::from_str(&read(&self.recovery().join(name))).expect("recovery JSON")
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos()
}

/// stdout must be exactly one JSON object on one line, and parse cleanly.
fn single_json_line(result: &Run) -> Value {
    assert!(
        result.stdout.ends_with('\n'),
        "stdout must end with a newline: {:?}",
        result.stdout
    );
    let lines: Vec<&str> = result.stdout.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "stdout must be a single line: {:?}",
        result.stdout
    );
    let payload: Value = serde_json::from_str(lines[0]).expect("JSON");
    assert!(payload.is_object(), "{payload}");
    payload
}

fn served(url: &str) -> Value {
    let response = get(&format!("{url}api/review"));
    assert_eq!(response.status, 200, "{}", response.text());
    response.json()
}

fn decide(url: &str, decision: &str) {
    let response = post(
        &format!("{url}api/decision"),
        &json!({"decision": decision, "annotations": []}),
    );
    assert!(
        (200..300).contains(&response.status),
        "{} {}",
        response.status,
        response.text()
    );
}

fn paths_of(files: &[Value]) -> Vec<String> {
    files
        .iter()
        .map(|f| f["path"].as_str().expect("path").to_string())
        .collect()
}

/// A started child that is killed if the test ends while it still runs.
struct Proc(Option<Child>);

impl Proc {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("a child")
    }

    /// Reads output while waiting and fails the test if the child outlives
    /// the timeout.
    fn communicate(mut self, timeout: u64) -> Run {
        let child = self.0.take().expect("a child");
        communicate(child, timeout)
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn drain<R: Read + Send + 'static>(mut reader: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = reader.read_to_end(&mut buffer);
        buffer
    })
}

fn communicate(mut child: Child, timeout: u64) -> Run {
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    wait_exit(&mut child, timeout);
    let collect = |h: Option<std::thread::JoinHandle<Vec<u8>>>| {
        h.map(|h| h.join().unwrap_or_default()).unwrap_or_default()
    };
    let status = child.wait().expect("wait");
    Run::from(std::process::Output {
        status,
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}

fn wait_exit(child: &mut Child, timeout: u64) {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    while child.try_wait().expect("wait").is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the process did not finish within {timeout}s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// RequestValidationTests
// ---------------------------------------------------------------------------

fn rejects_invalid_requests() {
    let case = Case::new();
    let root = s(&case.root);
    let cases: Vec<(&str, Value)> = vec![
        ("relative root", json!({"repositories": [{"root": "repo"}]})),
        (
            "parent traversal in root",
            json!({"repositories": [{"root": "/tmp/../etc"}]}),
        ),
        ("unknown protocol version", json!({"version": "2"})),
        ("empty review id", json!({"review_id": ""})),
        ("empty criterion", json!({"criterion": ""})),
        ("no repositories", json!({"repositories": []})),
        ("repositories not a list", json!({"repositories": {}})),
        (
            "duplicate roots",
            json!({"repositories": [{"root": root}, {"root": root}]}),
        ),
        (
            "empty scope list",
            json!({"repositories": [{"root": root, "paths": []}]}),
        ),
        (
            "scope with parent traversal",
            json!({"repositories": [{"root": root, "paths": ["src/../a.py"]}]}),
        ),
        (
            "scope with current dir",
            json!({"repositories": [{"root": root, "paths": ["./a.py"]}]}),
        ),
        (
            "absolute scope",
            json!({"repositories": [{"root": root, "paths": ["/a.py"]}]}),
        ),
        (
            "unnormalised scope",
            json!({"repositories": [{"root": root, "paths": ["src//a.py"]}]}),
        ),
        (
            "scope with trailing slash",
            json!({"repositories": [{"root": root, "paths": ["src/"]}]}),
        ),
        ("repository not an object", json!({"repositories": [root]})),
        (
            "unknown repository key",
            json!({"repositories": [{"root": root, "depth": 2}]}),
        ),
    ];
    for (label, override_) in cases {
        let mut payload = json!({"version": "1", "review_id": "r1", "criterion": "ship it"});
        for (key, value) in override_.as_object().expect("object") {
            payload[key] = value.clone();
        }
        let result = case.run_client(&["review", "--json", &payload.to_string()], &[]);
        assert_eq!(result.code, EXIT_ERROR, "case={label}: {}", result.stderr);
        assert_eq!(
            result.stdout, "",
            "case={label}: a failing review must not print an approval"
        );
        assert_ne!(
            case.states().last(),
            Some(&json!("approved")),
            "case={label}: an approval was recorded"
        );
        assert_eq!(case.verify(&[]).code, EXIT_NOT_APPROVED, "case={label}");
        assert!(
            result.stderr.contains("task_review:"),
            "case={label}: {}",
            result.stderr
        );
    }
}

fn rejects_non_json_request() {
    let case = Case::new();
    case.assert_no_approval(&case.run_client(&["review", "--json", "{not json"], &[]));
}

fn rejects_non_worktree_root() {
    let case = Case::new();
    let plain = case.root.parent().expect("parent").join("plain");
    std::fs::create_dir(&plain).expect("mkdir");
    let result = case.run_client(&["review", "--criterion", "x", "--repo", &s(&plain)], &[]);
    case.assert_no_approval(&result);
    assert!(result.stderr.contains("worktree"), "{}", result.stderr);
}

fn usage_errors() {
    let case = Case::new();
    let root = s(&case.root);
    let cases: Vec<(&str, Vec<&str>)> = vec![
        (
            "paths with two repos",
            vec![
                "review",
                "--criterion",
                "x",
                "--repo",
                &root,
                "--repo",
                &root,
                "--paths",
                "a.py",
            ],
        ),
        (
            "json with criterion",
            vec!["review", "--criterion", "x", "--json", "{}"],
        ),
        (
            "json with repo",
            vec!["review", "--json", "{}", "--repo", &root],
        ),
        ("no subcommand", vec![]),
    ];
    for (label, argv) in cases {
        let result = case.run_client(&argv, &[]);
        assert_eq!(result.code, EXIT_ERROR, "case={label}: {}", result.stderr);
        assert_eq!(result.stdout, "", "case={label}");
    }
    case.assert_no_approval(&case.run_client(&["consume"], &[]));
}

fn canonicalises_symlinked_root() {
    let case = Case::new();
    let link = case.root.parent().expect("parent").join("link");
    std::os::unix::fs::symlink(&case.root, &link).expect("symlink");
    let result = case.run_client(&["status", "--repo", &s(&link)], &[]);
    assert_eq!(result.code, EXIT_NOT_APPROVED, "{}", result.stderr);
    assert_eq!(
        single_json_line(&result)["repositories"][0]["root"],
        json!(s(&case.root))
    );
}

// ---------------------------------------------------------------------------
// FingerprintTests
// ---------------------------------------------------------------------------

fn fingerprint_is_stable_for_an_unchanged_tree() {
    let case = Case::new();
    let first = case.fingerprint(None, None);
    assert_eq!(first, case.fingerprint(None, None));
    assert_eq!(first.len(), 64);
}

fn fingerprint_changes_with_content() {
    let case = Case::new();
    let before = case.fingerprint(None, None);
    case.write("a.py", "print('three')\n");
    assert_ne!(before, case.fingerprint(None, None));
}

fn fingerprint_changes_with_an_untracked_file() {
    let case = Case::new();
    let before = case.fingerprint(None, None);
    case.write("untracked.txt", "different content\n");
    assert_ne!(before, case.fingerprint(None, None));
}

fn fingerprint_changes_with_a_binary_untracked_file() {
    let case = Case::new();
    let before = case.fingerprint(None, None);
    std::fs::write(case.root.join("blob.bin"), b"\x00\x01\x02binary\x00").expect("write");
    let after_first = case.fingerprint(None, None);
    assert_ne!(before, after_first);
    std::fs::write(case.root.join("blob.bin"), b"\x00\x01\x02different\x00").expect("write");
    assert_ne!(after_first, case.fingerprint(None, None));
}

fn fingerprint_changes_with_a_symlink_target() {
    let case = Case::new();
    let link = case.root.join("link");
    std::os::unix::fs::symlink("a.py", &link).expect("symlink");
    let before = case.fingerprint(None, None);
    std::fs::remove_file(&link).expect("unlink");
    std::os::unix::fs::symlink("other/b.txt", &link).expect("symlink");
    assert_ne!(before, case.fingerprint(None, None));
}

fn fingerprint_changes_with_a_rename() {
    let case = Case::new();
    let before = case.fingerprint(None, None);
    std::fs::rename(case.root.join("a.py"), case.root.join("renamed.py")).expect("rename");
    assert_ne!(before, case.fingerprint(None, None));
}

fn fingerprint_changes_with_a_new_baseline() {
    let case = Case::new();
    let before = case.fingerprint(None, None);
    case.commit("second");
    assert_ne!(
        before,
        case.fingerprint(None, None),
        "the baseline is part of the fingerprint"
    );
    assert_eq!(case.served_files(None, None), Vec::<Value>::new());
}

/// The same working-tree diff on a different baseline commit must not share
/// a fingerprint, or an approval could carry over to another base.
fn fingerprint_changes_with_the_baseline_alone() {
    let case = Case::new();
    git(&case.root, &["checkout", "-q", "--", "other/b.txt"]);
    std::fs::remove_file(case.root.join("untracked.txt")).expect("remove");
    let before_files = case.served_files(None, None);
    let before = case.fingerprint(None, None);
    case.write("other/b.txt", "moved on\n");
    git(&case.root, &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "unrelated", "--", "other/b.txt"]);
    assert_eq!(case.served_files(None, None), before_files, "the diff itself is unchanged");
    assert_ne!(before, case.fingerprint(None, None), "the baseline alone changes the fingerprint");
}

fn fingerprint_uses_the_empty_tree_for_an_unborn_repository() {
    let case = Case::new();
    let empty = case.base.join("unborn");
    std::fs::create_dir_all(empty.join("src")).expect("mkdir");
    git(&case.base, &["init", "-q", &s(&empty)]);
    assert_eq!(case.fingerprint(Some(&empty), None).len(), 64);
    write(&empty.join("src/a.py"), "x\n");
    git(&empty, &["add", "src/a.py"]);
    // A staged file can only be diffed when the baseline is a real tree.
    let files = case.served_files(Some(&empty), None);
    assert_eq!(paths_of(&files), vec!["src/a.py"]);
    let texts: Vec<&str> = files[0]["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|l| l["text"].as_str().unwrap_or_default())
        .collect();
    assert!(texts.contains(&"new file mode 100644"), "{texts:?}");
    assert_eq!(case.fingerprint(Some(&empty), None).len(), 64);
}

fn scope_limits_the_snapshot() {
    let case = Case::new();
    assert_eq!(
        paths_of(&case.served_files(None, Some(&["a.py"]))),
        vec!["a.py"]
    );
    assert_eq!(
        paths_of(&case.served_files(None, None)),
        vec!["a.py", "other/b.txt", "untracked.txt"]
    );
    let scoped = case.fingerprint(None, Some(&["a.py"]));
    case.write("other/b.txt", "out of scope\n");
    assert_eq!(scoped, case.fingerprint(None, Some(&["a.py"])));
}

fn scope_matches_a_directory_prefix_only() {
    let case = Case::new();
    case.write("otherwise.txt", "shares a prefix\n");
    assert_eq!(
        paths_of(&case.served_files(None, Some(&["other"]))),
        vec!["other/b.txt"]
    );
}

// ---------------------------------------------------------------------------
// ApprovalFlowTests
// ---------------------------------------------------------------------------

fn approve_records_a_verifiable_approval() {
    let case = Case::new();
    let result = case.review(&[], &[]);
    let payload = single_json_line(&result);
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
    assert_eq!(payload["decision"], json!("approve"));
    assert_eq!(payload["version"], json!(PROTOCOL_VERSION));

    let entries = case.entries();
    let states: Vec<&Value> = entries.iter().map(|e| &e["state"]).collect();
    assert_eq!(states, vec![&json!("superseded"), &json!("approved")]);
    let ts = fancy_regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$").expect("regex");
    for entry in &entries {
        assert_eq!(entry["producer"], json!(PRODUCER));
        assert_eq!(entry["criterion"], json!("ship it"));
        assert_eq!(entry["repositories"], json!([{"root": s(&case.root)}]));
        let stamp = entry["ts"].as_str().expect("ts");
        assert!(ts.is_match(stamp).expect("match"), "{stamp}");
    }
    let approved = entries.last().expect("an entry");
    assert_eq!(approved["decision"], json!("approve"));
    assert_eq!(approved["annotations"], json!([]));
    assert!(approved.get("error").is_none(), "{approved}");

    let verified = case.verify(&[]);
    assert_eq!(verified.code, EXIT_OK, "{}", verified.stderr);
    assert_eq!(
        single_json_line(&verified),
        json!({"approved": true, "fingerprint": approved["fingerprint"], "state": "approved",
               "reason": "the approval matches the current state"}),
    );
}

fn ledger_is_private() {
    let case = Case::new();
    case.review(&[], &[]);
    assert_eq!(mode(&case.ledger_path()), 0o600);
}

fn request_changes_and_cancellation_do_not_approve() {
    let case = Case::new();
    for (decision, state, annotations) in [
        (
            "request-changes",
            "rejected",
            r#"[{"text":"needs work","target":{"scope":"review"}}]"#,
        ),
        ("return-to-planning", "rejected", "[]"),
        ("cancelled", "cancelled", "[]"),
    ] {
        let template = format!(
            r#"{{"version":"1","review_id":"__REVIEW_ID__","decision":"{decision}","annotations":{annotations}}}"#
        );
        let result = case.review(&[], &[("STUB_RESULT", Some(&template))]);
        assert_eq!(
            result.code, EXIT_NOT_APPROVED,
            "decision={decision}: {}",
            result.stderr
        );
        let payload = single_json_line(&result);
        assert_eq!(payload["decision"], json!(decision), "decision={decision}");
        let states = case.states();
        assert_eq!(
            states[states.len() - 2..],
            [json!("superseded"), json!(state)],
            "decision={decision}"
        );
        assert_eq!(
            case.entries().last().expect("entry")["decision"],
            json!(decision),
            "decision={decision}"
        );
        assert_eq!(
            case.verify(&[]).code,
            EXIT_NOT_APPROVED,
            "decision={decision}"
        );
        assert!(
            !case.states().contains(&json!("approved")),
            "decision={decision}"
        );
    }
}

fn mid_review_change_is_rejected_after_approval() {
    let case = Case::new();
    let target = s(&case.root.join("a.py"));
    let result = case.review(
        &[],
        &[
            ("STUB_MODE", Some("mutate")),
            ("STUB_MUTATE", Some(&target)),
        ],
    );
    case.assert_no_approval(&result);
    assert!(
        result
            .stderr
            .contains("changed while the human was deciding"),
        "{}",
        result.stderr
    );
    assert_eq!(case.states().last(), Some(&json!("error")));
    assert!(case.entries().last().expect("entry").get("error").is_some());
}

fn status_reports_the_newest_entry() {
    let case = Case::new();
    case.review(&[], &[]);
    let status = case.run_client(&["status", "--repo", &s(&case.root)], &[]);
    assert_eq!(status.code, EXIT_OK, "{}", status.stderr);
    let payload = single_json_line(&status);
    assert_eq!(payload["approved"], json!(true));
    assert_eq!(payload["state"], json!("approved"));
    assert_eq!(&payload["entry"], case.entries().last().expect("entry"));
    assert_eq!(payload["repositories"][0]["matches"], json!(true));
}

fn status_before_any_review() {
    let case = Case::new();
    let status = case.run_client(&["status", "--repo", &s(&case.root)], &[]);
    assert_eq!(status.code, EXIT_NOT_APPROVED, "{}", status.stderr);
    let payload = single_json_line(&status);
    assert!(payload["entry"].is_null(), "{payload}");
    assert!(
        payload["reason"]
            .as_str()
            .expect("reason")
            .contains("no task review approval"),
        "{payload}"
    );
}

fn review_from_a_json_request() {
    let case = Case::new();
    let payload = json!({
        "version": "1",
        "review_id": "fixed-review-id",
        "criterion": "ship it",
        "repositories": [{"root": s(&case.root), "paths": ["a.py"]}],
    });
    let result = case.run_client(&["review", "--json", &payload.to_string()], &[]);
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
    assert_eq!(
        single_json_line(&result)["review_id"],
        json!("fixed-review-id")
    );
    assert_eq!(
        case.entries().last().expect("entry")["repositories"],
        json!([{"root": s(&case.root), "paths": ["a.py"]}])
    );
    assert_eq!(case.verify(&["--paths", "a.py"]).code, EXIT_OK);
}

fn verify_from_a_json_request() {
    let case = Case::new();
    let payload =
        json!({"version": "1", "repositories": [{"root": s(&case.root), "paths": ["a.py"]}]});
    let result = case.run_client(&["verify", "--json", &payload.to_string()], &[]);
    assert_eq!(result.code, EXIT_NOT_APPROVED, "{}", result.stderr);
    assert_eq!(single_json_line(&result)["approved"], json!(false));
}

// ---------------------------------------------------------------------------
// FailClosedResultTests
// ---------------------------------------------------------------------------

/// A rejected launch must leave the ledger without a single approval.
fn assert_rejected(extra: Env) {
    let case = Case::new();
    case.assert_no_approval(&case.review(&[], extra));
    assert!(
        !case.states().contains(&json!("approved")),
        "an approval was recorded: {:?}",
        case.entries()
    );
}

fn non_json_launcher_output() {
    assert_rejected(&[("STUB_MODE", Some("garbage"))]);
}

fn launcher_failure() {
    let case = Case::new();
    let result = case.review(&[], &[("STUB_MODE", Some("fail"))]);
    case.assert_no_approval(&result);
    assert!(
        result.stderr.contains("not open or is not a Git worktree"),
        "{}",
        result.stderr
    );
    let last = case.states().last().cloned().expect("a state");
    assert!(last.as_str().expect("state").contains("error"), "{last}");
}

fn two_json_documents() {
    assert_rejected(&[("STUB_RESULT", Some(&format!("{APPROVE}\n{APPROVE}")))]);
}

fn json_array_result() {
    assert_rejected(&[("STUB_RESULT", Some("[]"))]);
}

fn review_id_mismatch() {
    assert_rejected(&[(
        "STUB_RESULT",
        Some(&APPROVE.replace("__REVIEW_ID__", "some-other-review")),
    )]);
}

fn wrong_protocol_version() {
    assert_rejected(&[(
        "STUB_RESULT",
        Some(&APPROVE.replace(r#""version":"1""#, r#""version":"2""#)),
    )]);
}

fn missing_protocol_version() {
    assert_rejected(&[(
        "STUB_RESULT",
        Some(&APPROVE.replace(r#""version":"1","#, "")),
    )]);
}

fn unknown_decision() {
    assert_rejected(&[("STUB_RESULT", Some(&APPROVE.replace("approve", "ship-it")))]);
}

fn missing_decision() {
    assert_rejected(&[(
        "STUB_RESULT",
        Some(r#"{"version":"1","review_id":"__REVIEW_ID__","annotations":[]}"#),
    )]);
}

fn missing_annotations() {
    assert_rejected(&[(
        "STUB_RESULT",
        Some(r#"{"version":"1","review_id":"__REVIEW_ID__","decision":"approve"}"#),
    )]);
}

fn request_changes_without_text() {
    assert_rejected(&[("STUB_RESULT", Some(&REJECT.replace("needs work", "   ")))]);
}

fn request_changes_without_annotations() {
    assert_rejected(&[(
        "STUB_RESULT",
        Some(
            r#"{"version":"1","review_id":"__REVIEW_ID__","decision":"request-changes","annotations":[]}"#,
        ),
    )]);
}

fn cancelled_with_annotations() {
    assert_rejected(&[(
        "STUB_RESULT",
        Some(
            r#"{"version":"1","review_id":"__REVIEW_ID__","decision":"cancelled","annotations":[{"text":"why not","target":{"scope":"review"}}]}"#,
        ),
    )]);
}

fn malformed_annotation_target() {
    let malformed = r#"{"version":"1","review_id":"__REVIEW_ID__","decision":"approve","annotations":[{"text":"look here","target":{"scope":"line","repository":"/r","path":"a.py","side":"new","start_line":5,"end_line":2}}]}"#;
    assert_rejected(&[("STUB_RESULT", Some(malformed))]);
}

fn annotation_without_a_target() {
    assert_rejected(&[(
        "STUB_RESULT",
        Some(
            r#"{"version":"1","review_id":"__REVIEW_ID__","decision":"approve","annotations":[{"text":"look here"}]}"#,
        ),
    )]);
}

// ---------------------------------------------------------------------------
// LedgerTests
// ---------------------------------------------------------------------------

fn append_raw(case: &Case, line: &str) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(case.ledger_path())
        .expect("open");
    file.write_all(format!("{line}\n").as_bytes())
        .expect("append");
}

fn newest_entry_wins() {
    let case = Case::new();
    case.review(&[], &[]);
    assert_eq!(case.verify(&[]).code, EXIT_OK);
    case.review(&[], &[("STUB_RESULT", Some(REJECT))]);
    assert_eq!(case.verify(&[]).code, EXIT_NOT_APPROVED);
    case.review(&[], &[]);
    assert_eq!(case.verify(&[]).code, EXIT_OK);
    assert_eq!(
        case.states(),
        json!([
            "superseded",
            "approved",
            "superseded",
            "rejected",
            "superseded",
            "approved"
        ])
        .as_array()
        .expect("array")
        .clone()
    );
}

fn malformed_ledger_lines_are_skipped() {
    let case = Case::new();
    case.review(&[], &[]);
    append_raw(&case, "{not json at all");
    assert_eq!(case.verify(&[]).code, EXIT_OK);
    append_raw(&case, "[]");
    assert_eq!(case.verify(&[]).code, EXIT_OK);
    assert_eq!(
        case.entries().len(),
        2,
        "malformed lines are skipped, not counted"
    );
}

fn producer_mismatch_never_validates() {
    let case = Case::new();
    case.review(&[], &[]);
    let mut approved = case.entries().last().expect("entry").clone();
    approved["producer"] = json!("pi-rdg-task-review");
    append_raw(&case, &approved.to_string());
    let result = case.verify(&[]);
    assert_eq!(result.code, EXIT_NOT_APPROVED, "{}", result.stderr);
    let payload = single_json_line(&result);
    assert!(
        payload["reason"]
            .as_str()
            .expect("reason")
            .contains(PRODUCER),
        "{payload}"
    );
}

fn fingerprint_mismatch_never_validates() {
    let case = Case::new();
    case.review(&[], &[]);
    let mut entry = case.entries().last().expect("entry").clone();
    entry["fingerprint"] = json!("0".repeat(64));
    append_raw(&case, &entry.to_string());
    let result = case.verify(&[]);
    assert_eq!(result.code, EXIT_NOT_APPROVED, "{}", result.stderr);
    let payload = single_json_line(&result);
    assert!(
        payload["reason"]
            .as_str()
            .expect("reason")
            .contains("changed since"),
        "{payload}"
    );
}

fn verify_fails_after_any_worktree_change() {
    let case = Case::new();
    case.review(&[], &[]);
    assert_eq!(case.verify(&[]).code, EXIT_OK);
    case.write("a.py", "print('changed after approval')\n");
    assert_eq!(case.verify(&[]).code, EXIT_NOT_APPROVED);
    case.write("extra.txt", "a new file\n");
    assert_eq!(case.verify(&[]).code, EXIT_NOT_APPROVED);
    std::fs::remove_file(case.root.join("extra.txt")).expect("unlink");
    case.write("a.py", "print('two')\n");
    assert_eq!(
        case.verify(&[]).code,
        EXIT_OK,
        "restoring the state restores the approval"
    );
}

fn consume_after_approve_makes_verify_fail() {
    let case = Case::new();
    case.review(&[], &[]);
    assert_eq!(case.verify(&[]).code, EXIT_OK);
    let consumed = case.run_client(&["consume", "--repo", &s(&case.root)], &[]);
    assert_eq!(consumed.code, EXIT_OK, "{}", consumed.stderr);
    assert_eq!(single_json_line(&consumed)["state"], json!("consumed"));
    let result = case.verify(&[]);
    assert_eq!(result.code, EXIT_NOT_APPROVED, "{}", result.stderr);
    let payload = single_json_line(&result);
    assert!(
        payload["reason"]
            .as_str()
            .expect("reason")
            .contains("consumed"),
        "{payload}"
    );
    // The approval was spent, not erased: the record survives for the audit.
    assert_eq!(
        case.states(),
        vec![json!("superseded"), json!("approved"), json!("consumed")]
    );
    assert_eq!(
        case.entries().last().expect("entry")["decision"],
        json!("approve")
    );
}

fn revoke_after_approve_makes_verify_fail() {
    let case = Case::new();
    case.review(&[], &[]);
    let revoked = case.run_client(&["revoke", "--repo", &s(&case.root)], &[]);
    assert_eq!(revoked.code, EXIT_OK, "{}", revoked.stderr);
    assert_eq!(single_json_line(&revoked)["state"], json!("superseded"));
    assert_eq!(case.verify(&[]).code, EXIT_NOT_APPROVED);
}

fn a_failing_review_revokes_a_previous_approval() {
    let case = Case::new();
    case.review(&[], &[]);
    assert_eq!(case.verify(&[]).code, EXIT_OK);
    case.assert_no_approval(&case.review(&[], &[("STUB_MODE", Some("garbage"))]));
    assert_eq!(case.verify(&[]).code, EXIT_NOT_APPROVED);
}

fn scope_narrows_what_invalidates_an_approval() {
    let case = Case::new();
    case.review(&["--paths", "a.py"], &[]);
    assert_eq!(case.verify(&["--paths", "a.py"]).code, EXIT_OK);
    case.write("other/b.txt", "out of scope change\n");
    let scoped = case.verify(&["--paths", "a.py"]);
    assert_eq!(scoped.code, EXIT_OK, "{}", scoped.stderr);
    let unscoped = case.verify(&[]);
    assert_eq!(unscoped.code, EXIT_NOT_APPROVED, "{}", unscoped.stderr);
    case.write("a.py", "print('in scope')\n");
    assert_eq!(case.verify(&["--paths", "a.py"]).code, EXIT_NOT_APPROVED);
}

fn every_repository_needs_an_approval() {
    let case = Case::new();
    let other = case.make_repo("second");
    write(&other.join("a.py"), "print('changed')\n");
    let (root, second) = (s(&case.root), s(&other));
    let both = ["--repo", root.as_str(), "--repo", second.as_str()];
    let mut review = vec!["review", "--criterion", "ship both"];
    review.extend(both);
    let result = case.run_client(&review, &[]);
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
    for repo in [&case.root, &other] {
        let entries = read_entries(&repo.join(".git").join(LEDGER_NAME));
        let states: Vec<&Value> = entries.iter().map(|e| &e["state"]).collect();
        assert_eq!(states, vec![&json!("superseded"), &json!("approved")]);
    }
    let mut verify = vec!["verify"];
    verify.extend(both);
    assert_eq!(case.run_client(&verify, &[]).code, EXIT_OK);

    // One spent approval is enough to fail the whole set.
    case.run_client(&["consume", "--repo", &second], &[]);
    let both_roots = case.run_client(&verify, &[]);
    assert_eq!(both_roots.code, EXIT_NOT_APPROVED, "{}", both_roots.stderr);
    let payload = single_json_line(&both_roots);
    assert!(
        payload["reason"]
            .as_str()
            .expect("reason")
            .contains("consumed"),
        "{payload}"
    );
    // The fingerprint covers the whole reviewed set, so one repository on
    // its own never validates an approval recorded for two.
    let single = case.verify(&[]);
    assert_eq!(single.code, EXIT_NOT_APPROVED, "{}", single.stderr);
    let payload = single_json_line(&single);
    assert!(
        payload["reason"]
            .as_str()
            .expect("reason")
            .contains("changed since"),
        "{payload}"
    );
}

fn consume_and_revoke_accept_every_reviewed_repository() {
    let case = Case::new();
    let other = case.make_repo("second");
    write(&other.join("a.py"), "print('changed')\n");
    let (root, second) = (s(&case.root), s(&other));
    let both = ["--repo", root.as_str(), "--repo", second.as_str()];
    for (command, state) in [("consume", "consumed"), ("revoke", "superseded")] {
        let mut review = vec!["review", "--criterion", "ship both"];
        review.extend(both);
        assert_eq!(
            case.run_client(&review, &[]).code,
            EXIT_OK,
            "command={command}"
        );
        let mut close = vec![command];
        close.extend(both);
        let closed = case.run_client(&close, &[]);
        assert_eq!(closed.code, EXIT_OK, "command={command}: {}", closed.stderr);
        assert_eq!(
            single_json_line(&closed)["state"],
            json!(state),
            "command={command}"
        );
        let mut verify = vec!["verify"];
        verify.extend(both);
        assert_eq!(
            case.run_client(&verify, &[]).code,
            EXIT_NOT_APPROVED,
            "command={command}"
        );
    }
}

fn status_falls_back_to_the_working_directory() {
    let case = Case::new();
    case.review(&[], &[]);
    let status = case.run_client_in(&["status"], Some(&case.root), &[]);
    assert_eq!(status.code, EXIT_OK, "{}", status.stderr);
    assert_eq!(
        single_json_line(&status)["repositories"][0]["root"],
        json!(s(&case.root))
    );
}

// ---------------------------------------------------------------------------
// LauncherTests
// ---------------------------------------------------------------------------

fn the_reviewer_receives_the_request_and_drives_the_review() {
    let case = Case::new();
    let seen = case.base.join("seen.json");
    let seen_text = s(&seen);
    let result = case.review(
        &[],
        &[("STUB_REQUEST", Some(&seen_text)), (TIMEOUT_ENV, Some("0"))],
    );
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
    assert_eq!(single_json_line(&result)["decision"], json!("approve"));
    let recorded: Value = serde_json::from_str(&read(&seen)).expect("JSON");
    assert_eq!(
        recorded["argv"].as_array().expect("argv")[..2],
        [json!(s(&case.launcher)), json!("--review")]
    );
    let request = &recorded["request"];
    assert_eq!(request["version"], json!(PROTOCOL_VERSION));
    assert_eq!(request["criterion"], json!("ship it"));
    assert_eq!(request["repositories"], json!([{"root": s(&case.root)}]));
    assert_eq!(request["review_id"], single_json_line(&result)["review_id"]);
    assert_eq!(case.verify(&[]).code, EXIT_OK);
}

fn reviewer_failure_is_recorded_as_an_error() {
    let case = Case::new();
    let result = case.review(&[], &[("STUB_MODE", Some("fail"))]);
    case.assert_no_approval(&result);
    assert_eq!(case.states().last(), Some(&json!("error")));
    let error = case.entries().last().expect("entry")["error"]
        .as_str()
        .expect("error")
        .to_string();
    assert!(
        error.contains("not open or is not a Git worktree"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// TimeoutTests
// ---------------------------------------------------------------------------

fn timeout_environment() {
    let case = Case::new();
    for (value, message) in [
        ("-1", "must not be negative"),
        ("soon", "must be a whole number"),
    ] {
        let result = case.review(&[], &[(TIMEOUT_ENV, Some(value))]);
        assert_eq!(result.code, EXIT_ERROR, "value={value}: {}", result.stderr);
        assert_eq!(
            result.stdout, "",
            "value={value}: a failing review must not print an approval"
        );
        assert_ne!(
            case.states().last(),
            Some(&json!("approved")),
            "value={value}: an approval was recorded"
        );
        assert_eq!(case.verify(&[]).code, EXIT_NOT_APPROVED, "value={value}");
        assert!(
            result.stderr.contains(TIMEOUT_ENV),
            "value={value}: {}",
            result.stderr
        );
        assert!(
            result.stderr.contains(message),
            "value={value}: {}",
            result.stderr
        );
    }
    for value in ["5", "0"] {
        let result = case.review(&[], &[(TIMEOUT_ENV, Some(value))]);
        assert_eq!(result.code, EXIT_OK, "value={value}: {}", result.stderr);
    }
}

fn a_reviewer_that_outlives_the_timeout_is_an_error() {
    let case = Case::new();
    let result = case.review(&[], &[("STUB_SLEEP", Some("10")), (TIMEOUT_ENV, Some("1"))]);
    case.assert_no_approval(&result);
    assert!(
        result.stderr.contains("timed out before a human decided"),
        "{}",
        result.stderr
    );
}

fn launcher_failure_on_a_missing_binary() {
    let case = Case::new();
    let result = case.run_client(
        &["review", "--criterion", "x", "--repo", &s(&case.root)],
        &[(LAUNCHER_ENV, Some("/nonexistent/rdg"))],
    );
    case.assert_no_approval(&result);
    assert!(
        result.stderr.contains("unable to run the reviewer"),
        "{}",
        result.stderr
    );
}

// ---------------------------------------------------------------------------
// PackagingTests
// ---------------------------------------------------------------------------

fn client_is_an_executable_file() {
    let path = binary();
    assert!(path.is_file(), "build the client first: {}", path.display());
    let path_c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).expect("path");
    // SAFETY: `path_c` is a valid NUL-terminated string for the duration of the call.
    let executable = unsafe { libc::access(path_c.as_ptr(), libc::X_OK) } == 0;
    assert!(executable, "{} must be executable", path.display());
}

fn client_reports_usage_without_a_subcommand() {
    let completed = run(&mut senpai(Vec::<String>::new()));
    assert_eq!(completed.code, EXIT_ERROR);
    assert_eq!(completed.stdout, "");
    assert!(completed.stderr.contains("usage:"), "{}", completed.stderr);
}

// ---------------------------------------------------------------------------
// ReviewerResolutionTests
// ---------------------------------------------------------------------------

fn assert_html_reviewer_runs(case: &Case, extra: Env) {
    let root = s(&case.root);
    let (process, url) = case.start_html(
        senpai(["review", "--criterion", "ship it", "--repo", &root]),
        extra,
    );
    let files = served(&url)["repositories"][0]["files"]
        .as_array()
        .expect("files")
        .clone();
    assert_eq!(
        paths_of(&files),
        vec!["a.py", "other/b.txt", "untracked.txt"]
    );
    decide(&url, "cancelled");
    let run = process.communicate(30);
    assert_eq!(run.code, EXIT_NOT_APPROVED, "{}", run.stderr);
    let stdout: Value = serde_json::from_str(&run.stdout).expect("JSON");
    assert_eq!(stdout["decision"], json!("cancelled"));
    assert_eq!(case.states().last(), Some(&json!("cancelled")));
}

fn html_is_the_default_reviewer() {
    let case = Case::new();
    assert_html_reviewer_runs(&case, &[(REVIEWER_ENV, None), (LAUNCHER_ENV, None)]);
}

fn reviewer_named_html_runs_the_bundled_page() {
    let case = Case::new();
    assert_html_reviewer_runs(
        &case,
        &[
            (REVIEWER_ENV, Some("html")),
            (LAUNCHER_ENV, Some("/nonexistent/rdg")),
        ],
    );
}

fn rdg_reviewer_resolves_to_the_binary_on_the_path() {
    let case = Case::new();
    let tools = case.base.join("tools");
    std::fs::create_dir(&tools).expect("mkdir");
    install_stub(&tools.join("rdg"));
    let seen = case.base.join("seen.json");
    let seen_text = s(&seen);
    let path = format!(
        "{}:{}",
        tools.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let result = case.review(
        &[],
        &[
            ("STUB_REQUEST", Some(&seen_text)),
            ("PATH", Some(&path)),
            (REVIEWER_ENV, Some("rdg")),
        ],
    );
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
    let recorded: Value = serde_json::from_str(&read(&seen)).expect("JSON");
    let argv0 = recorded["argv"][0].as_str().expect("argv0");
    assert_eq!(argv0.rsplit('/').next(), Some("rdg"), "{argv0}");
}

fn reviewer_environment_wins_over_the_legacy_variable() {
    let case = Case::new();
    let launcher = s(&case.launcher);
    let result = case.review(
        &[],
        &[
            (REVIEWER_ENV, Some(&launcher)),
            (LAUNCHER_ENV, Some("/legacy/reviewer")),
        ],
    );
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
    let result = case.review(
        &[],
        &[
            (REVIEWER_ENV, Some("/nonexistent/reviewer")),
            (LAUNCHER_ENV, Some(&launcher)),
        ],
    );
    case.assert_no_approval(&result);
    assert!(
        result.stderr.contains("unable to run the reviewer"),
        "{}",
        result.stderr
    );
}

fn legacy_reviewer_variable_is_still_honored() {
    let case = Case::new();
    let launcher = s(&case.launcher);
    let result = case.review(
        &[],
        &[(REVIEWER_ENV, None), (LAUNCHER_ENV, Some(&launcher))],
    );
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
    let result = case.review(
        &[],
        &[
            (REVIEWER_ENV, None),
            (LAUNCHER_ENV, Some("/legacy/reviewer")),
        ],
    );
    case.assert_no_approval(&result);
    assert!(
        result.stderr.contains("unable to run the reviewer"),
        "{}",
        result.stderr
    );
}

fn blank_reviewer_values_fall_through() {
    let case = Case::new();
    assert_html_reviewer_runs(
        &case,
        &[(REVIEWER_ENV, Some("   ")), (LAUNCHER_ENV, Some(""))],
    );
}

fn an_explicit_launcher_path_still_wins() {
    let case = Case::new();
    // A path with a space only works if it is used as-is, not split into words.
    let spaced = case.base.join("reviewer dir").join("reviewer");
    std::fs::create_dir(spaced.parent().expect("parent")).expect("mkdir");
    install_stub(&spaced);
    let result = case.review(&[], &[(REVIEWER_ENV, Some(&s(&spaced)))]);
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
}

// ---------------------------------------------------------------------------
// DocumentCommandTests
// ---------------------------------------------------------------------------

const DOC_APPROVE: &str = APPROVE;
const DOC_COMMENT: &str = r#"{"version":"1","review_id":"__REVIEW_ID__","decision":"request-changes","annotations":[{"text":"name the failure mode","target":{"scope":"line","path":"__DOC__","start_line":5,"end_line":5}}]}"#;

/// `DocumentCommandTests`: a specification reviewed, approved, then spent.
struct DocCase {
    _tmp: TempDir,
    base: PathBuf,
    root: PathBuf,
    doc: PathBuf,
    launcher: PathBuf,
}

impl DocCase {
    fn new() -> Self {
        let tmp = TempDir::new();
        let base = tmp.path().to_path_buf();
        let root = base.join("repo");
        let doc = root.join("docs/specs/2026-09-27-topic.md");
        write(
            &doc,
            "# Topic\n\n## Problem\n\nSomething is wrong.\n\n- a goal\n",
        );
        git(&root, &["init", "-q", "-b", "main", "."]);
        commit(&root, "init");
        // The shell checks the flag, and `run_client` replaces `__DOC__` with
        // the document path before the stub starts.
        let launcher = base.join("stub-reviewer");
        let exe = std::env::current_exe().expect("the test binary");
        let body = format!(
            "#!/bin/sh\n[ \"$1\" = --review-doc ] || {{ echo \"expected --review-doc: $*\" >&2; exit 1; }}\nexec {} {} \"$@\"\n",
            shell_quote(&exe.to_string_lossy()),
            shell_quote(&format!("{ROLE_FLAG}reviewer"))
        );
        script(&launcher, &body);
        DocCase {
            _tmp: tmp,
            base,
            root,
            doc,
            launcher,
        }
    }

    fn run_client(&self, args: &[&str], template: &str) -> Run {
        let mut command = senpai(args);
        command
            .env("STUB_RESULT", template.replace("__DOC__", &s(&self.doc)))
            .env(REVIEWER_ENV, &self.launcher)
            .env(TIMEOUT_ENV, "60");
        run_with(&mut command, None, 90)
    }

    fn criterion_args(&self) -> Vec<String> {
        vec![
            "--criterion".into(),
            "Fix the parser".into(),
            "--doc".into(),
            s(&self.doc),
        ]
    }

    fn run_doc(&self, command: &str, extra: &[&str], template: &str) -> Run {
        let criterion = self.criterion_args();
        let mut args = vec![command];
        args.extend(criterion.iter().map(String::as_str));
        args.extend_from_slice(extra);
        self.run_client(&args, template)
    }
}

fn stdout_json(run: &Run) -> Value {
    serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {run:?}"))
}

fn an_approved_spec_verifies_and_then_stops_counting() {
    let case = DocCase::new();
    let reviewed = case.run_doc("review-doc", &[], DOC_APPROVE);
    assert_eq!(reviewed.code, EXIT_OK, "{}", reviewed.stderr);
    assert_eq!(stdout_json(&reviewed)["decision"], json!("approve"));
    let verified = case.run_doc("verify-doc", &[], DOC_APPROVE);
    assert_eq!(
        verified.code, EXIT_OK,
        "{}{}",
        verified.stdout, verified.stderr
    );
    assert_eq!(stdout_json(&verified)["approved"], json!(true));
    let consumed = case.run_doc("consume-doc", &[], DOC_APPROVE);
    assert_eq!(consumed.code, EXIT_OK, "{}", consumed.stderr);
    let after = case.run_doc("verify-doc", &[], DOC_APPROVE);
    assert_eq!(after.code, EXIT_NOT_APPROVED);
    assert_eq!(stdout_json(&after)["state"], json!("consumed"));
}

fn document_recovery_preserves_the_document_request_and_verified_decision() {
    let case = DocCase::new();
    let recovery = case
        .root
        .parent()
        .expect("parent")
        .join("document recovery");
    let result = case.run_doc(
        "review-doc",
        &["--recovery-dir", &s(&recovery)],
        DOC_APPROVE,
    );
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
    let request: Value = serde_json::from_str(&read(&recovery.join("request.json"))).expect("JSON");
    assert_eq!(request["path"], json!(s(&case.doc)));
    assert!(request.get("repositories").is_none(), "{request}");
    let saved: Value = serde_json::from_str(&read(&recovery.join("result.json"))).expect("JSON");
    assert_eq!(stdout_json(&result), saved);
    let verified = case.run_doc("verify-doc", &[], DOC_APPROVE);
    assert_eq!(verified.code, EXIT_OK, "{}", verified.stderr);
}

fn editing_the_spec_after_approval_invalidates_it() {
    let case = DocCase::new();
    let reviewed = case.run_doc("review-doc", &[], DOC_APPROVE);
    assert_eq!(reviewed.code, EXIT_OK, "{}", reviewed.stderr);
    std::fs::write(&case.doc, read(&case.doc) + "\nOne more line.\n").expect("write");
    let verified = case.run_doc("verify-doc", &[], DOC_APPROVE);
    assert_eq!(verified.code, EXIT_NOT_APPROVED);
    let payload = stdout_json(&verified);
    assert!(
        payload["reason"]
            .as_str()
            .expect("reason")
            .contains("changed since the approval"),
        "{payload}"
    );
}

fn a_different_criterion_does_not_match_the_approval() {
    let case = DocCase::new();
    let reviewed = case.run_doc("review-doc", &[], DOC_APPROVE);
    assert_eq!(reviewed.code, EXIT_OK, "{}", reviewed.stderr);
    let other = case.run_client(
        &[
            "verify-doc",
            "--criterion",
            "Something else entirely",
            "--doc",
            &s(&case.doc),
        ],
        DOC_APPROVE,
    );
    assert_eq!(other.code, EXIT_NOT_APPROVED);
    assert_eq!(stdout_json(&other)["approved"], json!(false));
}

fn verify_takes_the_criterion_from_the_ledger_when_it_is_not_repeated() {
    let case = DocCase::new();
    case.run_doc("review-doc", &[], DOC_APPROVE);
    let verified = case.run_client(&["verify-doc", "--doc", &s(&case.doc)], DOC_APPROVE);
    assert_eq!(
        verified.code, EXIT_OK,
        "{}{}",
        verified.stdout, verified.stderr
    );
}

fn requested_changes_leaves_no_approval() {
    let case = DocCase::new();
    let reviewed = case.run_doc("review-doc", &[], DOC_COMMENT);
    assert_eq!(reviewed.code, EXIT_NOT_APPROVED, "{reviewed:?}");
    let result = stdout_json(&reviewed);
    assert_eq!(result["decision"], json!("request-changes"));
    assert_eq!(result["annotations"][0]["target"]["start_line"], json!(5));
    let verified = case.run_doc("verify-doc", &[], DOC_COMMENT);
    assert_eq!(verified.code, EXIT_NOT_APPROVED);
}

fn a_diff_shaped_result_is_rejected_for_a_document_review() {
    let case = DocCase::new();
    let template = r#"{"version":"1","review_id":"__REVIEW_ID__","decision":"approve","annotations":[{"text":"wrong","target":{"scope":"line","repository":"/tmp","path":"a.py","side":"new","start_line":1,"end_line":1}}]}"#;
    let reviewed = case.run_doc("review-doc", &[], template);
    assert_eq!(
        reviewed.code, EXIT_ERROR,
        "a diff annotation is not a document comment"
    );
    assert!(
        reviewed.stderr.contains("malformed annotations"),
        "{}",
        reviewed.stderr
    );
}

fn the_ledger_names_the_document() {
    let case = DocCase::new();
    case.run_doc("review-doc", &[], DOC_APPROVE);
    let common = git(
        &case.root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    );
    let text = read(&Path::new(common.trim()).join(LEDGER_NAME));
    let entries: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON line"))
        .collect();
    assert_eq!(
        entries.last().expect("entry")["document"],
        json!(s(&case.doc))
    );
}

fn a_document_outside_a_repository_keeps_its_own_ledger() {
    let case = DocCase::new();
    let outside = case.base.join("loose");
    std::fs::create_dir(&outside).expect("mkdir");
    let loose = outside.join("notes.md");
    write(&loose, "# Notes\n");
    let reviewed = case.run_client(
        &[
            "review-doc",
            "--criterion",
            "Read the notes",
            "--doc",
            &s(&loose),
        ],
        DOC_APPROVE,
    );
    assert_eq!(reviewed.code, EXIT_OK, "{}", reviewed.stderr);
    assert!(outside.join(LEDGER_NAME).is_file());
    let verified = case.run_client(
        &[
            "verify-doc",
            "--criterion",
            "Read the notes",
            "--doc",
            &s(&loose),
        ],
        DOC_APPROVE,
    );
    assert_eq!(
        verified.code, EXIT_OK,
        "{}{}",
        verified.stdout, verified.stderr
    );
}

// ---------------------------------------------------------------------------
// GuidedReviewTests
// ---------------------------------------------------------------------------

fn assert_served_guide(expected: &Value, url: &str) {
    let served = served(url)["guide"].clone();
    assert_eq!(served["authored"], json!(true), "{served}");
    assert_eq!(served["summary"], expected["summary"]);
    let titles = |v: &Value| -> Vec<Value> {
        v["chapters"]
            .as_array()
            .expect("chapters")
            .iter()
            .map(|c| c["title"].clone())
            .collect()
    };
    assert_eq!(titles(expected), titles(&served));
}

fn status_accepts_paths_and_returns_the_scoped_guide_fingerprint() {
    let case = Case::new();
    let expected = case.guide()["fingerprint"].clone();
    let result = case.run_client(
        &["status", "--repo", &s(&case.root), "--paths", "a.py"],
        &[],
    );
    assert_eq!(result.code, EXIT_NOT_APPROVED, "{}", result.stderr);
    assert_eq!(stdout_json(&result)["fingerprint"], expected);
}

fn cli_captures_guide_once_and_existing_verification_still_checks_code() {
    let case = Case::new();
    let value = case.guide();
    let path = case.base.join("guide.json");
    write(&path, &value.to_string());
    let (root, guide) = (s(&case.root), s(&path));
    let (process, url) = case.start_html(
        senpai([
            "review",
            "--criterion",
            "ship it",
            "--repo",
            &root,
            "--paths",
            "a.py",
            "--guide",
            &guide,
        ]),
        &[],
    );
    write(&path, "changed after capture");
    assert_served_guide(&value, &url);
    decide(&url, "approve");
    let run = process.communicate(30);
    assert_eq!(run.code, EXIT_OK, "{}{}", run.stdout, run.stderr);
    assert_eq!(case.verify(&["--paths", "a.py"]).code, EXIT_OK);
    case.write("a.py", "changed after approval\n");
    assert_eq!(case.verify(&["--paths", "a.py"]).code, EXIT_NOT_APPROVED);
}

fn inline_guide_round_trip_and_mid_review_mutation_fails_closed() {
    let case = Case::new();
    let payload = case.guided_request(Some(case.guide()));
    let (process, url) = case.start_html(senpai(["review", "--json", &payload.to_string()]), &[]);
    assert_served_guide(&payload["guide"], &url);
    case.write("a.py", "mutated during review\n");
    decide(&url, "approve");
    let run = process.communicate(30);
    assert_eq!(run.code, EXIT_ERROR);
    assert!(
        run.stderr.contains("changed while the human"),
        "{}",
        run.stderr
    );
    assert_eq!(run.stdout, "");
    assert_eq!(case.verify(&["--paths", "a.py"]).code, EXIT_NOT_APPROVED);
}

fn stale_unknown_and_non_html_guides_never_reach_a_launcher() {
    let case = Case::new();
    for problem in ["stale", "unknown", "backend"] {
        let mut value = case.guide();
        let mut reviewer = "html".to_string();
        match problem {
            "stale" => value["fingerprint"] = json!("b".repeat(64)),
            "unknown" => value["chapters"][0]["files"][0]["path"] = json!("untracked.txt"),
            _ => reviewer = s(&case.launcher),
        }
        let url_file = case.base.join(format!("url-{problem}"));
        let seen = case.base.join(format!("seen-{problem}.json"));
        let (url_text, seen_text) = (s(&url_file), s(&seen));
        let result = case.run_client(
            &[
                "review",
                "--json",
                &case.guided_request(Some(value)).to_string(),
            ],
            &[
                ("STUB_REQUEST", Some(&seen_text)),
                (REVIEWER_ENV, Some(&reviewer)),
                (NO_BROWSER_ENV, Some("1")),
                (URL_FILE_ENV, Some(&url_text)),
            ],
        );
        assert_eq!(
            result.code, EXIT_ERROR,
            "problem={problem}: {}",
            result.stderr
        );
        assert_eq!(
            result.stdout, "",
            "problem={problem}: a failing review must not print an approval"
        );
        assert_ne!(
            case.states().last(),
            Some(&json!("approved")),
            "problem={problem}: an approval was recorded"
        );
        assert_eq!(
            case.verify(&[]).code,
            EXIT_NOT_APPROVED,
            "problem={problem}"
        );
        assert!(
            !url_file.exists(),
            "problem={problem}: an invalid guide reached the HTML reviewer"
        );
        assert!(
            !seen.exists(),
            "problem={problem}: an invalid guide reached the launcher"
        );
        if problem == "backend" {
            assert!(
                result.stderr.contains("guided reviews require"),
                "problem={problem}: {}",
                result.stderr
            );
        }
    }
}

fn bad_files_and_conflicting_flags_report_local_errors() {
    let case = Case::new();
    let path = case.base.join("guide.json");
    let url_file = case.base.join("url");
    let (path_text, url_text) = (s(&path), s(&url_file));
    let html: &[(&str, Option<&str>)] = &[
        (REVIEWER_ENV, Some("html")),
        (NO_BROWSER_ENV, Some("1")),
        (URL_FILE_ENV, Some(&url_text)),
    ];
    let big = vec![b'x'; MAX_GUIDE_BYTES + 1];
    for content in [b"not JSON".as_slice(), b"\xff".as_slice(), big.as_slice()] {
        let label = String::from_utf8_lossy(&content[..content.len().min(10)]).into_owned();
        std::fs::write(&path, content).expect("write");
        let result = case.review(&["--guide", &path_text], html);
        assert_eq!(
            result.code, EXIT_ERROR,
            "content={label:?}: {}",
            result.stdout
        );
        assert!(
            result.stderr.contains("guide"),
            "content={label:?}: {}",
            result.stderr
        );
    }
    std::fs::remove_file(&path).expect("unlink");
    let result = case.review(&["--guide", &path_text], html);
    assert_eq!(result.code, EXIT_ERROR, "{}", result.stdout);
    assert!(
        result.stderr.contains("unable to read guide"),
        "{}",
        result.stderr
    );
    let request = case.guided_request(None);
    let result = case.run_client(
        &[
            "review",
            "--json",
            &request.to_string(),
            "--guide",
            &path_text,
        ],
        html,
    );
    assert_eq!(result.code, EXIT_ERROR, "{}", result.stdout);
    assert!(result.stderr.contains("--guide"), "{}", result.stderr);
    assert!(!url_file.exists(), "a bad guide reached the HTML reviewer");
}

// ---------------------------------------------------------------------------
// RecoveryFlowTests
// ---------------------------------------------------------------------------

fn validated_result_and_exact_request_are_retained() {
    let case = Case::new();
    let recovery = s(&case.recovery());
    let result = case.review(&["--recovery-dir", &recovery, "--paths", "a.py"], &[]);
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
    let decision = single_json_line(&result);
    assert_eq!(decision, case.read_recovery("result.json"));
    let request = case.read_recovery("request.json");
    assert_eq!(decision["review_id"], request["review_id"]);
    assert_eq!(
        request["repositories"],
        json!([{"root": s(&case.root), "paths": ["a.py"]}])
    );
    let state = case.read_recovery("state.json");
    assert_eq!(
        (&state["state"], &state["decision"], &state["exit_code"]),
        (&json!("finished"), &json!("approve"), &json!(0))
    );
    assert_eq!(case.verify(&["--paths", "a.py"]).code, EXIT_OK);
    assert_eq!(mode(&case.recovery()), 0o700);
    for artifact in std::fs::read_dir(case.recovery()).expect("read_dir") {
        let artifact = artifact.expect("entry").path();
        assert_eq!(mode(&artifact), 0o600, "{}", artifact.display());
    }
}

fn nonapproval_is_a_finished_decision_not_a_launcher_error() {
    let case = Case::new();
    let recovery = s(&case.recovery());
    let result = case.review(
        &["--recovery-dir", &recovery],
        &[("STUB_RESULT", Some(REJECT))],
    );
    assert_eq!(result.code, EXIT_NOT_APPROVED, "{}", result.stderr);
    assert_eq!(
        case.read_recovery("result.json")["decision"],
        json!("request-changes")
    );
    let state = case.read_recovery("state.json");
    assert_eq!(
        (&state["state"], &state["exit_code"]),
        (&json!("finished"), &json!(1))
    );
    assert_eq!(case.verify(&[]).code, EXIT_NOT_APPROVED);
}

fn malformed_result_retains_error_without_an_approval_artifact() {
    let case = Case::new();
    let recovery = s(&case.recovery());
    let result = case.review(
        &["--recovery-dir", &recovery],
        &[("STUB_MODE", Some("garbage"))],
    );
    case.assert_no_approval(&result);
    assert!(!case.recovery().join("result.json").exists());
    let state = case.read_recovery("state.json");
    assert_eq!(
        (&state["state"], &state["exit_code"]),
        (&json!("error"), &json!(2))
    );
    assert!(
        state["error"].as_str().expect("error").contains("JSON"),
        "{state}"
    );
}

fn approval_for_changed_inputs_is_not_retained_as_a_result() {
    let case = Case::new();
    let (recovery, target) = (s(&case.recovery()), s(&case.root.join("a.py")));
    let result = case.review(
        &["--recovery-dir", &recovery],
        &[
            ("STUB_MODE", Some("mutate")),
            ("STUB_MUTATE", Some(&target)),
        ],
    );
    case.assert_no_approval(&result);
    assert!(!case.recovery().join("result.json").exists());
    let state = case.read_recovery("state.json");
    assert!(
        state["error"]
            .as_str()
            .expect("error")
            .contains("changed while the human"),
        "{state}"
    );
}

fn recovery_storage_failure_cannot_leave_a_verifiable_approval() {
    let case = Case::new();
    let document = case.root.join("plan.md");
    write(&document, "# Approved plan\n");
    let (root, doc) = (s(&case.root), s(&document));
    for command in ["review", "review-doc"] {
        for artifact in ["result.json", "state.json"] {
            let label = format!("command={command} artifact={artifact}");
            let directory = case.base.join(format!("{command}-{artifact}"));
            let scope: [&str; 2] = if command == "review" {
                ["--repo", &root]
            } else {
                ["--doc", &doc]
            };
            let (dir_text, block) = (s(&directory), s(&directory.join(artifact)));
            let mut args = vec![command, "--criterion", "ship it"];
            args.extend(scope);
            args.extend(["--recovery-dir", &dir_text]);
            let result = case.run_client(&args, &[("STUB_BLOCK", Some(&block))]);
            assert_eq!(result.code, EXIT_ERROR, "{label}: {}", result.stderr);
            assert_eq!(result.stdout, "", "{label}");
            let verify_command = if command == "review" {
                "verify"
            } else {
                "verify-doc"
            };
            let mut verify = vec![verify_command];
            verify.extend(scope);
            let verified = case.run_client(&verify, &[]);
            assert_eq!(
                verified.code, EXIT_NOT_APPROVED,
                "{label}: {}",
                verified.stdout
            );
            if artifact == "result.json" {
                let state: Value =
                    serde_json::from_str(&read(&directory.join("state.json"))).expect("JSON");
                assert_eq!(state["state"], json!("error"), "{label}");
            }
        }
    }
}

fn complete_multi_repository_scope_is_retained_and_verified_together() {
    let case = Case::new();
    let other = case.make_repo("second repository");
    write(&other.join("a.py"), "second change\n");
    let (root, second, recovery) = (s(&case.root), s(&other), s(&case.recovery()));
    let result = case.run_client(
        &[
            "review",
            "--criterion",
            "ship both",
            "--repo",
            &root,
            "--repo",
            &second,
            "--recovery-dir",
            &recovery,
        ],
        &[],
    );
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
    let verify = case.run_client(&["verify", "--repo", &root, "--repo", &second], &[]);
    assert_eq!(verify.code, EXIT_OK, "{}", verify.stderr);
    write(&other.join("a.py"), "changed after approval\n");
    assert_eq!(
        case.run_client(&["verify", "--repo", &root, "--repo", &second], &[])
            .code,
        EXIT_NOT_APPROVED
    );
}

fn recovery_contents(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| {
            let path = e.expect("entry").path();
            (
                path.file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned(),
                std::fs::read(&path).expect("read"),
            )
        })
        .collect()
}

fn existing_run_is_not_overwritten_or_relaunched() {
    let case = Case::new();
    let recovery = s(&case.recovery());
    let first = case.review(&["--recovery-dir", &recovery], &[]);
    assert_eq!(first.code, EXIT_OK, "{}", first.stderr);
    let saved = recovery_contents(&case.recovery());
    let second = case.review(&["--recovery-dir", &recovery], &[]);
    assert_eq!(second.code, EXIT_ERROR, "{}", second.stderr);
    assert!(second.stderr.contains("recover"), "{}", second.stderr);
    assert_eq!(saved, recovery_contents(&case.recovery()));
    assert_eq!(case.verify(&[]).code, EXIT_OK);
}

fn recovery_inside_reviewed_tree_or_public_directory_is_rejected() {
    let case = Case::new();
    let public = case.base.join("public");
    std::fs::DirBuilder::new()
        .mode(0o755)
        .create(&public)
        .expect("mkdir");
    for path in [case.root.join("recovery"), public] {
        let result = case.review(&["--recovery-dir", &s(&path)], &[]);
        assert_eq!(
            result.code,
            EXIT_ERROR,
            "path={}: {}",
            path.display(),
            result.stderr
        );
        assert_eq!(
            case.entries(),
            Vec::<Value>::new(),
            "path={}",
            path.display()
        );
    }
    assert!(!case.root.join("recovery").exists());
}

/// Ends the client: SIGTERM, then a forced kill of the
/// client and the reviewer's process group if it does not finish.
struct Cancellable {
    child: Option<Child>,
    recovery: PathBuf,
}

impl Drop for Cancellable {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            // SAFETY: plain signal delivery to a child this test started.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().ok().flatten().is_none() {
            if Instant::now() >= deadline {
                let pid = std::fs::read_to_string(self.recovery.join("process.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                    .and_then(|v| v["reviewer_pid"].as_i64());
                if let Some(pid) = pid {
                    // SAFETY: signal delivery; a missing group is ignored.
                    unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
                }
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.wait();
    }
}

fn live_diagnostics_arrive_before_decision_and_cancellation_reaps_reviewer() {
    let case = Case::new();
    let fifo = case.base.join("decision");
    let fifo_c = std::ffi::CString::new(s(&fifo)).expect("path");
    // SAFETY: `fifo_c` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o666) }, 0, "mkfifo");
    std::fs::remove_file(&case.launcher).expect("unlink the stub");
    script(
        &case.launcher,
        "#!/bin/sh\necho 'review page ready' >&2\nIFS= read -r _ < \"$DECISION_FIFO\"\n",
    );
    let (root, recovery, fifo_text) = (s(&case.root), s(&case.recovery()), s(&fifo));
    let mut command = senpai([
        "review",
        "--criterion",
        "ship it",
        "--repo",
        &root,
        "--recovery-dir",
        &recovery,
    ]);
    case.env(&mut command, &[("DECISION_FIFO", Some(&fifo_text))]);
    let mut child = spawn(&mut command);
    let stdout = child.stdout.take().map(drain).expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let mut guard = Cancellable {
        child: Some(child),
        recovery: case.recovery(),
    };

    let (sender, receiver) = mpsc::channel();
    let rest = std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        let _ = reader.read_until(b'\n', &mut line);
        let _ = sender.send(line);
        let mut buffer = Vec::new();
        let _ = reader.read_to_end(&mut buffer);
        buffer
    });
    let line = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("reviewer diagnostics were buffered until exit");
    assert_eq!(line, b"review page ready\n");
    let child = guard.child.as_mut().expect("child");
    assert!(child.try_wait().expect("poll").is_none());
    let state = case.read_recovery("state.json");
    assert_eq!(state["state"], json!("reviewing"));
    let reviewer_pid = case.read_recovery("process.json")["reviewer_pid"]
        .as_i64()
        .expect("reviewer_pid") as i32;
    // SAFETY: plain signal delivery to a child this test started.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let mut child = guard.child.take().expect("child");
    wait_exit(&mut child, 10);
    let run = Run::from(std::process::Output {
        status: child.wait().expect("wait"),
        stdout: stdout.join().unwrap_or_default(),
        stderr: rest.join().unwrap_or_default(),
    });
    assert_eq!(run.code, EXIT_ERROR, "{}", run.stderr);
    assert_eq!(run.stdout, "");
    assert_eq!(case.read_recovery("state.json")["state"], json!("error"));
    // SAFETY: signal 0 only checks whether the process exists.
    let alive = unsafe { libc::kill(reviewer_pid, 0) };
    let errno = std::io::Error::last_os_error().raw_os_error();
    assert!(
        alive != 0 && errno == Some(libc::ESRCH),
        "the reviewer {reviewer_pid} is still alive"
    );
    assert_eq!(case.verify(&[]).code, EXIT_NOT_APPROVED);
}

// ---------------------------------------------------------------------------
// PreflightTests
// ---------------------------------------------------------------------------

fn valid_guide_preflight_does_not_change_existing_approval() {
    let case = Case::new();
    assert_eq!(case.review(&["--paths", "a.py"], &[]).code, EXIT_OK);
    let ledger = std::fs::read(case.ledger_path()).expect("ledger");
    let guide = case.base.join("guide.json");
    write(&guide, &case.guide().to_string());
    let result = case.run_client(
        &[
            "preflight",
            "--criterion",
            "ship it",
            "--repo",
            &s(&case.root),
            "--paths",
            "a.py",
            "--guide",
            &s(&guide),
        ],
        &[(REVIEWER_ENV, Some("html"))],
    );
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
    let payload = single_json_line(&result);
    assert_eq!(payload["fingerprint"], case.guide()["fingerprint"]);
    assert!(payload.get("decision").is_none(), "{payload}");
    assert_eq!(ledger, std::fs::read(case.ledger_path()).expect("ledger"));
}

fn duplicate_stale_and_out_of_scope_guides_fail_without_ledger_mutation() {
    let case = Case::new();
    for problem in ["duplicate", "stale", "scope"] {
        let mut value = case.guide();
        match problem {
            "duplicate" => {
                let first = value["chapters"][0].clone();
                value["chapters"]
                    .as_array_mut()
                    .expect("chapters")
                    .push(first);
            }
            "stale" => value["fingerprint"] = json!("0".repeat(64)),
            _ => value["chapters"][0]["files"][0]["path"] = json!("other/b.txt"),
        }
        let guide = case.base.join("guide.json");
        write(&guide, &value.to_string());
        let result = case.run_client(
            &[
                "preflight",
                "--criterion",
                "ship it",
                "--repo",
                &s(&case.root),
                "--paths",
                "a.py",
                "--guide",
                &s(&guide),
            ],
            &[(REVIEWER_ENV, Some("html"))],
        );
        assert_eq!(
            result.code, EXIT_ERROR,
            "problem={problem}: {}",
            result.stderr
        );
        let expected = match problem {
            "duplicate" => "duplicate",
            "stale" => "stale",
            _ => "outside the captured diff",
        };
        assert!(
            result.stderr.contains(expected),
            "problem={problem}: {}",
            result.stderr
        );
        assert_eq!(result.stdout, "", "problem={problem}");
        assert_eq!(case.entries(), Vec::<Value>::new(), "problem={problem}");
    }
}

// ---------------------------------------------------------------------------
// RangeTests
// ---------------------------------------------------------------------------

/// A case whose changes are committed on `feature`, one commit ahead of
/// `main`, with a clean working tree.
fn feature_case() -> Case {
    let case = Case::new();
    git(&case.root, &["checkout", "-q", "-b", "feature"]);
    case.commit("feature work");
    case
}

fn range_fingerprint(case: &Case, range: &[&str]) -> String {
    let root = s(&case.root);
    let mut args = vec!["preflight", "--criterion", "ship it", "--repo", &root];
    args.extend_from_slice(range);
    let result = case.run_client(&args, &[]);
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
    single_json_line(&result)["fingerprint"].as_str().expect("a fingerprint").to_string()
}

fn served_range(case: &Case, base: &str, head: Option<&str>) -> Value {
    let mut repository = json!({"root": s(&case.root), "base": base});
    if let Some(head) = head {
        repository["head"] = json!(head);
    }
    case.served_repository(repository)
}

fn base_review_includes_committed_and_uncommitted_changes() {
    let case = feature_case();
    assert_eq!(case.served_files(None, None), Vec::<Value>::new(), "nothing is uncommitted");
    let served = served_range(&case, "main", None);
    assert_eq!(paths_of(served["files"].as_array().expect("files")), ["a.py", "other/b.txt", "untracked.txt"]);
    let main = git(&case.root, &["rev-parse", "main"]).trim().to_string();
    assert_eq!(served["range"]["baseline"], json!(main));
    assert_eq!(served["range"]["head"], Value::Null);
    let committed = range_fingerprint(&case, &["--base", "main"]);
    assert_ne!(committed, case.fingerprint(None, None));
    case.write("a.py", "print('three')\n");
    case.write("fresh.txt", "not yet tracked\n");
    let files = served_range(&case, "main", None)["files"].as_array().expect("files").clone();
    assert_eq!(paths_of(&files), ["a.py", "fresh.txt", "other/b.txt", "untracked.txt"]);
    assert_ne!(committed, range_fingerprint(&case, &["--base", "main"]));
}

fn base_approval_verifies_only_with_the_same_base() {
    let case = feature_case();
    case.write("a.py", "print('three')\n");
    let result = case.review(&["--base", "main"], &[]);
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
    let entry = case.entries().last().expect("entry").clone();
    assert_eq!(entry["state"], json!("approved"));
    assert_eq!(entry["repositories"][0]["base"], json!("main"));
    assert_eq!(case.verify(&["--base", "main"]).code, EXIT_OK);
    assert_eq!(case.verify(&[]).code, EXIT_NOT_APPROVED, "a HEAD review is a different range");
    case.commit("commit the reviewed change");
    assert_eq!(case.verify(&["--base", "main"]).code, EXIT_OK, "committing reviewed content keeps the range");
    case.write("a.py", "print('four')\n");
    assert_eq!(case.verify(&["--base", "main"]).code, EXIT_NOT_APPROVED);
}

fn head_review_ignores_the_working_tree() {
    let case = feature_case();
    let range = ["--base", "main", "--head", "feature"];
    let fixed = range_fingerprint(&case, &range);
    assert_ne!(fixed, range_fingerprint(&case, &["--base", "main"]), "a commit range has its own fingerprint");
    case.write("a.py", "print('uncommitted')\n");
    case.write("fresh.txt", "not yet tracked\n");
    assert_eq!(fixed, range_fingerprint(&case, &range));
    let served = served_range(&case, "main", Some("feature"));
    let files = served["files"].as_array().expect("files");
    assert_eq!(paths_of(files), ["a.py", "other/b.txt", "untracked.txt"]);
    let a = files.iter().find(|f| f["path"] == json!("a.py")).expect("a.py");
    assert_eq!(a["text"], json!("print('two')\n"), "content comes from the head commit");
    let feature = git(&case.root, &["rev-parse", "feature"]).trim().to_string();
    assert_eq!(served["range"]["head_commit"], json!(feature));
    let result = case.review(&range, &[]);
    assert_eq!(result.code, EXIT_OK, "{}", result.stderr);
    let root = s(&case.root);
    let mut verify = vec!["verify", "--repo", root.as_str()];
    verify.extend_from_slice(&range);
    assert_eq!(case.run_client(&verify, &[]).code, EXIT_OK);
    case.write("feature-only.txt", "x\n");
    case.commit("more work");
    assert_eq!(case.run_client(&verify, &[]).code, EXIT_NOT_APPROVED, "a moved head is a new range");
}

fn head_moving_during_review_denies_approval() {
    let case = feature_case();
    let root = s(&case.root);
    let main = git(&case.root, &["rev-parse", "main"]).trim().to_string();
    let move_head = ["-C", root.as_str(), "update-ref", "refs/heads/feature", main.as_str()].join("\t");
    let result = case.review(
        &["--base", "main", "--head", "feature"],
        &[("STUB_MODE", Some("git")), ("STUB_GIT", Some(&move_head))],
    );
    case.assert_no_approval(&result);
    assert!(result.stderr.contains("changed while the human was deciding"), "{}", result.stderr);
}

fn range_usage_errors_and_unknown_revisions_fail_closed() {
    let case = feature_case();
    let root = s(&case.root);
    let other = case.make_repo("other");
    let other = s(&other);
    let unrelated = git(&case.root, &["commit-tree", EMPTY_TREE, "-m", "unrelated"]).trim().to_string();
    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("head without base", vec!["review", "--criterion", "x", "--repo", &root, "--head", "feature"]),
        ("base with two repos", vec!["review", "--criterion", "x", "--repo", &root, "--repo", &other, "--base", "main"]),
        ("option-like base", vec!["review", "--criterion", "x", "--repo", &root, "--base=-p"]),
        ("json with base", vec!["review", "--json", "{}", "--base", "main"]),
        ("unknown base", vec!["review", "--criterion", "x", "--repo", &root, "--base", "no-such-branch"]),
        ("unknown head", vec!["review", "--criterion", "x", "--repo", &root, "--base", "main", "--head", "nope"]),
        ("no merge base", vec!["review", "--criterion", "x", "--repo", &root, "--base", &unrelated]),
        ("verify unknown base", vec!["verify", "--repo", &root, "--base", "no-such-branch"]),
    ];
    for (label, argv) in cases {
        let result = case.run_client(&argv, &[]);
        assert_eq!(result.code, EXIT_ERROR, "case={label}: {}", result.stderr);
        assert_eq!(result.stdout, "", "case={label}");
    }
    assert!(!case.states().contains(&json!("approved")));
}

fn base_moving_during_review_denies_approval() {
    let case = feature_case();
    case.write("a.py", "print('three')\n");
    let root = s(&case.root);
    let feature = git(&case.root, &["rev-parse", "feature"]).trim().to_string();
    let move_base = ["-C", root.as_str(), "update-ref", "refs/heads/main", feature.as_str()].join("\t");
    let result = case.review(&["--base", "main"], &[("STUB_MODE", Some("git")), ("STUB_GIT", Some(&move_base))]);
    case.assert_no_approval(&result);
    assert!(result.stderr.contains("changed while the human was deciding"), "{}", result.stderr);
}

fn added_lines(file: &Value) -> Vec<String> {
    file["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .filter(|l| l["type"] == json!("add"))
        .map(|l| l["text"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn range_reads_pathspec_like_names_literally() {
    let case = feature_case();
    case.write(":(icase)B.txt", "evil\n");
    case.commit("a pathspec-like name");
    let served = served_range(&case, "main", Some("feature"));
    let files = served["files"].as_array().expect("files");
    let odd = files.iter().find(|f| f["path"] == json!(":(icase)B.txt")).expect("the odd file");
    assert_eq!(odd["status"], json!("added"));
    assert_eq!(added_lines(odd), ["evil"]);
}

fn range_scope_keeps_renames_out_of_scope() {
    let case = feature_case();
    case.write("other/keep.txt", "one\ntwo\nthree\nfour\nfive\n");
    case.commit("add keep");
    let base = git(&case.root, &["rev-parse", "HEAD"]).trim().to_string();
    git(&case.root, &["mv", "other/keep.txt", "moved.txt"]);
    case.commit("move keep out of other");
    let mut repository = json!({"root": s(&case.root), "base": base, "head": "feature", "paths": ["other"]});
    let files = case.served_repository(repository.clone())["files"].as_array().expect("files").clone();
    assert_eq!(paths_of(&files), ["moved.txt"]);
    assert_eq!(files[0]["old_path"], json!("other/keep.txt"), "the rename out of scope is shown");
    let before = range_fingerprint(&case, &["--base", &base, "--paths", "other"]);
    case.write("other/keep.txt", "restored\n");
    assert_ne!(before, range_fingerprint(&case, &["--base", &base, "--paths", "other"]));
    repository.as_object_mut().expect("object").remove("head");
    let leaves = case.served_repository(repository)["files"].as_array().expect("files").clone();
    assert!(!leaves.is_empty(), "the working-tree range shows the scope's changes");
}

fn range_shows_ignored_submodule_changes_and_deletions() {
    let case = feature_case();
    let feature = git(&case.root, &["rev-parse", "feature"]).trim().to_string();
    let main = git(&case.root, &["rev-parse", "main"]).trim().to_string();
    git(&case.root, &["checkout", "-q", "main"]);
    case.write(".gitmodules", "[submodule \"sub\"]\n\tpath = sub\n\turl = ./sub\n\tignore = all\n");
    git(&case.root, &["add", ".gitmodules"]);
    git(&case.root, &["update-index", "--add", "--cacheinfo", &format!("160000,{main},sub")]);
    git(&case.root, &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "add sub"]);
    git(&case.root, &["checkout", "-q", "-b", "bump"]);
    git(&case.root, &["update-index", "--cacheinfo", &format!("160000,{feature},sub")]);
    git(&case.root, &["rm", "-q", "a.py"]);
    git(&case.root, &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "bump sub, drop a.py"]);
    let files = served_range(&case, "main", Some("bump"))["files"].as_array().expect("files").clone();
    assert_eq!(paths_of(&files), ["a.py", "sub"]);
    let a = files.iter().find(|f| f["path"] == json!("a.py")).expect("a.py");
    assert_eq!(a["status"], json!("deleted"));
    let sub = files.iter().find(|f| f["path"] == json!("sub")).expect("sub");
    assert!(added_lines(sub).iter().any(|l| l.contains(&feature)), "{sub}");
}
