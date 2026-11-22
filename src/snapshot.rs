//! The reviewed state: which files changed, and a fingerprint of exactly that.
//!
//! Every byte hashed here must match the Python client's, or approvals that it
//! recorded stop verifying. Paths are kept as Python strings so they sort and
//! hash exactly as they did there.

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;

use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::git::{self, Git};
use crate::os;
use crate::pystr::PyStr;
use crate::request::{encode_path, Repository};

pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Clone, Debug, PartialEq)]
pub enum EntryKind {
    Untracked,
    Tracked,
    Rename,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StatusEntry {
    pub kind: EntryKind,
    pub path: PyStr,
    pub original_path: Option<PyStr>,
    pub mode_head: PyStr,
    pub mode_index: PyStr,
    pub mode_worktree: PyStr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileSnapshot {
    pub path: PyStr,
    pub old_path: Option<PyStr>,
    pub mode: PyStr,
    pub old_mode: PyStr,
    pub content_hash: String,
    pub patch_hash: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RepositorySnapshot {
    pub root: PyStr,
    pub baseline: String,
    pub scope_paths: Option<Vec<PyStr>>,
    pub files: Vec<FileSnapshot>,
    pub fingerprint: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub repositories: Vec<RepositorySnapshot>,
    pub fingerprint: String,
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn split_fixed_fields(record: &PyStr, separators: usize) -> Option<Vec<PyStr>> {
    let bytes = record.as_wtf8();
    let mut fields = Vec::new();
    let mut start = 0;
    for _ in 0..separators {
        let index = bytes[start..].iter().position(|&b| b == b' ')? + start;
        fields.push(PyStr::from_wtf8_unchecked(bytes[start..index].to_vec()));
        start = index + 1;
    }
    fields.push(PyStr::from_wtf8_unchecked(bytes[start..].to_vec()));
    Some(fields)
}

/// Parses `git status --porcelain=v2 -z` records into ordered entries.
pub fn parse_porcelain_v2(data: &[u8]) -> Result<Vec<StatusEntry>> {
    let records: Vec<&[u8]> = data.split(|&b| b == 0).collect();
    let mut entries = Vec::new();
    let mut index = 0;
    let entry = |kind, path, original_path, fields: &[PyStr], head: usize, idx: usize, wt: usize| StatusEntry {
        kind,
        path,
        original_path,
        mode_head: fields[head].clone(),
        mode_index: fields[idx].clone(),
        mode_worktree: fields[wt].clone(),
    };
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.is_empty() {
            continue;
        }
        let text = PyStr::decode_surrogateescape(record);
        if text.starts_with("? ") {
            entries.push(StatusEntry {
                kind: EntryKind::Untracked,
                path: PyStr::from_wtf8_unchecked(text.as_wtf8()[2..].to_vec()),
                original_path: None,
                mode_head: PyStr::new(),
                mode_index: PyStr::new(),
                mode_worktree: PyStr::new(),
            });
        } else if text.starts_with("1 ") {
            let fields = split_fixed_fields(&text, 8)
                .filter(|f| f.len() == 9)
                .ok_or_else(|| Error::Git("invalid porcelain v2 ordinary record".into()))?;
            entries.push(entry(EntryKind::Tracked, fields[8].clone(), None, &fields, 3, 4, 5));
        } else if text.starts_with("2 ") {
            let fields = split_fixed_fields(&text, 9)
                .filter(|f| f.len() == 10 && index < records.len())
                .ok_or_else(|| Error::Git("invalid porcelain v2 rename record".into()))?;
            let original = PyStr::decode_surrogateescape(records[index]);
            index += 1;
            entries.push(entry(EntryKind::Rename, fields[9].clone(), Some(original), &fields, 3, 4, 5));
        } else if text.starts_with("u ") {
            let fields = split_fixed_fields(&text, 10)
                .filter(|f| f.len() == 11)
                .ok_or_else(|| Error::Git("invalid porcelain v2 unmerged record".into()))?;
            entries.push(entry(EntryKind::Tracked, fields[10].clone(), None, &fields, 3, 5, 6));
        }
    }
    Ok(entries)
}

pub fn matches_scope(path: &PyStr, scope: &Option<Vec<PyStr>>) -> bool {
    match scope {
        None => true,
        Some(paths) if paths.is_empty() => true,
        Some(paths) => paths.iter().any(|s| {
            let p = path.as_wtf8();
            let s = s.as_wtf8();
            p == s || (p.len() > s.len() && p.starts_with(s) && p[s.len()] == b'/')
        }),
    }
}

pub fn mode_for_path(path: &[u8]) -> &'static str {
    let Ok(info) = fs::symlink_metadata(os::os_path(path)) else {
        return "";
    };
    let kind = info.file_type();
    if kind.is_symlink() {
        "120000"
    } else if kind.is_dir() {
        "040000"
    } else if info.permissions().mode() & 0o111 != 0 {
        "100755"
    } else {
        "100644"
    }
}

/// Hashes a working-tree path, tagging the kind so kinds cannot collide.
pub fn hash_working_path(path: &[u8]) -> Result<String> {
    let Ok(info) = fs::symlink_metadata(os::os_path(path)) else {
        return Ok(String::new());
    };
    let kind = info.file_type();
    let mut digest = Sha256::new();
    if kind.is_symlink() {
        digest.update(b"symlink\0");
        let target = fs::read_link(os::os_path(path)).map_err(|e| Error::Other(crate::error::os_error(&e, Some(path))))?;
        digest.update(os::path_bytes(&target));
    } else if kind.is_file() {
        digest.update(b"file\0");
        let Ok(mut file) = fs::File::open(os::os_path(path)) else {
            return Ok(String::new());
        };
        let mut buffer = vec![0u8; 1 << 20];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => digest.update(&buffer[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Ok(String::new()),
            }
        }
    } else if kind.is_dir() {
        digest.update(b"directory\0");
        digest.update(mode_for_path(path).as_bytes());
    } else {
        digest.update(b"other\0");
        digest.update(mode_for_path(path).as_bytes());
    }
    Ok(hex(&digest.finalize()))
}

fn non_zero(mode: &PyStr) -> bool {
    !mode.is_empty() && mode != "000000"
}

pub fn entry_modes(entry: &StatusEntry, absolute: &[u8]) -> (PyStr, PyStr) {
    if entry.kind == EntryKind::Untracked {
        return (PyStr::from(mode_for_path(absolute)), PyStr::new());
    }
    let mode = if non_zero(&entry.mode_worktree) {
        entry.mode_worktree.clone()
    } else if non_zero(&entry.mode_index) {
        entry.mode_index.clone()
    } else {
        entry.mode_head.clone()
    };
    let old_mode = if non_zero(&entry.mode_head) { entry.mode_head.clone() } else { PyStr::new() };
    (mode, old_mode)
}

pub fn git_blob_hash(data: &[u8]) -> String {
    let mut digest = Sha1::new();
    digest.update(format!("blob {}\0", data.len()).as_bytes());
    digest.update(data);
    hex(&digest.finalize())
}

pub fn decode_text(data: &[u8]) -> Option<&str> {
    if data.contains(&0) {
        return None;
    }
    std::str::from_utf8(data).ok()
}

/// Splits patch or file text on newlines only, as git does.
pub fn split_patch_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// The all-added patch an untracked file would produce. The index line carries
/// the blob hash, so binary content still changes the patch when it changes.
pub fn synthesize_added_patch(path: &PyStr, mode: &str, data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut line = |parts: &[&[u8]]| {
        for part in parts {
            out.extend_from_slice(part);
        }
        out.push(b'\n');
    };
    // Python formats the path into a str and encodes it strictly as UTF-8, so a
    // path that is not valid UTF-8 cannot produce a patch there either.
    let path_bytes = path
        .encode_strict()
        .ok_or_else(|| Error::Other(format!("'utf-8' codec can't encode the path {}", path.display())))?;
    line(&[b"diff --git a/", path_bytes, b" b/", path_bytes]);
    line(&[b"new file mode ", if mode.is_empty() { b"100644" } else { mode.as_bytes() }]);
    line(&[b"index ", "0".repeat(40).as_bytes(), b"..", git_blob_hash(data).as_bytes()]);
    line(&[b"--- /dev/null"]);
    line(&[b"+++ b/", path_bytes]);
    match decode_text(data) {
        None => line(&[b"Binary files /dev/null and b/", path_bytes, b" differ"]),
        Some(text) => {
            let body = split_patch_lines(text);
            line(&[format!("@@ -0,0 +1,{} @@", body.len()).as_bytes()]);
            for item in body {
                line(&[b"+", item.as_bytes()]);
            }
        }
    }
    Ok(out)
}

pub fn absolute_path(root: &[u8], path: &PyStr) -> Result<Vec<u8>> {
    let relative = encode_path(path)?;
    let mut out = root.to_vec();
    for part in relative.split(|&b| b == b'/') {
        if part.starts_with(b"/") || out.is_empty() {
            out = part.to_vec();
        } else {
            if !out.ends_with(b"/") {
                out.push(b'/');
            }
            out.extend_from_slice(part);
        }
    }
    Ok(out)
}

pub fn entry_patch(git: &dyn Git, root: &[u8], baseline: &str, entry: &StatusEntry) -> Result<Vec<u8>> {
    if entry.kind == EntryKind::Untracked {
        let absolute = absolute_path(root, &entry.path)?;
        let data = fs::read(os::os_path(&absolute)).unwrap_or_default();
        return synthesize_added_patch(&entry.path, mode_for_path(&absolute), &data);
    }
    let mut args = git::args(&["diff", "--no-ext-diff", "--no-textconv", "--find-renames", baseline, "--"]);
    args.push(os::os_string(encode_path(&entry.path)?));
    git.run(root, &args, &[0])
}

pub fn resolve_baseline(git: &dyn Git, root: &[u8]) -> Result<String> {
    let output = git.run(root, &git::args(&["rev-parse", "--verify", "HEAD"]), &[0, 1, 128])?;
    let text = String::from_utf8_lossy(&output);
    let candidate = text.trim_matches(crate::pystr::py_isspace_char);
    let valid = (40..=64).contains(&candidate.len()) && candidate.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    Ok(if valid { candidate.to_string() } else { EMPTY_TREE.to_string() })
}

pub fn fingerprint_repository(root: &PyStr, baseline: &str, scope: &Option<Vec<PyStr>>, files: &[FileSnapshot]) -> Result<String> {
    let strict = |value: &PyStr| -> Result<Vec<u8>> {
        value
            .encode_strict()
            .map(<[u8]>::to_vec)
            .ok_or_else(|| Error::Other(format!("'utf-8' codec can't encode {}", value.display())))
    };
    let escaped = |value: &PyStr| -> Result<Vec<u8>> {
        value
            .encode_surrogateescape()
            .ok_or_else(|| Error::Other(format!("'utf-8' codec can't encode {}", value.display())))
    };
    let mut digest = Sha256::new();
    digest.update(strict(root)?);
    digest.update(b"\0");
    digest.update(baseline.as_bytes());
    digest.update(b"\0");
    match scope {
        Some(paths) if !paths.is_empty() => {
            let mut joined = PyStr::new();
            for (index, path) in paths.iter().enumerate() {
                if index > 0 {
                    joined.push_str("\0");
                }
                joined.push_pystr(path);
            }
            digest.update(strict(&joined)?);
        }
        _ => digest.update(b"*"),
    }
    for file in files {
        let empty = PyStr::new();
        let content = PyStr::from(file.content_hash.as_str());
        let patch = PyStr::from(file.patch_hash.as_str());
        for value in [
            &file.path,
            file.old_path.as_ref().unwrap_or(&empty),
            &file.mode,
            &file.old_mode,
            &content,
            &patch,
        ] {
            digest.update(b"\0");
            digest.update(escaped(value)?);
        }
    }
    Ok(hex(&digest.finalize()))
}

/// Status entries in scope, sorted the way Python sorts `str` paths.
pub fn selected_entries(git: &dyn Git, repository: &Repository) -> Result<Vec<StatusEntry>> {
    let root = repository.root_bytes()?;
    let status = git::run(git, &root, &["status", "--porcelain=v2", "-z", "--untracked-files=all"])?;
    let mut selected: Vec<StatusEntry> = parse_porcelain_v2(&status)?
        .into_iter()
        .filter(|e| matches_scope(&e.path, &repository.paths))
        .collect();
    selected.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(selected)
}

pub fn collect_repository(git: &dyn Git, repository: &Repository) -> Result<RepositorySnapshot> {
    let root = repository.root_bytes()?;
    let baseline = resolve_baseline(git, &root)?;
    let mut files = Vec::new();
    for entry in selected_entries(git, repository)? {
        let absolute = absolute_path(&root, &entry.path)?;
        let patch = entry_patch(git, &root, &baseline, &entry)?;
        let (mode, old_mode) = entry_modes(&entry, &absolute);
        files.push(FileSnapshot {
            path: entry.path.clone(),
            old_path: entry.original_path.clone(),
            mode,
            old_mode,
            content_hash: hash_working_path(&absolute)?,
            patch_hash: sha256_hex(&patch),
        });
    }
    let fingerprint = fingerprint_repository(&repository.root, &baseline, &repository.paths, &files)?;
    Ok(RepositorySnapshot {
        root: repository.root.clone(),
        baseline,
        scope_paths: repository.paths.clone().filter(|p| !p.is_empty()),
        files,
        fingerprint,
    })
}

pub fn collect_snapshot(git: &dyn Git, repositories: &[Repository]) -> Result<Snapshot> {
    let snapshots: Vec<RepositorySnapshot> = repositories.iter().map(|r| collect_repository(git, r)).collect::<Result<_>>()?;
    let mut joined = PyStr::new();
    for (index, snapshot) in snapshots.iter().enumerate() {
        if index > 0 {
            joined.push_str("\0");
        }
        joined.push_pystr(&snapshot.root);
        joined.push_str("\0");
        joined.push_str(&snapshot.fingerprint);
    }
    let bytes = joined
        .encode_strict()
        .ok_or_else(|| Error::Other("'utf-8' codec can't encode a repository root".into()))?;
    Ok(Snapshot { repositories: snapshots, fingerprint: sha256_hex(bytes) })
}

#[cfg(test)]
mod fingerprint_tests {
    use super::*;

    fn file(path: PyStr, old: Option<&str>, mode: &str, old_mode: &str, content: &str, patch: &str) -> FileSnapshot {
        FileSnapshot {
            path,
            old_path: old.map(PyStr::from),
            mode: mode.into(),
            old_mode: old_mode.into(),
            content_hash: content.into(),
            patch_hash: patch.into(),
        }
    }

    /// Values computed by the last Python client (commit 7659d0c) for the
    /// same inputs, so existing approvals keep matching this formula.
    #[test]
    fn fingerprints_match_the_recorded_python_client() {
        let h = |c: char| c.to_string().repeat(64);
        let cases = [
            (
                fingerprint_repository(
                    &"/repo".into(),
                    &"a".repeat(40),
                    &None,
                    &[file("a.py".into(), None, "100644", "100644", &h('c'), &h('p'))],
                ),
                "e68059bbb03f70bb960e9937ef0d90f6669f7c10e797c5949bc1c535d5d6fadc",
            ),
            (
                fingerprint_repository(
                    &"/srv/repo two".into(),
                    &"b".repeat(40),
                    &Some(vec!["src".into(), "docs/x.md".into()]),
                    &[
                        file("src/new.rs".into(), Some("src/old.rs"), "100755", "100644", &h('d'), &h('e')),
                        file("docs/x.md".into(), None, "000000", "100644", "", &h('f')),
                    ],
                ),
                "3649a836190d6d401db4d4d6cf3ad3e6143304777478cc0d62391947c3abe77c",
            ),
            (
                fingerprint_repository(
                    &"/repo".into(),
                    "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
                    &None,
                    &[file(PyStr::decode_surrogateescape(b"caf\xe9.txt"), None, "120000", "", &h('1'), &h('2'))],
                ),
                "f09564491be5f07531240e231f0fe89ccb40f2ab18436fbf5728164c2d15e14a",
            ),
            (
                fingerprint_repository(&"/repo".into(), &"a".repeat(40), &Some(Vec::new()), &[]),
                "e0431bd7e2bb4f019911ec83d0167cbf19fa055b2dc960a1cf1afea707ac0010",
            ),
        ];
        for (index, (actual, expected)) in cases.into_iter().enumerate() {
            assert_eq!(actual.expect("fingerprint"), expected, "case {index}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_records_parse_like_python() {
        let data = b"# branch.oid abc\0\
1 .M N... 100644 100644 100644 aaaa bbbb a.py\0\
2 R. N... 100644 100644 100644 aaaa bbbb R100 new name.txt\0old name.txt\0\
u UU N... 100644 100644 100644 100644 aaaa bbbb cccc conflict.txt\0\
? untracked file\0";
        let entries = parse_porcelain_v2(data).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].path, PyStr::from("a.py"));
        assert_eq!(entries[1].kind, EntryKind::Rename);
        assert_eq!(entries[1].path, PyStr::from("new name.txt"));
        assert_eq!(entries[1].original_path, Some(PyStr::from("old name.txt")));
        assert_eq!(entries[2].mode_index, PyStr::from("100644"));
        assert_eq!(entries[3].path, PyStr::from("untracked file"));
        assert!(parse_porcelain_v2(b"1 too few\0").is_err());
    }

    #[test]
    fn synthesized_patch_matches_python_shape() {
        let patch = synthesize_added_patch(&PyStr::from("n.txt"), "100644", b"a\nb\n").unwrap();
        let text = String::from_utf8(patch).unwrap();
        assert!(text.starts_with("diff --git a/n.txt b/n.txt\nnew file mode 100644\nindex 0000000000000000000000000000000000000000.."));
        assert!(text.ends_with("+++ b/n.txt\n@@ -0,0 +1,2 @@\n+a\n+b\n"));
        assert_eq!(git_blob_hash(b""), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    }

    #[test]
    fn scope_matches_directories_and_exact_paths() {
        let scope = Some(vec![PyStr::from("src")]);
        assert!(matches_scope(&PyStr::from("src/a.rs"), &scope));
        assert!(matches_scope(&PyStr::from("src"), &scope));
        assert!(!matches_scope(&PyStr::from("srcx/a.rs"), &scope));
    }
}
