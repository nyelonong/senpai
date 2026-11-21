//! Line-oriented rendering of a markdown document for the document reviewer.
//!
//! `rows` returns one row per source line, in order, 1-based, so a comment on
//! "line 42" always names exactly one source line; `blocks` groups rows into
//! rendered blocks that each carry their source range. Every fragment is
//! HTML-safe: the source is escaped before any formatting is applied.
//!
//! The patterns are the Python reviewer's, with `\s` and `\w` spelled out as
//! Python defines them so the same lines render the same way.

use std::sync::LazyLock;

use fancy_regex::{Captures, Regex};

use crate::json::{Object, Value};
use crate::pystr::{py_isspace_char, py_strip};

// Python's `\s` adds the information separators U+001C..U+001F to Unicode
// White_Space, and its `\w` is "alphanumeric or underscore".
const SPACE: &str = r"\s\x{1C}-\x{1F}";
const WORD: &str = r"\p{L}\p{N}_";

fn compile(pattern: &str) -> Regex {
    let expanded = pattern.replace("{S}", SPACE).replace("{W}", WORD);
    Regex::new(&expanded).expect("markdown pattern compiles")
}

static HEADING: LazyLock<Regex> = LazyLock::new(|| compile(r"^(#{1,6})[ \t]+(.*)$"));
static HEADING_TAIL: LazyLock<Regex> = LazyLock::new(|| compile(r"(?:[ \t]+#+)+[ \t]*$"));
static QUOTE: LazyLock<Regex> = LazyLock::new(|| compile(r"^>[ \t]?"));
static LIST: LazyLock<Regex> = LazyLock::new(|| compile(r"^(?:[-*+]|\d+[.)])(?:[ \t]|$)"));
static TASK: LazyLock<Regex> = LazyLock::new(|| compile(r"^\[([ xX])\](?:[ \t]|$)"));
static RULE: LazyLock<Regex> = LazyLock::new(|| compile(r"^([*_-])(?:[ \t]*\1){2,}$"));
static CODE_SPAN: LazyLock<Regex> = LazyLock::new(|| compile(r"(`+)([^`]*?)\1"));
static LINK: LazyLock<Regex> = LazyLock::new(|| compile(r"\[([^\]\n]*)\]\([^)\n]*\)"));
static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| compile(r"\x00(\d+)\x00"));
static BOLD: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"\*\*(?=[^{S}*])(.+?)(?<=[^{S}*])\*\*|__(?=[^{S}_])([^_]+?)(?<=[^{S}_])__(?![{W}])")
});
static STRIKE: LazyLock<Regex> = LazyLock::new(|| compile(r"~~(?=[^{S}~])(.+?)(?<=[^{S}~])~~"));
static ITALIC: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"\*(?=[^{S}*])([^*]+?)(?<=[^{S}*])\*|(?<![{W}*\\])_(?=[^{S}_])([^_]+?)(?<=[^{S}_])_(?![{W}])")
});

const BOM: char = '\u{FEFF}';
const CHECKED_MARK: &str = "\u{2611} ";
const UNCHECKED_MARK: &str = "\u{2610} ";

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn sub(regex: &Regex, text: &str, mut replace: impl FnMut(&Captures<str>) -> String) -> String {
    regex
        .try_replacen(text, 0, |caps: &Captures<str>| replace(caps))
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| text.to_string())
}

fn group<'t>(caps: &Captures<'t, str>, index: usize) -> &'t str {
    caps.get(index).map_or("", |m| m.as_str())
}

fn first_group<'t>(caps: &Captures<'t, str>) -> &'t str {
    match caps.get(1).map(|m| m.as_str()) {
        Some(text) if !text.is_empty() => text,
        _ => group(caps, 2),
    }
}

fn source_lines(text: &str) -> Vec<String> {
    let text = text.strip_prefix(BOM).unwrap_or(text);
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_string).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

fn emphasis(text: &str) -> String {
    let out = escape(text);
    let out = sub(&BOLD, &out, |c| format!("<b>{}</b>", first_group(c)));
    let out = sub(&STRIKE, &out, |c| format!("<s>{}</s>", group(c, 1)));
    sub(&ITALIC, &out, |c| format!("<i>{}</i>", first_group(c)))
}

/// Inline markdown. Links keep their label and lose their target: the page has
/// no use for navigation, so no `<a>` tag is ever emitted.
fn inline(text: &str) -> String {
    let mut stashed: Vec<String> = Vec::new();
    let work = sub(&CODE_SPAN, text, |c| {
        stashed.push(format!("<code>{}</code>", escape(group(c, 2))));
        format!("\x00{}\x00", stashed.len() - 1)
    });
    let work = sub(&LINK, &work, |c| {
        stashed.push(emphasis(group(c, 1)));
        format!("\x00{}\x00", stashed.len() - 1)
    });
    let out = emphasis(&work);
    sub(&PLACEHOLDER, &out, |c| {
        let index: Option<usize> = group(c, 1).parse().ok();
        match index.and_then(|i| stashed.get(i)) {
            Some(rendered) => rendered.clone(),
            None => group(c, 0).to_string(),
        }
    })
}

fn fence_marker(stripped: &str) -> Option<(char, usize)> {
    if stripped.starts_with("```") || stripped.starts_with("~~~") {
        let marker = stripped.chars().next().expect("non-empty");
        let count = stripped.chars().take_while(|&c| c == marker).count();
        return Some((marker, count));
    }
    None
}

fn is_match(regex: &Regex, text: &str) -> bool {
    regex.is_match(text).unwrap_or(false)
}

fn classify(stripped: &str) -> &'static str {
    if is_match(&HEADING, stripped) {
        "heading"
    } else if is_match(&QUOTE, stripped) {
        "quote"
    } else if stripped.starts_with('|') && stripped.ends_with('|') && stripped.matches('|').count() >= 3 {
        "table"
    } else if is_match(&RULE, stripped) {
        "rule"
    } else if is_match(&LIST, stripped) {
        "list"
    } else if stripped.is_empty() {
        "blank"
    } else {
        "text"
    }
}

fn row(number: usize, stripped: &str, raw: &str) -> Object {
    let kind = classify(stripped);
    let mut row = Object::new()
        .with("n", number)
        .with("kind", kind)
        .with("text", if raw.is_empty() { stripped } else { raw });
    let html = match kind {
        "blank" => String::new(),
        "rule" => escape(stripped),
        "heading" => {
            let caps = HEADING.captures(stripped).ok().flatten().expect("classified as heading");
            row.insert("level", Value::from(group(&caps, 1).chars().count().min(6)));
            let body = group(&caps, 2).trim_end_matches(py_isspace_char);
            inline(&sub(&HEADING_TAIL, body, |_| String::new()))
        }
        "quote" => {
            let end = QUOTE.find(stripped).ok().flatten().map_or(0, |m| m.end());
            inline(&stripped[end..])
        }
        "table" => {
            let inner = &stripped[1..stripped.len() - 1];
            inline(py_strip(inner))
        }
        "text" => inline(stripped),
        _ => {
            let end = LIST.find(stripped).ok().flatten().map_or(0, |m| m.end());
            let content = &stripped[end..];
            match TASK.captures(content).ok().flatten() {
                Some(task) => {
                    let checked = matches!(group(&task, 1), "x" | "X");
                    row.insert("checked", Value::Bool(checked));
                    let rest = &content[task.get(0).expect("match").end()..];
                    format!("{}{}", if checked { CHECKED_MARK } else { UNCHECKED_MARK }, inline(rest))
                }
                None => inline(content),
            }
        }
    };
    row.insert("html", Value::from(html));
    row
}

fn code_row(number: usize, line: &str) -> Object {
    Object::new()
        .with("n", number)
        .with("kind", "code")
        .with("html", escape(line))
        .with("text", line)
}

/// One row per source line, in order. Malformed input never fails: anything
/// unclassifiable is a `text` row.
pub fn rows(text: &str) -> Vec<Object> {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for (offset, line) in source_lines(text).iter().enumerate() {
        let number = offset + 1;
        let stripped = py_strip(line);
        let marker = fence_marker(stripped);
        if let Some((open_char, open_count)) = fence {
            if let Some((c, count)) = marker {
                let tail = &stripped[count..];
                if c == open_char && count >= open_count && py_strip(tail).is_empty() {
                    fence = None;
                }
            }
            out.push(code_row(number, line));
            continue;
        }
        if marker.is_some() {
            fence = marker;
            out.push(code_row(number, line));
            continue;
        }
        out.push(row(number, stripped, line));
    }
    out
}

const RUN_KINDS: [&str; 5] = ["code", "table", "quote", "blank", "text"];

fn kind_of(row: &Object) -> &str {
    row.get("kind").and_then(Value::as_pystr).and_then(|s| s.as_str()).unwrap_or("text")
}

fn string_of<'a>(row: &'a Object, key: &str) -> &'a str {
    row.get(key).and_then(Value::as_pystr).and_then(|s| s.as_str()).unwrap_or("")
}

fn block(kind: &str, taken: &[&Object]) -> Object {
    let html: Vec<&str> = taken.iter().map(|r| string_of(r, "html")).collect();
    let mut block = Object::new()
        .with("kind", kind)
        .with("start", taken[0].get("n").cloned().unwrap_or(Value::Null))
        .with("end", taken[taken.len() - 1].get("n").cloned().unwrap_or(Value::Null))
        .with("html", html.join("\n"));
    for key in ["level", "checked"] {
        if let Some(value) = taken[0].get(key) {
            block.insert(key, value.clone());
        }
    }
    let source: Vec<&str> = taken.iter().map(|r| string_of(r, "text")).collect();
    block.insert("source", Value::from(source.join("\n")));
    block
}

/// Groups rows into rendered blocks whose source ranges are contiguous,
/// non-overlapping, and cover every line exactly once.
pub fn blocks(text: &str) -> Vec<Object> {
    let lines = rows(text);
    let mut grouped = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let kind = kind_of(&lines[index]).to_string();
        if kind == "heading" || kind == "rule" {
            grouped.push(block(&kind, &[&lines[index]]));
            index += 1;
            continue;
        }
        let mut taken = vec![&lines[index]];
        index += 1;
        if kind == "list" {
            while index < lines.len() && kind_of(&lines[index]) == "text" {
                taken.push(&lines[index]);
                index += 1;
            }
        } else {
            while index < lines.len() && kind_of(&lines[index]) == kind && RUN_KINDS.contains(&kind.as_str()) {
                taken.push(&lines[index]);
                index += 1;
            }
        }
        grouped.push(block(&kind, &taken));
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    fn html(source: &str) -> String {
        string_of(&rows(source)[0], "html").to_string()
    }

    #[test]
    fn inline_formatting_matches_python() {
        assert_eq!(html("**bold** and *it* and ~~gone~~"), "<b>bold</b> and <i>it</i> and <s>gone</s>");
        assert_eq!(html("***both***"), "<i><b>both</b></i>");
        assert_eq!(html("snake_case_name stays"), "snake_case_name stays");
        assert_eq!(html("`a < b` and [label](http://x)"), "<code>a &lt; b</code> and label");
        assert_eq!(html("before <script>x</script>"), "before &lt;script&gt;x&lt;/script&gt;");
        assert_eq!(html("a\x00\x30\x00 b"), "a\x00\x30\x00 b");
    }

    #[test]
    fn rows_cover_every_line() {
        let produced = rows("# Title #\n\n- [x] done\n```py\nx = 1\n\n```\n---\n| a | b |\n> quote\n");
        let kinds: Vec<&str> = produced.iter().map(kind_of).collect();
        assert_eq!(kinds, ["heading", "blank", "list", "code", "code", "code", "code", "rule", "table", "quote"]);
        assert_eq!(produced[0].get("level"), Some(&Value::Int(1)));
        assert_eq!(string_of(&produced[0], "html"), "Title");
        assert_eq!(produced[2].get("checked"), Some(&Value::Bool(true)));
        assert_eq!(
            json::dumps(&Value::Object(produced[0].clone()), json::COMPACT),
            r##"{"n":1,"kind":"heading","text":"# Title #","level":1,"html":"Title"}"##
        );
    }

    #[test]
    fn blocks_group_runs_and_list_continuations() {
        let grouped = blocks("- item\n  wrapped\ntext\nmore\n\n\n");
        let ranges: Vec<(Value, Value)> = grouped
            .iter()
            .map(|b| (b.get("start").cloned().unwrap(), b.get("end").cloned().unwrap()))
            .collect();
        assert_eq!(ranges, [(Value::Int(1), Value::Int(4)), (Value::Int(5), Value::Int(6))]);
    }
}
