//! JSON with Python `json` module semantics.
//!
//! Output matches `json.dumps` byte for byte (`ensure_ascii`, optional
//! `sort_keys`, both separator styles, Python float repr), and parsing accepts
//! what `json.loads` accepts, including lone surrogate escapes. Objects keep
//! insertion order like a Python `dict`.

use crate::pystr::{encode_code_point, PyStr};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i128),
    BigInt(String),
    Float(f64),
    Str(PyStr),
    Array(Vec<Value>),
    Object(Object),
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Object(Vec<(PyStr, Value)>);

impl Object {
    pub fn new() -> Self {
        Object(Vec::new())
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn insert(&mut self, key: impl Into<PyStr>, value: Value) {
        let key = key.into();
        if let Some(slot) = self.0.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
        } else {
            self.0.push((key, value));
        }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.insert(key, value.into());
        self
    }

    pub fn keys(&self) -> impl Iterator<Item = &PyStr> {
        self.0.iter().map(|(k, _)| k)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&PyStr, &Value)> {
        self.0.iter().map(|(k, v)| (k, v))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// True when the key set is exactly `keys` (Python `set(d) == {...}`).
    pub fn has_exact_keys(&self, keys: &[&str]) -> bool {
        self.0.len() == keys.len() && keys.iter().all(|k| self.contains_key(k))
    }
}

impl Value {
    pub fn str(text: &str) -> Value {
        Value::Str(PyStr::from(text))
    }

    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_pystr(&self) -> Option<&PyStr> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_str(&self, expected: &str) -> bool {
        matches!(self, Value::Str(s) if s == expected)
    }

    /// `isinstance(v, int) and not isinstance(v, bool)`.
    pub fn as_int(&self) -> Option<i128> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// A positive integer, including integers too large for `i128`.
    pub fn is_positive_integer(&self) -> bool {
        match self {
            Value::Int(i) => *i >= 1,
            Value::BigInt(digits) => !digits.starts_with('-'),
            _ => false,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object().and_then(|o| o.get(key))
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Value::Null => "NoneType",
            Value::Bool(_) => "bool",
            Value::Int(_) | Value::BigInt(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "str",
            Value::Array(_) => "list",
            Value::Object(_) => "dict",
        }
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::str(v)
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Str(PyStr::from(v))
    }
}

impl From<PyStr> for Value {
    fn from(v: PyStr) -> Self {
        Value::Str(v)
    }
}

impl From<&PyStr> for Value {
    fn from(v: &PyStr) -> Self {
        Value::Str(v.clone())
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v as i128)
    }
}

impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Int(v as i128)
    }
}

impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Value::Int(v as i128)
    }
}

impl From<usize> for Value {
    fn from(v: usize) -> Self {
        Value::Int(v as i128)
    }
}

impl From<Object> for Value {
    fn from(v: Object) -> Self {
        Value::Object(v)
    }
}

impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Self {
        Value::Array(v)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

// ---------------------------------------------------------------------------
// output
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Style {
    pub sort_keys: bool,
    pub compact: bool,
}

/// `json.dumps(v)`.
pub const DEFAULT: Style = Style { sort_keys: false, compact: false };
/// `json.dumps(v, sort_keys=True)`.
pub const SORTED: Style = Style { sort_keys: true, compact: false };
/// `json.dumps(v, separators=(",", ":"))`.
pub const COMPACT: Style = Style { sort_keys: false, compact: true };

pub fn dumps(value: &Value, style: Style) -> String {
    let mut out = String::new();
    write_value(value, style, &mut out);
    out
}

fn write_value(value: &Value, style: Style, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::BigInt(digits) => out.push_str(digits),
        Value::Float(f) => out.push_str(&float_repr(*f, true)),
        Value::Str(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(if style.compact { "," } else { ", " });
                }
                write_value(item, style, out);
            }
            out.push(']');
        }
        Value::Object(object) => {
            out.push('{');
            let mut entries: Vec<&(PyStr, Value)> = object.0.iter().collect();
            if style.sort_keys {
                entries.sort_by(|a, b| a.0.cmp(&b.0));
            }
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push_str(if style.compact { "," } else { ", " });
                }
                write_string(key, out);
                out.push_str(if style.compact { ":" } else { ": " });
                write_value(item, style, out);
            }
            out.push('}');
        }
    }
}

fn write_string(value: &PyStr, out: &mut String) {
    out.push('"');
    for cp in value.code_points() {
        match cp {
            0x22 => out.push_str("\\\""),
            0x5C => out.push_str("\\\\"),
            0x0A => out.push_str("\\n"),
            0x0D => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            0x08 => out.push_str("\\b"),
            0x0C => out.push_str("\\f"),
            0x20..=0x7E => out.push(cp as u8 as char),
            0x10000.. => {
                let v = cp - 0x10000;
                let high = 0xD800 | ((v >> 10) & 0x3FF);
                let low = 0xDC00 | (v & 0x3FF);
                out.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
            }
            _ => out.push_str(&format!("\\u{cp:04x}")),
        }
    }
    out.push('"');
}

/// Python's `float.__repr__`; `json` spells the non-finite values its own way.
pub fn float_repr(value: f64, json: bool) -> String {
    if value.is_nan() {
        return if json { "NaN".into() } else { "nan".into() };
    }
    if value.is_infinite() {
        let text = if json { "Infinity" } else { "inf" };
        return if value < 0.0 { format!("-{text}") } else { text.into() };
    }
    // Rust's `{:e}` gives the shortest round-trip digits, as Python's repr does.
    let formatted = format!("{value:e}");
    let (mantissa, exponent) = formatted.split_once('e').expect("exponent");
    let exponent: i32 = exponent.parse().expect("exponent digits");
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let sign = if negative { "-" } else { "" };
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        let body = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point as usize >= digits.len() {
            format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
        } else {
            format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
        };
        format!("{sign}{body}")
    } else {
        let mantissa = if digits.len() > 1 {
            format!("{}.{}", &digits[..1], &digits[1..])
        } else {
            digits.clone()
        };
        let exp_sign = if exponent < 0 { '-' } else { '+' };
        format!("{sign}{mantissa}e{exp_sign}{:02}", exponent.abs())
    }
}

// ---------------------------------------------------------------------------
// input
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `json.loads(text)` for text that is already a Python `str`.
pub fn loads(text: &PyStr) -> Result<Value, ParseError> {
    let bytes = text.as_wtf8();
    if bytes.starts_with("\u{FEFF}".as_bytes()) {
        return Err(ParseError("Unexpected UTF-8 BOM (decode using utf-8-sig): line 1 column 1 (char 0)".into()));
    }
    let mut parser = Parser { bytes, pos: 0 };
    parser.skip_ws();
    let value = parser.value()?;
    parser.skip_ws();
    if parser.pos != bytes.len() {
        return Err(parser.error("Extra data"));
    }
    Ok(value)
}

pub fn loads_str(text: &str) -> Result<Value, ParseError> {
    loads(&PyStr::from(text))
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn error(&self, message: &str) -> ParseError {
        let before = &self.bytes[..self.pos.min(self.bytes.len())];
        let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
        let line_start = before.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let char_index = PyStr::from_wtf8_unchecked(before.to_vec()).char_len();
        let column = PyStr::from_wtf8_unchecked(before[line_start..].to_vec()).char_len() + 1;
        ParseError(format!("{message}: line {line} column {column} (char {char_index})"))
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && matches!(self.bytes[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn rest(&self) -> &[u8] {
        &self.bytes[self.pos..]
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        let Some(&byte) = self.bytes.get(self.pos) else {
            return Err(self.error("Expecting value"));
        };
        match byte {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => {
                self.pos += 1;
                Ok(Value::Str(self.string()?))
            }
            b'n' if self.rest().starts_with(b"null") => {
                self.pos += 4;
                Ok(Value::Null)
            }
            b't' if self.rest().starts_with(b"true") => {
                self.pos += 4;
                Ok(Value::Bool(true))
            }
            b'f' if self.rest().starts_with(b"false") => {
                self.pos += 5;
                Ok(Value::Bool(false))
            }
            b'N' if self.rest().starts_with(b"NaN") => {
                self.pos += 3;
                Ok(Value::Float(f64::NAN))
            }
            b'I' if self.rest().starts_with(b"Infinity") => {
                self.pos += 8;
                Ok(Value::Float(f64::INFINITY))
            }
            b'-' if self.rest().starts_with(b"-Infinity") => {
                self.pos += 9;
                Ok(Value::Float(f64::NEG_INFINITY))
            }
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(self.error("Expecting value")),
        }
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        let b = self.bytes;
        let mut i = self.pos;
        if b.get(i) == Some(&b'-') {
            i += 1;
        }
        match b.get(i) {
            Some(b'0') => i += 1,
            Some(b'1'..=b'9') => {
                while matches!(b.get(i), Some(b'0'..=b'9')) {
                    i += 1;
                }
            }
            _ => return Err(self.error("Expecting value")),
        }
        let mut is_float = false;
        if b.get(i) == Some(&b'.') && matches!(b.get(i + 1), Some(b'0'..=b'9')) {
            is_float = true;
            i += 1;
            while matches!(b.get(i), Some(b'0'..=b'9')) {
                i += 1;
            }
        }
        if matches!(b.get(i), Some(b'e' | b'E')) {
            let mut j = i + 1;
            if matches!(b.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            if matches!(b.get(j), Some(b'0'..=b'9')) {
                while matches!(b.get(j), Some(b'0'..=b'9')) {
                    j += 1;
                }
                is_float = true;
                i = j;
            }
        }
        let text = std::str::from_utf8(&b[start..i]).expect("ascii number");
        self.pos = i;
        if is_float {
            Ok(Value::Float(text.parse::<f64>().unwrap_or(f64::NAN)))
        } else {
            match text.parse::<i128>() {
                Ok(v) => Ok(Value::Int(v)),
                Err(_) => {
                    let trimmed = text.strip_prefix('-').unwrap_or(text);
                    let negative = text.starts_with('-') && trimmed.bytes().any(|d| d != b'0');
                    Ok(Value::BigInt(format!("{}{}", if negative { "-" } else { "" }, trimmed)))
                }
            }
        }
    }

    fn string(&mut self) -> Result<PyStr, ParseError> {
        let mut out = Vec::new();
        loop {
            let Some(&byte) = self.bytes.get(self.pos) else {
                return Err(self.error("Unterminated string starting at"));
            };
            match byte {
                b'"' => {
                    self.pos += 1;
                    return Ok(PyStr::from_wtf8_unchecked(out));
                }
                b'\\' => {
                    self.pos += 1;
                    let Some(&escape) = self.bytes.get(self.pos) else {
                        return Err(self.error("Unterminated string starting at"));
                    };
                    self.pos += 1;
                    let simple = match escape {
                        b'"' => Some(b'"'),
                        b'\\' => Some(b'\\'),
                        b'/' => Some(b'/'),
                        b'b' => Some(0x08),
                        b'f' => Some(0x0C),
                        b'n' => Some(b'\n'),
                        b'r' => Some(b'\r'),
                        b't' => Some(b'\t'),
                        b'u' => None,
                        _ => {
                            self.pos -= 2;
                            return Err(self.error("Invalid \\escape"));
                        }
                    };
                    if let Some(c) = simple {
                        out.push(c);
                        continue;
                    }
                    let mut cp = self.hex4()?;
                    if (0xD800..=0xDBFF).contains(&cp) && self.rest().starts_with(b"\\u") {
                        let save = self.pos;
                        self.pos += 2;
                        match self.hex4() {
                            Ok(low) if (0xDC00..=0xDFFF).contains(&low) => {
                                cp = 0x10000 + (((cp - 0xD800) << 10) | (low - 0xDC00));
                            }
                            _ => self.pos = save,
                        }
                    }
                    encode_code_point(cp, &mut out);
                }
                0x00..=0x1F => return Err(self.error("Invalid control character at")),
                _ => {
                    out.push(byte);
                    self.pos += 1;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let digits = self.bytes.get(self.pos..self.pos + 4);
        let parsed = digits
            .and_then(|d| std::str::from_utf8(d).ok())
            .filter(|d| d.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|d| u32::from_str_radix(d, 16).ok());
        match parsed {
            Some(v) => {
                self.pos += 4;
                Ok(v)
            }
            None => Err(self.error("Invalid \\uXXXX escape")),
        }
    }

    fn array(&mut self) -> Result<Value, ParseError> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.error("Expecting ',' delimiter")),
            }
        }
    }

    fn object(&mut self) -> Result<Value, ParseError> {
        self.pos += 1;
        let mut object = Object::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Ok(Value::Object(object));
        }
        loop {
            self.skip_ws();
            if self.bytes.get(self.pos) != Some(&b'"') {
                return Err(self.error("Expecting property name enclosed in double quotes"));
            }
            self.pos += 1;
            let key = self.string()?;
            self.skip_ws();
            if self.bytes.get(self.pos) != Some(&b':') {
                return Err(self.error("Expecting ':' delimiter"));
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.value()?;
            object.insert(key, value);
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(object));
                }
                _ => return Err(self.error("Expecting ',' delimiter")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumps_matches_python_defaults() {
        let value = loads_str(r#"{"b": [1, 2.5, null, true], "a": "é\u0001\ud83d\ude00\"", "c": 1e16}"#).unwrap();
        assert_eq!(
            dumps(&value, DEFAULT),
            r#"{"b": [1, 2.5, null, true], "a": "\u00e9\u0001\ud83d\ude00\"", "c": 1e+16}"#
        );
        assert_eq!(
            dumps(&value, SORTED),
            r#"{"a": "\u00e9\u0001\ud83d\ude00\"", "b": [1, 2.5, null, true], "c": 1e+16}"#
        );
        assert_eq!(dumps(&value, COMPACT), r#"{"b":[1,2.5,null,true],"a":"\u00e9\u0001\ud83d\ude00\"","c":1e+16}"#);
    }

    #[test]
    fn float_repr_matches_python() {
        for (value, expected) in [
            (1.0, "1.0"),
            (0.1, "0.1"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (123456789012345680.0, "1.2345678901234568e+17"),
            (1e15, "1000000000000000.0"),
            (-2.5e-7, "-2.5e-07"),
            (0.0, "0.0"),
        ] {
            assert_eq!(float_repr(value, true), expected, "{value}");
        }
    }

    #[test]
    fn lone_surrogates_round_trip() {
        let value = loads_str(r#""\udc80x""#).unwrap();
        assert_eq!(dumps(&value, DEFAULT), r#""\udc80x""#);
        let escaped = value.as_pystr().unwrap().encode_surrogateescape().unwrap();
        assert_eq!(escaped, b"\x80x");
    }

    #[test]
    fn rejects_what_python_rejects() {
        for bad in ["", "{", "[1,]", "{\"a\" 1}", "\"\u{1}\"", "01", "1 2", "'x'", "\u{feff}1"] {
            assert!(loads_str(bad).is_err(), "{bad:?}");
        }
        assert_eq!(loads_str("-0").unwrap(), Value::Int(0));
        assert!(matches!(loads_str("NaN").unwrap(), Value::Float(f) if f.is_nan()));
        assert_eq!(
            loads_str("123456789012345678901234567890123456789012").unwrap(),
            Value::BigInt("123456789012345678901234567890123456789012".into())
        );
    }

    #[test]
    fn duplicate_keys_keep_first_position_and_last_value() {
        let value = loads_str(r#"{"a": 1, "b": 2, "a": 3}"#).unwrap();
        assert_eq!(dumps(&value, COMPACT), r#"{"a":3,"b":2}"#);
    }
}
