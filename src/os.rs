//! The `os.path`, time and randomness helpers the Python client relied on,
//! with the same results for the same inputs.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};


/// `posixpath.normpath`.
pub fn normpath(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return b".".to_vec();
    }
    let initial = if path.starts_with(b"/") {
        if path.starts_with(b"//") && !path.starts_with(b"///") {
            2
        } else {
            1
        }
    } else {
        0
    };
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in path.split(|&b| b == b'/') {
        if part.is_empty() || part == b"." {
            continue;
        }
        if part != b".." || (initial == 0 && parts.is_empty()) || parts.last() == Some(&&b".."[..]) {
            parts.push(part);
        } else if !parts.is_empty() {
            parts.pop();
        }
    }
    let mut out = vec![b'/'; initial];
    out.extend_from_slice(&parts.join(&b'/'));
    if out.is_empty() {
        out.push(b'.');
    }
    out
}

pub fn getcwd() -> Vec<u8> {
    std::env::current_dir()
        .map(|p| p.into_os_string().into_vec())
        .unwrap_or_else(|_| b"/".to_vec())
}

fn join(base: &[u8], rest: &[u8]) -> Vec<u8> {
    if rest.starts_with(b"/") {
        return rest.to_vec();
    }
    let mut out = base.to_vec();
    if !out.is_empty() && !out.ends_with(b"/") {
        out.push(b'/');
    }
    out.extend_from_slice(rest);
    out
}

/// `os.path.abspath`.
pub fn abspath(path: &[u8]) -> Vec<u8> {
    if path.starts_with(b"/") {
        normpath(path)
    } else {
        normpath(&join(&getcwd(), path))
    }
}

/// `os.path.isabs`.
pub fn isabs(path: &[u8]) -> bool {
    path.starts_with(b"/")
}

/// `os.path.realpath(path)` (non-strict), ported from CPython 3.13's
/// `posixpath.realpath`: symlinks are resolved for the parts that exist, the
/// rest is kept as written, and a symlink loop is left unresolved.
pub fn realpath(filename: &[u8]) -> Vec<u8> {
    enum Part {
        Name(Vec<u8>),
        Resolved(Vec<u8>),
    }
    let mut rest: Vec<Part> = filename.split(|&b| b == b'/').rev().map(|p| Part::Name(p.to_vec())).collect();
    let mut part_count = rest.len();
    let mut path = if filename.starts_with(b"/") { b"/".to_vec() } else { getcwd() };
    let mut seen: std::collections::HashMap<Vec<u8>, Option<Vec<u8>>> = std::collections::HashMap::new();
    while part_count > 0 {
        let name = match rest.pop() {
            Some(Part::Resolved(link)) => {
                seen.insert(link, Some(path.clone()));
                continue;
            }
            Some(Part::Name(name)) => name,
            None => break,
        };
        part_count -= 1;
        if name.is_empty() || name == b"." {
            continue;
        }
        if name == b".." {
            let cut = path.iter().rposition(|&b| b == b'/').unwrap_or(0);
            path.truncate(cut);
            if path.is_empty() {
                path = b"/".to_vec();
            }
            continue;
        }
        let newpath = if path == b"/" { [b"/".as_slice(), &name].concat() } else { [path.as_slice(), b"/", &name].concat() };
        let is_link = matches!(fs::symlink_metadata(os_path(&newpath)), Ok(m) if m.file_type().is_symlink());
        if !is_link {
            path = newpath;
            continue;
        }
        if let Some(cached) = seen.get(&newpath) {
            path = cached.clone().unwrap_or(newpath);
            continue;
        }
        let target = match fs::read_link(os_path(&newpath)) {
            Ok(t) => t.into_os_string().into_vec(),
            Err(_) => {
                path = newpath;
                continue;
            }
        };
        seen.insert(newpath.clone(), None);
        if target.starts_with(b"/") {
            path = b"/".to_vec();
        }
        rest.push(Part::Resolved(newpath));
        let parts: Vec<Part> = target.split(|&b| b == b'/').rev().map(|p| Part::Name(p.to_vec())).collect();
        part_count += parts.len();
        rest.extend(parts);
    }
    path
}

/// `os.path.expanduser`.
pub fn expanduser(path: &[u8]) -> Vec<u8> {
    if !path.starts_with(b"~") {
        return path.to_vec();
    }
    let end = path.iter().position(|&b| b == b'/').unwrap_or(path.len());
    let user = &path[1..end];
    let home: Option<Vec<u8>> = if user.is_empty() {
        match std::env::var_os("HOME") {
            Some(h) => Some(h.into_vec()),
            None => home_of(None),
        }
    } else {
        home_of(Some(user))
    };
    let Some(home) = home else {
        return path.to_vec();
    };
    let mut trimmed = home;
    while trimmed.len() > 1 && trimmed.ends_with(b"/") {
        trimmed.pop();
    }
    let mut out = if trimmed == b"/" && end < path.len() { Vec::new() } else { trimmed };
    out.extend_from_slice(&path[end..]);
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

fn home_of(user: Option<&[u8]>) -> Option<Vec<u8>> {
    // SAFETY: getpwnam/getpwuid return pointers into static storage that stays
    // valid until the next call; the directory is copied out immediately.
    unsafe {
        let entry = match user {
            Some(name) => {
                let name = std::ffi::CString::new(name).ok()?;
                libc::getpwnam(name.as_ptr())
            }
            None => libc::getpwuid(libc::getuid()),
        };
        if entry.is_null() || (*entry).pw_dir.is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr((*entry).pw_dir).to_bytes().to_vec())
    }
}

pub fn dirname(path: &[u8]) -> Vec<u8> {
    let cut = path.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
    let mut head = path[..cut].to_vec();
    if !head.is_empty() && head.iter().any(|&b| b != b'/') {
        while head.ends_with(b"/") {
            head.pop();
        }
    }
    head
}

pub fn basename(path: &[u8]) -> Vec<u8> {
    let cut = path.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
    path[cut..].to_vec()
}

pub fn os_path(bytes: &[u8]) -> &Path {
    Path::new(std::ffi::OsStr::from_bytes(bytes))
}

pub fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}

pub fn os_string(bytes: Vec<u8>) -> OsString {
    OsString::from_vec(bytes)
}


// ---------------------------------------------------------------------------
// time and randomness
// ---------------------------------------------------------------------------

/// `datetime.now(timezone.utc)` to whole seconds, ISO 8601 with a `Z`.
pub fn utc_timestamp() -> String {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn random_bytes(count: usize) -> Vec<u8> {
    let mut buffer = vec![0u8; count];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buffer))
        .expect("the system random source is readable");
    buffer
}

/// `uuid.uuid4().hex`.
pub fn uuid4_hex() -> String {
    let mut bytes = random_bytes(16);
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `secrets.token_urlsafe(n)`.
pub fn token_urlsafe(n: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = random_bytes(n);
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normpath_matches_python() {
        for (input, expected) in [
            ("", "."),
            ("/", "/"),
            ("//", "//"),
            ("///", "/"),
            ("a/./b/../c", "a/c"),
            ("../a", "../a"),
            ("/../a", "/a"),
            ("a/", "a"),
            ("//a//b", "//a/b"),
        ] {
            assert_eq!(normpath(input.as_bytes()), expected.as_bytes(), "{input}");
        }
    }

    #[test]
    fn realpath_resolves_links_and_keeps_missing_parts() {
        let dir = std::env::temp_dir().join(format!("senpai-realpath-{}", uuid4_hex()));
        fs::create_dir_all(dir.join("real")).unwrap();
        std::os::unix::fs::symlink("real", dir.join("link")).unwrap();
        let base = fs::canonicalize(&dir).unwrap();
        let input = path_bytes(&dir.join("link/missing/x"));
        assert_eq!(realpath(&input), path_bytes(&base.join("real/missing/x")));
        std::os::unix::fs::symlink("loop", dir.join("loop")).unwrap();
        let looped = path_bytes(&dir.join("loop/y"));
        assert_eq!(realpath(&looped), path_bytes(&base.join("loop/y")));
        assert_eq!(realpath(b"/a/../../b"), b"/b");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn timestamp_and_ids_have_python_shapes() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_413), (2025, 11, 21));
        let stamp = utc_timestamp();
        assert_eq!(stamp.len(), 20);
        assert!(stamp.ends_with('Z'));
        let id = uuid4_hex();
        assert_eq!(id.len(), 32);
        assert_eq!(&id[12..13], "4");
        assert_eq!(token_urlsafe(24).len(), 32);
    }
}
