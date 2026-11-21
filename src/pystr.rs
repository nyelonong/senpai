//! Text with Python `str` semantics.
//!
//! The Python client decodes git output and argv with `surrogateescape`, so a
//! file name that is not valid UTF-8 survives as lone surrogate code points and
//! is hashed back to its original bytes. `PyStr` stores those code points as
//! generalized UTF-8 (WTF-8): byte order equals code point order, which is the
//! order Python sorts `str` values in, and every Python string round-trips.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PyStr(Vec<u8>);

impl PyStr {
    pub fn new() -> Self {
        PyStr(Vec::new())
    }

    pub fn as_wtf8(&self) -> &[u8] {
        &self.0
    }

    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn push_code_point(&mut self, cp: u32) {
        encode_code_point(cp, &mut self.0);
    }

    pub fn push_str(&mut self, text: &str) {
        self.0.extend_from_slice(text.as_bytes());
    }

    pub fn push_pystr(&mut self, other: &PyStr) {
        self.0.extend_from_slice(&other.0);
    }

    pub fn from_code_points<I: IntoIterator<Item = u32>>(points: I) -> Self {
        let mut out = PyStr::new();
        for cp in points {
            out.push_code_point(cp);
        }
        out
    }

    /// Trusted WTF-8 bytes produced by this module (for example a slice taken
    /// at code point boundaries of another `PyStr`).
    pub fn from_wtf8_unchecked(bytes: Vec<u8>) -> Self {
        PyStr(bytes)
    }

    /// `bytes.decode("utf-8", "surrogateescape")`.
    pub fn decode_surrogateescape(bytes: &[u8]) -> Self {
        let mut out = Vec::with_capacity(bytes.len());
        for chunk in bytes.utf8_chunks() {
            out.extend_from_slice(chunk.valid().as_bytes());
            for &byte in chunk.invalid() {
                encode_code_point(0xDC00 + byte as u32, &mut out);
            }
        }
        PyStr(out)
    }

    /// `bytes.decode("utf-8", "replace")`.
    pub fn decode_lossy(bytes: &[u8]) -> Self {
        PyStr(String::from_utf8_lossy(bytes).into_owned().into_bytes())
    }

    /// `str.encode("utf-8", "surrogateescape")`; `None` where Python raises.
    pub fn encode_surrogateescape(&self) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(self.0.len());
        for cp in self.code_points() {
            if (0xDC80..=0xDCFF).contains(&cp) {
                out.push((cp - 0xDC00) as u8);
            } else if (0xD800..=0xDFFF).contains(&cp) {
                return None;
            } else {
                encode_code_point(cp, &mut out);
            }
        }
        Some(out)
    }

    /// `str.encode("utf-8")`; `None` where Python raises (any surrogate).
    pub fn encode_strict(&self) -> Option<&[u8]> {
        self.as_str().map(str::as_bytes)
    }

    pub fn to_path(&self) -> Option<PathBuf> {
        self.encode_surrogateescape()
            .map(|bytes| PathBuf::from(OsStr::from_bytes(&bytes)))
    }

    pub fn from_os(value: &OsStr) -> Self {
        PyStr::decode_surrogateescape(value.as_bytes())
    }

    pub fn code_points(&self) -> CodePoints<'_> {
        CodePoints { bytes: &self.0, index: 0 }
    }

    pub fn char_len(&self) -> usize {
        self.code_points().count()
    }

    pub fn starts_with(&self, prefix: &str) -> bool {
        self.0.starts_with(prefix.as_bytes())
    }

    pub fn ends_with(&self, suffix: &str) -> bool {
        self.0.ends_with(suffix.as_bytes())
    }

    pub fn contains_byte(&self, byte: u8) -> bool {
        self.0.contains(&byte)
    }

    /// `str.strip()` with Python's whitespace definition.
    pub fn strip(&self) -> PyStr {
        let points: Vec<u32> = self.code_points().collect();
        let start = points.iter().position(|&cp| !py_isspace(cp)).unwrap_or(points.len());
        let end = points.iter().rposition(|&cp| !py_isspace(cp)).map_or(start, |i| i + 1);
        PyStr::from_code_points(points[start..end.max(start)].iter().copied())
    }

    pub fn is_blank(&self) -> bool {
        self.code_points().all(py_isspace)
    }

    /// Splits on an ASCII byte, as Python's `str.split(sep)` does.
    pub fn split_byte(&self, separator: u8) -> Vec<PyStr> {
        self.0.split(|&b| b == separator).map(|part| PyStr(part.to_vec())).collect()
    }

    /// Printable form for diagnostics, matching Python's `backslashreplace`
    /// handling of lone surrogates on stderr.
    pub fn display(&self) -> String {
        let mut out = String::with_capacity(self.0.len());
        for cp in self.code_points() {
            match char::from_u32(cp) {
                Some(c) => out.push(c),
                None => out.push_str(&format!("\\u{cp:04x}")),
            }
        }
        out
    }
}

impl From<&str> for PyStr {
    fn from(value: &str) -> Self {
        PyStr(value.as_bytes().to_vec())
    }
}

impl From<String> for PyStr {
    fn from(value: String) -> Self {
        PyStr(value.into_bytes())
    }
}

impl From<&String> for PyStr {
    fn from(value: &String) -> Self {
        PyStr(value.as_bytes().to_vec())
    }
}

impl PartialEq<str> for PyStr {
    fn eq(&self, other: &str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl PartialEq<&str> for PyStr {
    fn eq(&self, other: &&str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl fmt::Display for PyStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

impl fmt::Debug for PyStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&py_repr(self))
    }
}

pub struct CodePoints<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl Iterator for CodePoints<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let bytes = self.bytes;
        let i = self.index;
        if i >= bytes.len() {
            return None;
        }
        let lead = bytes[i];
        let (cp, width) = if lead < 0x80 {
            (lead as u32, 1)
        } else if lead < 0xE0 {
            (((lead as u32 & 0x1F) << 6) | (bytes[i + 1] as u32 & 0x3F), 2)
        } else if lead < 0xF0 {
            (
                ((lead as u32 & 0x0F) << 12)
                    | ((bytes[i + 1] as u32 & 0x3F) << 6)
                    | (bytes[i + 2] as u32 & 0x3F),
                3,
            )
        } else {
            (
                ((lead as u32 & 0x07) << 18)
                    | ((bytes[i + 1] as u32 & 0x3F) << 12)
                    | ((bytes[i + 2] as u32 & 0x3F) << 6)
                    | (bytes[i + 3] as u32 & 0x3F),
                4,
            )
        };
        self.index += width;
        Some(cp)
    }
}

pub fn encode_code_point(cp: u32, out: &mut Vec<u8>) {
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.push(0xC0 | (cp >> 6) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else if cp < 0x10000 {
        out.push(0xE0 | (cp >> 12) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else {
        out.push(0xF0 | (cp >> 18) as u8);
        out.push(0x80 | ((cp >> 12) & 0x3F) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    }
}

/// `str.isspace()` for one code point.
pub fn py_isspace(cp: u32) -> bool {
    matches!(
        cp,
        0x09..=0x0D
            | 0x1C..=0x20
            | 0x85
            | 0xA0
            | 0x1680
            | 0x2000..=0x200A
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
    )
}

pub fn py_isspace_char(c: char) -> bool {
    py_isspace(c as u32)
}

/// `str.strip()` for Rust strings.
pub fn py_strip(text: &str) -> &str {
    text.trim_matches(py_isspace_char)
}

/// Python's `str.splitlines()` boundaries.
pub fn py_splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        let boundary = matches!(
            c,
            '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{1C}' | '\u{1D}' | '\u{1E}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        );
        if !boundary {
            continue;
        }
        lines.push(&text[start..index]);
        let mut next = index + c.len_utf8();
        if c == '\r' {
            if let Some(&(_, '\n')) = chars.peek() {
                chars.next();
                next += 1;
            }
        }
        start = next;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Python's `repr()` of a `str`, used where the client formats `{value!r}`.
pub fn py_repr(value: &PyStr) -> String {
    let points: Vec<u32> = value.code_points().collect();
    let has_single = points.contains(&('\'' as u32));
    let has_double = points.contains(&('"' as u32));
    let quote = if has_single && !has_double { '"' } else { '\'' };
    let mut out = String::new();
    out.push(quote);
    for cp in points {
        match cp {
            0x5C => out.push_str("\\\\"),
            0x09 => out.push_str("\\t"),
            0x0A => out.push_str("\\n"),
            0x0D => out.push_str("\\r"),
            _ if cp == quote as u32 => {
                out.push('\\');
                out.push(quote);
            }
            _ if cp < 0x20 || cp == 0x7F => out.push_str(&format!("\\x{cp:02x}")),
            _ => match char::from_u32(cp) {
                Some(c) if is_printable(c) => out.push(c),
                _ if cp <= 0xFF => out.push_str(&format!("\\x{cp:02x}")),
                _ if cp <= 0xFFFF => out.push_str(&format!("\\u{cp:04x}")),
                _ => out.push_str(&format!("\\U{cp:08x}")),
            },
        }
    }
    out.push(quote);
    out
}

/// An approximation of Python's `str.isprintable()` for `repr`: control,
/// format, surrogate, private-use, unassigned and non-space separators are
/// escaped.
fn is_printable(c: char) -> bool {
    let cp = c as u32;
    if cp == 0x20 {
        return true;
    }
    if (0x80..0xA0).contains(&cp) || cp == 0xAD {
        return false;
    }
    if py_isspace(cp) {
        return false;
    }
    if (0xE000..=0xF8FF).contains(&cp) || cp >= 0xF0000 {
        return false;
    }
    if matches!(cp, 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0xFEFF | 0xFFF9..=0xFFFB) {
        return false;
    }
    true
}

impl PartialOrd<str> for PyStr {
    fn partial_cmp(&self, other: &str) -> Option<Ordering> {
        Some(self.0.as_slice().cmp(other.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surrogateescape_round_trips_invalid_bytes() {
        let raw = b"caf\xe9/\xed\xa0\x80ok";
        let decoded = PyStr::decode_surrogateescape(raw);
        assert_eq!(decoded.encode_surrogateescape().unwrap(), raw);
        assert!(decoded.as_str().is_none());
        let points: Vec<u32> = decoded.code_points().collect();
        assert_eq!(points[3], 0xDCE9);
    }

    #[test]
    fn ordering_matches_code_points() {
        let escaped = PyStr::decode_surrogateescape(b"\xff");
        let below = PyStr::from("\u{D7FF}");
        let above = PyStr::from("\u{E000}");
        assert!(below < escaped && escaped < above);
    }

    #[test]
    fn strip_uses_python_whitespace() {
        assert_eq!(PyStr::from("\u{1c} a \u{3000}").strip(), PyStr::from("a"));
        assert_eq!(py_strip("\u{1f}x\t"), "x");
    }

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(py_splitlines("a\r\nb\rc\u{2028}d\n"), vec!["a", "b", "c", "d"]);
        assert_eq!(py_splitlines("a\n\nb"), vec!["a", "", "b"]);
        assert!(py_splitlines("").is_empty());
    }

    #[test]
    fn repr_matches_python() {
        assert_eq!(py_repr(&PyStr::from("it's")), "\"it's\"");
        assert_eq!(py_repr(&PyStr::from("a\tb")), "'a\\tb'");
        assert_eq!(py_repr(&PyStr::from("x\u{7f}")), "'x\\x7f'");
    }
}
