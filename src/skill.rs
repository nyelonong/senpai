//! `senpai skill install|check|remove`: puts the `senpai` agent skill where
//! each coding agent looks for skills. Agents that follow links get a link to
//! the skill in this checkout; Gemini, which does not, gets a marked copy.
//! Nothing is created for an agent that is not installed, and nothing senpai
//! cannot prove it put there is ever replaced or removed.

use std::path::{Path, PathBuf};

use crate::json::{Object, Value};
use crate::pystr::PyStr;

const SKILL_DIR_ENV: &str = "SENPAI_SKILL_DIR";
const BUILT_SKILL_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/skills/senpai");
/// Marks a copied skill as senpai's own. Its first line is the absolute source
/// directory; every further line is one file senpai copied, relative to the
/// copy. A copy holding anything else is not senpai's to delete.
const COPY_MARKER: &str = ".senpai-skill";
const USAGE: &str = "usage: senpai skill {install,check,remove}\n";

#[derive(Clone, Copy, PartialEq)]
enum Way {
    Link,
    Copy,
}

struct Place {
    agent: &'static str,
    /// The agent's own folder; without it the agent is taken as not installed.
    root: PathBuf,
    target: PathBuf,
    way: Way,
}

fn places(home: &Path) -> Vec<Place> {
    let place = |agent, root: &str, way| {
        let root = home.join(root);
        Place { agent, target: root.join("skills/senpai"), root, way }
    };
    vec![
        place("claude", ".claude", Way::Link),
        place("agents", ".agents", Way::Link),
        place("pi", ".pi/agent", Way::Link),
        place("codex", ".codex", Way::Link),
        place("gemini", ".gemini/config", Way::Copy),
    ]
}

fn source() -> Result<PathBuf, String> {
    let dir = std::env::var_os(SKILL_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(BUILT_SKILL_DIR));
    let dir = std::fs::canonicalize(&dir).map_err(|_| format!("no senpai skill at {}; set {SKILL_DIR_ENV}", dir.display()))?;
    if is_senpai_skill(&dir) {
        Ok(dir)
    } else {
        Err(format!("{} holds no SKILL.md named senpai", dir.display()))
    }
}

fn is_senpai_skill(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("SKILL.md"))
        .map(|text| text.lines().take(5).any(|line| line.trim() == "name: senpai"))
        .unwrap_or(false)
}

enum Found {
    Missing,
    OurLink(PathBuf),
    OurCopy { from: PathBuf, files: Vec<PathBuf> },
    Foreign,
}

/// Every regular file under `dir`, relative to it, without following links.
fn files_under(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        for entry in std::fs::read_dir(dir.join(&relative))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = relative.join(entry.file_name());
            if kind.is_dir() {
                pending.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// A copy is senpai's only when its marker names an absolute source, it holds a
/// skill named senpai, and it holds exactly the files the marker lists.
fn our_copy(target: &Path) -> Option<(PathBuf, Vec<PathBuf>)> {
    let marker = std::fs::read_to_string(target.join(COPY_MARKER)).ok()?;
    let mut lines = marker.lines();
    let from = PathBuf::from(lines.next()?.trim());
    let mut listed: Vec<PathBuf> = lines.filter(|l| !l.is_empty()).map(PathBuf::from).collect();
    if !from.is_absolute() || !is_senpai_skill(target) || listed.iter().any(|p| p.is_absolute()) {
        return None;
    }
    listed.push(PathBuf::from(COPY_MARKER));
    listed.sort();
    (files_under(target).ok()? == listed).then_some((from, listed))
}

fn inspect(place: &Place) -> Found {
    let target = &place.target;
    match std::fs::symlink_metadata(target) {
        Err(_) => Found::Missing,
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::read_link(target) {
            // A link to a senpai skill is ours, and so is a dangling link to a
            // moved checkout's skills/senpai; replacing a link never touches
            // what it points at.
            Ok(points_at) if is_senpai_skill(target) || (!target.exists() && points_at.ends_with("skills/senpai")) => {
                Found::OurLink(points_at)
            }
            _ => Found::Foreign,
        },
        Ok(meta) if meta.is_dir() && place.way == Way::Copy => match our_copy(target) {
            Some((from, files)) => Found::OurCopy { from, files },
            None => Found::Foreign,
        },
        Ok(_) => Found::Foreign,
    }
}

fn same_contents(source: &Path, target: &Path) -> bool {
    files_under(source).is_ok_and(|files| {
        files.iter().all(|f| std::fs::read(source.join(f)).ok() == std::fs::read(target.join(f)).ok())
    })
}

/// The copy's recorded files are exactly the source's files today.
fn same_files(source: &Path, copied: &[PathBuf]) -> bool {
    let mut expected: Vec<PathBuf> = files_under(source).unwrap_or_default();
    expected.push(PathBuf::from(COPY_MARKER));
    expected.sort();
    expected == copied
}

/// `linked`, `stale`, `missing`, `foreign` or `skipped` (agent not installed).
fn state(place: &Place, source: &Path) -> &'static str {
    if !place.root.is_dir() {
        return "skipped";
    }
    match inspect(place) {
        Found::Missing => "missing",
        Found::Foreign => "foreign",
        Found::OurLink(points_at) if points_at == source => "linked",
        Found::OurCopy { from, files } if from == source && same_files(source, &files) && same_contents(source, &place.target) => {
            "linked"
        }
        _ => "stale",
    }
}

/// Removes senpai's own link, or the files of its own copy one by one, then
/// the folders they leave empty.
fn remove_ours(place: &Place) -> std::io::Result<()> {
    match inspect(place) {
        Found::OurLink(_) => std::fs::remove_file(&place.target),
        Found::OurCopy { files, .. } => {
            for file in &files {
                std::fs::remove_file(place.target.join(file))?;
            }
            let mut dirs: Vec<PathBuf> = files.iter().filter_map(|f| f.parent().map(Path::to_path_buf)).collect();
            dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
            for dir in dirs.into_iter().filter(|d| !d.as_os_str().is_empty()) {
                let _ = std::fs::remove_dir(place.target.join(dir));
            }
            std::fs::remove_dir(&place.target)
        }
        Found::Missing | Found::Foreign => Ok(()),
    }
}

/// Copies the skill's files (not links) into a temporary sibling, marks it,
/// and moves it into place, so a failed copy never leaves a half-made skill.
fn place_copy(source: &Path, target: &Path) -> std::io::Result<()> {
    let parent = target.parent().expect("a skills folder");
    let staging = parent.join(format!(".senpai-skill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let files = files_under(source)?;
    let result = (|| {
        for file in &files {
            let from = source.join(file);
            if !std::fs::symlink_metadata(&from)?.is_file() {
                continue;
            }
            let to = staging.join(file);
            std::fs::create_dir_all(to.parent().expect("a parent"))?;
            std::fs::copy(&from, &to)?;
        }
        let copied = files_under(&staging)?;
        let mut marker = format!("{}\n", source.display());
        for file in copied {
            marker.push_str(&format!("{}\n", file.display()));
        }
        std::fs::write(staging.join(COPY_MARKER), marker)?;
        std::fs::rename(&staging, target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

/// Removes staging folders an interrupted copy left beside the target, but
/// only ones that are provably senpai's own copies.
fn remove_leftover_staging(target: &Path) {
    let Some(parent) = target.parent() else { return };
    let Ok(entries) = std::fs::read_dir(parent) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if !name.starts_with(".senpai-skill-") || !is_dir {
            continue;
        }
        if let Some((_, files)) = our_copy(&entry.path()) {
            for file in &files {
                let _ = std::fs::remove_file(entry.path().join(file));
            }
            let mut dirs: Vec<PathBuf> = files.iter().filter_map(|f| f.parent().map(Path::to_path_buf)).collect();
            dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
            for dir in dirs.into_iter().filter(|d| !d.as_os_str().is_empty()) {
                let _ = std::fs::remove_dir(entry.path().join(dir));
            }
            let _ = std::fs::remove_dir(entry.path());
        }
    }
}

fn install(place: &Place, source: &Path) -> std::io::Result<()> {
    if place.way == Way::Copy {
        remove_leftover_staging(&place.target);
    }
    remove_ours(place)?;
    std::fs::create_dir_all(place.target.parent().expect("a skills folder"))?;
    match place.way {
        Way::Link => std::os::unix::fs::symlink(source, &place.target),
        Way::Copy => place_copy(source, &place.target),
    }
}

/// `senpai skill install|check|remove`. Exit 0 when every installed agent has
/// the current skill (or, for `remove`, when senpai's own copies are gone);
/// 1 when something is stale, missing or blocked by a foreign file; 2 on error.
pub fn main(argv: &[PyStr]) -> i32 {
    let action = match argv.iter().map(PyStr::display).collect::<Vec<_>>().as_slice() {
        [a] if a == "-h" || a == "--help" => {
            print!("{USAGE}\nInstall, check or remove the senpai agent skill for each coding agent.\n");
            return crate::EXIT_OK;
        }
        [a] if matches!(a.as_str(), "install" | "check" | "remove") => a.clone(),
        _ => {
            eprintln!("{USAGE}senpai skill: error: choose install, check or remove");
            return crate::EXIT_ERROR;
        }
    };
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from) else {
        eprintln!("senpai skill: HOME is not set");
        return crate::EXIT_ERROR;
    };
    let source = match source() {
        Ok(source) => source,
        Err(message) => {
            eprintln!("senpai skill: {message}");
            return crate::EXIT_ERROR;
        }
    };
    let places = places(&home);
    for place in &places {
        let target = std::fs::canonicalize(&place.target).unwrap_or_else(|_| place.target.clone());
        if source.starts_with(&target) && !std::fs::symlink_metadata(&place.target).is_ok_and(|m| m.file_type().is_symlink()) {
            eprintln!("senpai skill: the skill source {} is the {} copy itself", source.display(), place.agent);
            return crate::EXIT_ERROR;
        }
    }
    let mut rows = Vec::new();
    let (mut complete, mut failed) = (true, false);
    for place in &places {
        let before = state(place, &source);
        let outcome = match (action.as_str(), before) {
            ("install", "missing" | "stale") => install(place, &source).map(|()| state(place, &source)),
            ("remove", "linked" | "stale") => remove_ours(place).map(|()| "removed"),
            (_, state) => Ok(state),
        };
        let after = match outcome {
            Ok(after) => after,
            Err(error) => {
                eprintln!("senpai skill: {}: {error}", place.target.display());
                failed = true;
                "error"
            }
        };
        complete &= match action.as_str() {
            "remove" => !matches!(after, "linked" | "stale" | "error"),
            _ => matches!(after, "linked" | "skipped"),
        };
        rows.push(Value::Object(
            Object::new()
                .with("agent", place.agent)
                .with("path", PyStr::from(place.target.to_string_lossy().into_owned()))
                .with("state", after),
        ));
    }
    let out = Object::new()
        .with("skill", PyStr::from(source.to_string_lossy().into_owned()))
        .with("places", Value::Array(rows));
    println!("{}", crate::json::dumps(&Value::Object(out), crate::json::DEFAULT));
    if failed {
        crate::EXIT_ERROR
    } else if complete {
        crate::EXIT_OK
    } else {
        crate::EXIT_NOT_APPROVED
    }
}
