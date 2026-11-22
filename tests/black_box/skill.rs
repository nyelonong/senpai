//! `senpai skill`: the agent skill reaches each installed agent, and nothing
//! senpai does not own is replaced or removed.

use std::path::Path;

use crate::support::*;

pub const TESTS: &[(&str, fn())] = &[
    ("install_links_the_skill_for_installed_agents_only", install_links_the_skill_for_installed_agents_only),
    ("foreign_files_are_reported_and_kept", foreign_files_are_reported_and_kept),
    ("check_reports_stale_and_missing_places", check_reports_stale_and_missing_places),
    ("remove_takes_away_only_senpais_own", remove_takes_away_only_senpais_own),
    ("usage_errors", usage_errors),
    ("a_marker_in_a_real_folder_at_a_link_place_is_never_deleted", a_marker_in_a_real_folder_at_a_link_place_is_never_deleted),
    ("files_added_to_the_gemini_copy_are_kept", files_added_to_the_gemini_copy_are_kept),
    ("a_relative_source_is_made_absolute", a_relative_source_is_made_absolute),
    ("a_link_to_a_moved_checkout_is_replaced", a_link_to_a_moved_checkout_is_replaced),
    ("remove_keeps_foreign_links", remove_keeps_foreign_links),
    ("missing_home_or_source_is_an_error", missing_home_or_source_is_an_error),
    ("the_built_in_skill_is_kept_in_senpais_home", the_built_in_skill_is_kept_in_senpais_home),
];

/// A downloaded binary has no checkout: `SENPAI_SKILL_DIR=built-in` makes this
/// one behave the same way, using the copy of SKILL.md built into it.
fn built_in(home: &Path, senpai_home: &Path, action: &str) -> Run {
    let mut command = senpai(["skill", action]);
    command.env("HOME", home).env("SENPAI_SKILL_DIR", "built-in").env(HOME_ENV, senpai_home);
    run(&mut command)
}

fn the_built_in_skill_is_kept_in_senpais_home() {
    let tmp = TempDir::new();
    let home = tmp.path().join("home");
    let senpai_home = tmp.path().join("senpai-home");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(home.join(".gemini/config")).unwrap();
    let stored = senpai_home.join("skills/senpai/SKILL.md");

    let before = built_in(&home, &senpai_home, "check");
    assert_eq!(before.code, EXIT_NOT_APPROVED, "{before:?}");
    assert_eq!((state_of(&before, "claude"), state_of(&before, "gemini")), ("missing".into(), "missing".into()));
    assert!(!senpai_home.exists(), "check never writes");

    let installed = built_in(&home, &senpai_home, "install");
    assert_eq!(installed.code, EXIT_OK, "{installed:?}");
    assert_eq!(read(&stored), read(&source().join("SKILL.md")), "the stored copy is the built-in skill");
    let link = std::fs::read_link(home.join(".claude/skills/senpai")).expect("claude gets a link");
    assert_eq!(link, std::fs::canonicalize(senpai_home.join("skills/senpai")).unwrap());
    assert_eq!(read(&home.join(".gemini/config/skills/senpai/SKILL.md")), read(&stored), "gemini gets a copy");
    assert_eq!(built_in(&home, &senpai_home, "check").code, EXIT_OK);

    write(&stored, "---\nname: senpai\n---\nan older build's skill\n");
    let older = built_in(&home, &senpai_home, "check");
    assert_eq!(older.code, EXIT_NOT_APPROVED, "{older:?}");
    assert_eq!((state_of(&older, "claude"), state_of(&older, "gemini")), ("stale".into(), "stale".into()));
    assert_eq!(built_in(&home, &senpai_home, "install").code, EXIT_OK);
    assert_eq!(read(&stored), read(&source().join("SKILL.md")), "install brings the stored copy up to date");
    assert_eq!(built_in(&home, &senpai_home, "check").code, EXIT_OK);

    let removed = built_in(&home, &senpai_home, "remove");
    assert_eq!((state_of(&removed, "claude"), state_of(&removed, "gemini")), ("removed".into(), "removed".into()));
    assert!(std::fs::symlink_metadata(home.join(".claude/skills/senpai")).is_err());
}

fn skill_from(home: &Path, action: &str, source: &Path) -> Run {
    let mut command = senpai(["skill", action]);
    command.env("HOME", home).env("SENPAI_SKILL_DIR", source);
    run(&mut command)
}

fn a_marker_in_a_real_folder_at_a_link_place_is_never_deleted() {
    let tmp = TempDir::new();
    let home = tmp.path();
    let mine = home.join(".claude/skills/senpai");
    write(&mine.join("notes.md"), "my notes\n");
    write(&mine.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(&mine.join("SKILL.md"), "---\nname: senpai\n---\nmy fork\n");
    write(&mine.join(".senpai-skill"), "");
    for action in ["check", "install", "remove"] {
        let result = skill(home, action);
        assert_eq!(state_of(&result, "claude"), "foreign", "{action}: {result:?}");
    }
    assert_eq!(read(&mine.join("notes.md")), "my notes\n");
    assert!(mine.join(".git/HEAD").is_file(), "the folder and its .git survive");
}

fn files_added_to_the_gemini_copy_are_kept() {
    let tmp = TempDir::new();
    let home = tmp.path();
    std::fs::create_dir_all(home.join(".gemini/config")).unwrap();
    let source = home.join("src-skill");
    write(&source.join("SKILL.md"), "---\nname: senpai\n---\nv1\n");
    write(&source.join("refs/guide.md"), "guide\n");
    assert_eq!(skill_from(home, "install", &source).code, EXIT_OK);
    let copy = home.join(".gemini/config/skills/senpai");
    assert_eq!(read(&copy.join("refs/guide.md")), "guide\n", "nested files are copied");
    write(&copy.join("mine.md"), "keep me\n");
    std::os::unix::fs::symlink(home.join("elsewhere"), copy.join("link")).unwrap();
    write(&source.join("SKILL.md"), "---\nname: senpai\n---\nv2\n");
    for action in ["check", "install", "remove"] {
        let result = skill_from(home, action, &source);
        assert_eq!(state_of(&result, "gemini"), "foreign", "{action}: {result:?}");
    }
    assert_eq!(read(&copy.join("mine.md")), "keep me\n");
    assert!(std::fs::symlink_metadata(copy.join("link")).unwrap().file_type().is_symlink());
    // Without the user's files, the stale copy is senpai's again and refreshes.
    std::fs::remove_file(copy.join("mine.md")).unwrap();
    std::fs::remove_file(copy.join("link")).unwrap();
    let refreshed = skill_from(home, "install", &source);
    assert_eq!((refreshed.code, state_of(&refreshed, "gemini")), (EXIT_OK, "linked".to_string()));
    assert!(read(&copy.join("SKILL.md")).contains("v2"));
    assert!(std::fs::read_dir(home.join(".gemini/config/skills")).unwrap().count() == 1, "no staging folder is left");
    // A file dropped from the source makes the copy stale; reinstalling drops it too.
    std::fs::remove_file(source.join("refs/guide.md")).unwrap();
    assert_eq!(state_of(&skill_from(home, "check", &source), "gemini"), "stale");
    assert_eq!(skill_from(home, "install", &source).code, EXIT_OK);
    assert!(!copy.join("refs/guide.md").exists());
    // An interrupted copy's staging folder is cleaned up; a stranger's is not.
    let leftover = home.join(".gemini/config/skills/.senpai-skill-999");
    write(&leftover.join("SKILL.md"), "---\nname: senpai\n---\n");
    write(&leftover.join(".senpai-skill"), &format!("{}\nSKILL.md\n", source.display()));
    let stranger = home.join(".gemini/config/skills/.senpai-skill-998");
    write(&stranger.join("notes.md"), "mine\n");
    write(&source.join("SKILL.md"), "---\nname: senpai\n---\nv3\n");
    assert_eq!(skill_from(home, "install", &source).code, EXIT_OK);
    assert!(!leftover.exists(), "senpai's own leftover is removed");
    assert_eq!(read(&stranger.join("notes.md")), "mine\n");
}

fn a_relative_source_is_made_absolute() {
    let tmp = TempDir::new();
    let home = tmp.path();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    write(&home.join("checkout/skills/senpai/SKILL.md"), "---\nname: senpai\n---\n");
    let mut command = senpai(["skill", "install"]);
    command.env("HOME", home).env("SENPAI_SKILL_DIR", "skills/senpai").current_dir(home.join("checkout"));
    let result = run(&mut command);
    assert_eq!((result.code, state_of(&result, "codex")), (EXIT_OK, "linked".to_string()), "{result:?}");
    let link = std::fs::read_link(home.join(".codex/skills/senpai")).unwrap();
    assert!(link.is_absolute(), "{link:?}");
}

fn a_link_to_a_moved_checkout_is_replaced() {
    let tmp = TempDir::new();
    let home = tmp.path();
    std::fs::create_dir_all(home.join(".pi/agent/skills")).unwrap();
    std::os::unix::fs::symlink(home.join("moved-away/skills/senpai"), home.join(".pi/agent/skills/senpai")).unwrap();
    assert_eq!(state_of(&skill(home, "check"), "pi"), "stale");
    let installed = skill(home, "install");
    assert_eq!((installed.code, state_of(&installed, "pi")), (EXIT_OK, "linked".to_string()));
    assert_eq!(std::fs::read_link(home.join(".pi/agent/skills/senpai")).unwrap(), source());
}

fn remove_keeps_foreign_links() {
    let tmp = TempDir::new();
    let home = tmp.path();
    std::fs::create_dir_all(home.join(".codex/skills")).unwrap();
    std::os::unix::fs::symlink(home.join("my-own-skill"), home.join(".codex/skills/senpai")).unwrap();
    let removed = skill(home, "remove");
    assert_eq!((removed.code, state_of(&removed, "codex")), (EXIT_OK, "foreign".to_string()));
    assert_eq!(std::fs::read_link(home.join(".codex/skills/senpai")).unwrap(), home.join("my-own-skill"));
}

fn missing_home_or_source_is_an_error() {
    let tmp = TempDir::new();
    let mut no_home = senpai(["skill", "check"]);
    no_home.env_remove("HOME");
    let result = run(&mut no_home);
    assert_eq!(result.code, EXIT_ERROR);
    assert!(result.stderr.contains("HOME is not set"), "{}", result.stderr);
    let missing = skill_from(tmp.path(), "install", &tmp.path().join("nowhere"));
    assert_eq!(missing.code, EXIT_ERROR);
    assert!(missing.stderr.contains("no senpai skill"), "{}", missing.stderr);
    let unnamed = tmp.path().join("other");
    write(&unnamed.join("SKILL.md"), "---\nname: other\n---\n");
    let wrong = skill_from(tmp.path(), "install", &unnamed);
    assert_eq!(wrong.code, EXIT_ERROR);
    assert!(wrong.stderr.contains("named senpai"), "{}", wrong.stderr);
}

fn skill(home: &Path, action: &str) -> Run {
    let mut command = senpai(["skill", action]);
    command.env("HOME", home).env_remove("SENPAI_SKILL_DIR");
    run(&mut command)
}

fn states(run: &Run) -> Vec<(String, String)> {
    run.json()["places"]
        .as_array()
        .expect("places")
        .iter()
        .map(|p| (p["agent"].as_str().unwrap().to_string(), p["state"].as_str().unwrap().to_string()))
        .collect()
}

fn state_of(run: &Run, agent: &str) -> String {
    states(run).into_iter().find(|(a, _)| a == agent).map(|(_, s)| s).expect("agent listed")
}

fn source() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/senpai")
}

fn install_links_the_skill_for_installed_agents_only() {
    let tmp = TempDir::new();
    let home = tmp.path();
    for root in [".claude", ".codex", ".gemini/config"] {
        std::fs::create_dir_all(home.join(root)).unwrap();
    }
    let installed = skill(home, "install");
    assert_eq!(installed.code, EXIT_OK, "{installed:?}");
    assert_eq!(state_of(&installed, "claude"), "linked");
    assert_eq!(state_of(&installed, "codex"), "linked");
    assert_eq!(state_of(&installed, "gemini"), "linked");
    assert_eq!(state_of(&installed, "pi"), "skipped");
    assert!(!home.join(".pi").exists(), "nothing is created for an agent that is not installed");
    assert_eq!(std::fs::read_link(home.join(".claude/skills/senpai")).unwrap(), source());
    let copied = home.join(".gemini/config/skills/senpai");
    assert!(!std::fs::symlink_metadata(&copied).unwrap().file_type().is_symlink(), "Gemini gets a copy");
    assert_eq!(read(&copied.join("SKILL.md")), read(&source().join("SKILL.md")));
    let again = skill(home, "install");
    assert_eq!((again.code, states(&again)), (EXIT_OK, states(&installed)), "installing twice changes nothing");
    assert_eq!(skill(home, "check").code, EXIT_OK);
}

fn foreign_files_are_reported_and_kept() {
    let tmp = TempDir::new();
    let home = tmp.path();
    write(&home.join(".agents/skills/senpai/SKILL.md"), "---\nname: someone-else\n---\n");
    std::fs::create_dir_all(home.join(".claude/skills")).unwrap();
    std::os::unix::fs::symlink("/nowhere", home.join(".claude/skills/senpai")).unwrap();
    let installed = skill(home, "install");
    assert_eq!(installed.code, EXIT_NOT_APPROVED, "{installed:?}");
    assert_eq!(state_of(&installed, "agents"), "foreign");
    assert_eq!(state_of(&installed, "claude"), "foreign");
    assert_eq!(read(&home.join(".agents/skills/senpai/SKILL.md")), "---\nname: someone-else\n---\n");
    assert_eq!(std::fs::read_link(home.join(".claude/skills/senpai")).unwrap(), Path::new("/nowhere"));
}

fn check_reports_stale_and_missing_places() {
    let tmp = TempDir::new();
    let home = tmp.path();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(home.join(".gemini/config")).unwrap();
    let missing = skill(home, "check");
    assert_eq!((missing.code, state_of(&missing, "claude")), (EXIT_NOT_APPROVED, "missing".to_string()));
    assert!(!home.join(".claude/skills").exists(), "check changes nothing");
    assert_eq!(skill(home, "install").code, EXIT_OK);
    // Another checkout's skill, and an edited copy, are senpai's own but stale.
    let other = home.join("old-checkout/skills/senpai");
    write(&other.join("SKILL.md"), "---\nname: senpai\n---\nold\n");
    std::fs::remove_file(home.join(".claude/skills/senpai")).unwrap();
    std::os::unix::fs::symlink(&other, home.join(".claude/skills/senpai")).unwrap();
    // An edit that drops the skill's name makes the copy the user's, and it is kept.
    write(&home.join(".gemini/config/skills/senpai/SKILL.md"), "edited\n");
    assert_eq!(state_of(&skill(home, "check"), "gemini"), "foreign");
    write(&home.join(".gemini/config/skills/senpai/SKILL.md"), "---\nname: senpai\n---\nedited\n");
    let stale = skill(home, "check");
    assert_eq!(stale.code, EXIT_NOT_APPROVED);
    assert_eq!(state_of(&stale, "claude"), "stale");
    assert_eq!(state_of(&stale, "gemini"), "stale");
    let fixed = skill(home, "install");
    assert_eq!(fixed.code, EXIT_OK, "{fixed:?}");
    assert_eq!(std::fs::read_link(home.join(".claude/skills/senpai")).unwrap(), source());
    assert_eq!(read(&home.join(".gemini/config/skills/senpai/SKILL.md")), read(&source().join("SKILL.md")));
}

fn remove_takes_away_only_senpais_own() {
    let tmp = TempDir::new();
    let home = tmp.path();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(home.join(".gemini/config")).unwrap();
    write(&home.join(".agents/skills/senpai/SKILL.md"), "mine\n");
    skill(home, "install");
    let removed = skill(home, "remove");
    assert_eq!(removed.code, EXIT_OK, "{removed:?}");
    assert_eq!(state_of(&removed, "claude"), "removed");
    assert_eq!(state_of(&removed, "gemini"), "removed");
    assert_eq!(state_of(&removed, "agents"), "foreign");
    assert!(std::fs::symlink_metadata(home.join(".claude/skills/senpai")).is_err());
    assert!(!home.join(".gemini/config/skills/senpai").exists());
    assert_eq!(read(&home.join(".agents/skills/senpai/SKILL.md")), "mine\n");
}

fn usage_errors() {
    let tmp = TempDir::new();
    for args in [vec!["skill"], vec!["skill", "bogus"], vec!["skill", "install", "extra"]] {
        let mut command = senpai(&args);
        command.env("HOME", tmp.path());
        let result = run(&mut command);
        assert_eq!(result.code, EXIT_ERROR, "{args:?}");
        assert!(result.stderr.contains("usage: senpai skill"), "{args:?}: {}", result.stderr);
    }
    assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none(), "usage errors touch nothing");
}
