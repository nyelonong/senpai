//! `senpai tui`: the inbox, reviews and questions in a terminal, with keys and
//! mouse. It is one more way to send a decision: every decision goes to the
//! service, which checks it exactly as it checks the browser page's.

use std::io::Stdout;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use unicode_width::UnicodeWidthChar;

use crate::json::{Object, Value};
use crate::pystr::PyStr;

const REFRESH: Duration = Duration::from_millis(1000);
const SCROLL_STEP: usize = 3;
const CONFIRM_GUARD: Duration = Duration::from_millis(400);
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 10;

// ---------------------------------------------------------------------------
// the service, as the terminal UI sees it
// ---------------------------------------------------------------------------

/// What the terminal UI needs from the inbox service.
pub trait Inbox {
    fn items(&mut self) -> Result<Vec<Item>, String>;
    fn payload(&mut self, id: i128) -> Result<Object, String>;
    /// `key` is the item's secret from `payload`, so a decision can only
    /// reach the item that was shown, even if the service restarted.
    fn decide(&mut self, id: i128, key: &str, decision: Value) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: i128,
    pub kind: String,
    pub title: String,
    pub places: Vec<String>,
    pub age: i128,
}

impl Item {
    fn from_value(value: &Value) -> Option<Item> {
        let object = value.as_object()?;
        Some(Item {
            id: object.get("id").and_then(Value::as_int)?,
            kind: text(object, "kind"),
            title: clean_lines(&raw_text(object, "title")),
            places: object
                .get("places")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_pystr).map(|p| clean(&p.display())).collect())
                .unwrap_or_default(),
            age: object.get("age").and_then(Value::as_int).unwrap_or(0),
        })
    }
}

fn text(object: &Object, key: &str) -> String {
    clean(&raw_text(object, key))
}

fn raw_text(object: &Object, key: &str) -> String {
    object.get(key).and_then(Value::as_pystr).map(PyStr::display).unwrap_or_default()
}

/// Content from the reviewed change is shown, never obeyed: control
/// characters become visible symbols so they cannot move the cursor, erase
/// lines or recolour the screen the person approves from. Tabs become spaces.
fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\t' => out.push_str("    "),
            '\u{0}'..='\u{1f}' => out.push(char::from_u32(0x2400 + c as u32).unwrap_or('\u{FFFD}')),
            '\u{7f}' => out.push('\u{2421}'),
            '\u{80}'..='\u{9f}' => out.push('\u{FFFD}'),
            _ => out.push(c),
        }
    }
    out
}

/// `clean`, keeping line breaks so a title can span lines.
fn clean_lines(text: &str) -> String {
    text.split('\n').map(clean).collect::<Vec<_>>().join("\n")
}

/// The real service, over its private socket.
pub struct Service;

impl Inbox for Service {
    fn items(&mut self) -> Result<Vec<Item>, String> {
        let reply = crate::inbox::request(Object::new().with("op", "items")).map_err(|e| e.message())?;
        let items = reply.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(items.iter().filter_map(Item::from_value).collect())
    }

    fn payload(&mut self, id: i128) -> Result<Object, String> {
        crate::inbox::request(Object::new().with("op", "payload").with("id", Value::Int(id))).map_err(|e| e.message())
    }

    fn decide(&mut self, id: i128, key: &str, decision: Value) -> Result<(), String> {
        let message = Object::new().with("op", "decide").with("id", Value::Int(id)).with("key", key);
        crate::inbox::request(message.with("decision", decision))
            .map(|_| ())
            .map_err(|e| e.message())
    }
}

// ---------------------------------------------------------------------------
// review rows
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum RowKind {
    File,
    Meta,
    Hunk,
    Add,
    Remove,
    Context,
    Note,
    Heading,
    Code,
    Quote,
    Table,
    List,
    Text,
    Blank,
}

/// The line a row stands for, as a comment target names it.
#[derive(Clone, Debug, PartialEq)]
struct Anchor {
    repository: Option<PyStr>,
    path: PyStr,
    side: Option<&'static str>,
    line: i128,
}

#[derive(Clone, Debug)]
struct Row {
    kind: RowKind,
    gutter: String,
    text: String,
    anchor: Option<Anchor>,
}

fn int(object: &Object, key: &str) -> Option<i128> {
    object.get(key).and_then(Value::as_int)
}

fn diff_rows(payload: &Object) -> Vec<Row> {
    let mut rows = Vec::new();
    for repository in payload.get("repositories").and_then(Value::as_array).into_iter().flatten() {
        let Some(repository) = repository.as_object() else { continue };
        for file in repository.get("files").and_then(Value::as_array).into_iter().flatten() {
            let Some(file) = file.as_object() else { continue };
            let root = file.get("repository").and_then(Value::as_pystr).cloned().unwrap_or_default();
            let path = file.get("path").and_then(Value::as_pystr).cloned().unwrap_or_default();
            let old = text(file, "old_path");
            let name = if old.is_empty() { clean(&path.display()) } else { format!("{old} → {}", clean(&path.display())) };
            rows.push(Row {
                kind: RowKind::File,
                gutter: String::new(),
                text: format!(
                    "{} {name}  +{} −{}",
                    text(file, "status"),
                    int(file, "added").unwrap_or(0),
                    int(file, "removed").unwrap_or(0)
                ),
                anchor: None,
            });
            if matches!(file.get("binary"), Some(Value::Bool(true))) {
                rows.push(Row { kind: RowKind::Note, gutter: String::new(), text: "binary file".into(), anchor: None });
                continue;
            }
            for line in file.get("lines").and_then(Value::as_array).into_iter().flatten() {
                let Some(line) = line.as_object() else { continue };
                let (old_line, new_line) = (int(line, "old_line"), int(line, "new_line"));
                let body = text(line, "text");
                let number = |n: Option<i128>| n.map_or(String::new(), |n| n.to_string());
                let gutter = format!("{:>5} {:>5}", number(old_line), number(new_line));
                let anchor = |side: &'static str, line: Option<i128>| {
                    line.map(|line| Anchor { repository: Some(root.clone()), path: path.clone(), side: Some(side), line })
                };
                let (kind, anchor) = match text(line, "type").as_str() {
                    "add" => (RowKind::Add, anchor("new", new_line)),
                    "remove" => (RowKind::Remove, anchor("old", old_line)),
                    "context" => (RowKind::Context, anchor("new", new_line)),
                    _ if body.starts_with("@@") => (RowKind::Hunk, None),
                    _ => (RowKind::Meta, None),
                };
                if kind == RowKind::Meta && !body.starts_with('\\') {
                    continue;
                }
                rows.push(Row { kind, gutter, text: body, anchor });
            }
        }
    }
    if rows.is_empty() {
        rows.push(Row { kind: RowKind::Note, gutter: String::new(), text: "no changes".into(), anchor: None });
    }
    rows
}

fn document_rows(payload: &Object) -> Vec<Row> {
    let path = payload.get("path").and_then(Value::as_pystr).cloned().unwrap_or_default();
    let mut commentable = Vec::new();
    for block in payload.get("blocks").and_then(Value::as_array).into_iter().flatten() {
        if let Some(block) = block.as_object() {
            let (start, end) = (int(block, "start").unwrap_or(0), int(block, "end").unwrap_or(-1));
            commentable.extend(start..=end);
        }
    }
    let mut rows = Vec::new();
    for row in payload.get("rows").and_then(Value::as_array).into_iter().flatten() {
        let Some(row) = row.as_object() else { continue };
        let n = int(row, "n").unwrap_or(0);
        let kind = match text(row, "kind").as_str() {
            "heading" => RowKind::Heading,
            "code" => RowKind::Code,
            "quote" => RowKind::Quote,
            "table" => RowKind::Table,
            "list" => RowKind::List,
            "blank" => RowKind::Blank,
            _ => RowKind::Text,
        };
        let anchor = commentable
            .contains(&n)
            .then(|| Anchor { repository: None, path: path.clone(), side: None, line: n });
        rows.push(Row { kind, gutter: format!("{n:>5}"), text: text(row, "text"), anchor });
    }
    rows
}

fn row_style(kind: RowKind) -> Style {
    match kind {
        RowKind::File => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        RowKind::Hunk => Style::new().fg(Color::Magenta),
        RowKind::Meta | RowKind::Note => Style::new().fg(Color::DarkGray),
        RowKind::Add => Style::new().fg(Color::Green),
        RowKind::Remove => Style::new().fg(Color::Red),
        RowKind::Heading => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        RowKind::Code => Style::new().fg(Color::Yellow),
        RowKind::Quote => Style::new().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        RowKind::Table => Style::new().fg(Color::Blue),
        _ => Style::new(),
    }
}

/// Splits `text` into pieces no wider than `width` columns.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = vec![String::new()];
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width && used > 0 {
            lines.push(String::new());
            used = 0;
        }
        lines.last_mut().expect("one line").push(c);
        used += w;
    }
    lines
}

// ---------------------------------------------------------------------------
// screens
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct Comment {
    text: String,
    /// The first and last selected rows; none for a whole-review comment.
    target: Option<(Anchor, Anchor)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Approve,
    RequestChanges,
    ReturnToPlanning,
    Comment,
    ReviewComment,
    Comments,
    Back,
    Send,
    Dismiss,
    Confirm,
    Cancel,
    Open(usize),
    Pick(usize),
    EditComment(usize),
}

impl Action {
    fn decision(self) -> Option<&'static str> {
        match self {
            Action::Approve => Some("approve"),
            Action::RequestChanges => Some("request-changes"),
            Action::ReturnToPlanning => Some("return-to-planning"),
            _ => None,
        }
    }
}

enum Editing {
    New(Option<(Anchor, Anchor)>),
    Existing(usize),
}

/// An open decision dialog. Cancel has the focus, and input in the first
/// moments after it opens is ignored, so typing or a double click that runs
/// into it cannot send a decision.
#[derive(Clone, Copy)]
struct Confirm {
    decision: &'static str,
    send_focused: bool,
    opened: Instant,
}

struct Review {
    id: i128,
    key: String,
    document: bool,
    title: String,
    rows: Vec<Row>,
    cursor: usize,
    selection: Option<usize>,
    scroll: usize,
    comments: Vec<Comment>,
    showing_comments: bool,
    editor: Option<(Editing, String)>,
    confirm: Option<Confirm>,
    note: Option<String>,
    /// Screen line → row, from the last draw, for mouse hits.
    lines: Vec<(u16, usize)>,
    view: Rect,
    page: usize,
    dragging: bool,
}

struct Question {
    id: i128,
    key: String,
    title: String,
    options: Vec<String>,
    chosen: Option<usize>,
    answer: String,
}

enum Screen {
    List { items: Vec<Item>, cursor: usize },
    Review(Box<Review>),
    Question(Question),
}

pub struct App {
    screen: Screen,
    /// Set by `--item`: the program ends when that item is done.
    only: Option<i128>,
    status: String,
    buttons: Vec<(Rect, Action)>,
    list_rows: Vec<(Rect, usize)>,
    pub done: bool,
    help: bool,
    /// How long a new decision dialog ignores input.
    guard: Duration,
}

impl App {
    pub fn new(only: Option<i128>) -> Self {
        App {
            screen: Screen::List { items: Vec::new(), cursor: 0 },
            only,
            status: String::new(),
            buttons: Vec::new(),
            list_rows: Vec::new(),
            done: false,
            help: false,
            guard: CONFIRM_GUARD,
        }
    }

    fn current_id(&self) -> Option<i128> {
        self.current().map(|(id, _)| id)
    }

    fn current(&self) -> Option<(i128, String)> {
        match &self.screen {
            Screen::Review(review) => Some((review.id, review.key.clone())),
            Screen::Question(question) => Some((question.id, question.key.clone())),
            Screen::List { .. } => None,
        }
    }

    /// Loads the inbox; opens the `--item` target, and notices when the open
    /// item was decided elsewhere or withdrawn.
    pub fn refresh(&mut self, inbox: &mut dyn Inbox) {
        let items = match inbox.items() {
            Ok(items) => items,
            Err(error) => {
                self.status = clean(&error);
                return;
            }
        };
        if let Some(id) = self.current_id() {
            if !items.iter().any(|i| i.id == id) {
                self.finish("That item is no longer waiting: it was decided elsewhere or withdrawn.");
            }
        }
        if let Screen::List { items: shown, cursor } = &mut self.screen {
            *cursor = (*cursor).min(items.len().saturating_sub(1));
            *shown = items;
            if let Some(id) = self.only {
                if shown.iter().any(|i| i.id == id) {
                    self.open(inbox, id);
                } else {
                    self.done = true;
                }
            }
        }
    }

    fn finish(&mut self, message: &str) {
        self.status = message.to_string();
        if self.only.is_some() {
            self.done = true;
        }
        self.screen = Screen::List { items: Vec::new(), cursor: 0 };
    }

    fn open(&mut self, inbox: &mut dyn Inbox, id: i128) {
        let reply = match inbox.payload(id) {
            Ok(reply) => reply,
            Err(error) => {
                self.status = clean(&error);
                return;
            }
        };
        let title = clean_lines(&raw_text(&reply, "title"));
        let kind = text(&reply, "kind");
        let key = raw_text(&reply, "key");
        if kind == "question" {
            let options = reply
                .get("options")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_pystr).map(|o| clean(&o.display())).collect())
                .unwrap_or_default();
            self.screen = Screen::Question(Question { id, key, title, options, chosen: None, answer: String::new() });
            return;
        }
        let payload = reply.get("payload").and_then(Value::as_object).cloned().unwrap_or_default();
        let document = kind == "document";
        let rows = if document { document_rows(&payload) } else { diff_rows(&payload) };
        let cursor = rows.iter().position(|r| r.anchor.is_some()).unwrap_or(0);
        let authored = payload.get("guide").and_then(|g| g.get("authored"));
        let note = (!document && matches!(authored, Some(Value::Bool(true))))
            .then(|| "This review has a guide; open it in the browser inbox to read it.".to_string());
        let mut title = title;
        if document {
            title = format!("{title}\n{}", text(&payload, "path"));
        }
        self.screen = Screen::Review(Box::new(Review {
            id,
            key,
            document,
            title,
            rows,
            cursor,
            selection: None,
            scroll: 0,
            comments: Vec::new(),
            showing_comments: false,
            editor: None,
            confirm: None,
            note,
            lines: Vec::new(),
            view: Rect::default(),
            page: 10,
            dragging: false,
        }));
        self.status.clear();
    }

    // -- input ----------------------------------------------------------------

    pub fn on_event(&mut self, event: Event, inbox: &mut dyn Inbox) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key, inbox),
            Event::Mouse(mouse) => self.on_mouse(mouse, inbox),
            Event::Paste(text) => {
                if let Some(buffer) = self.text_field() {
                    buffer.extend(text.chars().map(|c| if c.is_control() { ' ' } else { c }));
                }
            }
            _ => {}
        }
    }

    fn text_field(&mut self) -> Option<&mut String> {
        match &mut self.screen {
            Screen::Review(review) => review.editor.as_mut().map(|(_, buffer)| buffer),
            Screen::Question(question) => Some(&mut question.answer),
            Screen::List { .. } => None,
        }
    }

    fn on_key(&mut self, key: KeyEvent, inbox: &mut dyn Inbox) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.done = true;
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        match &mut self.screen {
            Screen::List { items, cursor } => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => self.done = true,
                KeyCode::Down | KeyCode::Char('j') => *cursor = (*cursor + 1).min(items.len().saturating_sub(1)),
                KeyCode::Up | KeyCode::Char('k') => *cursor = cursor.saturating_sub(1),
                KeyCode::Enter => {
                    if let Some(id) = items.get(*cursor).map(|i| i.id) {
                        self.open(inbox, id);
                    }
                }
                KeyCode::Char('?') => self.help = true,
                _ => {}
            },
            Screen::Question(question) => match key.code {
                KeyCode::Esc => self.back(),
                KeyCode::Enter => self.act(Action::Send, inbox),
                KeyCode::Backspace => {
                    question.answer.pop();
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => self.act(Action::Dismiss, inbox),
                KeyCode::Char(c) if question.answer.is_empty() && c.is_ascii_digit() && !question.options.is_empty() => {
                    let index = c.to_digit(10).unwrap_or(0) as usize;
                    if (1..=question.options.len()).contains(&index) {
                        question.chosen = Some(index - 1);
                    }
                }
                KeyCode::Char(c) if !c.is_control() => question.answer.push(c),
                _ => {}
            },
            Screen::Review(review) => {
                if let Some(confirm) = &mut review.confirm {
                    if confirm.opened.elapsed() < self.guard {
                        return;
                    }
                    match key.code {
                        KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                            confirm.send_focused = !confirm.send_focused;
                        }
                        KeyCode::Enter if confirm.send_focused => self.act(Action::Confirm, inbox),
                        _ => review.confirm = None,
                    }
                    return;
                }
                if let Some((_, buffer)) = &mut review.editor {
                    match key.code {
                        KeyCode::Enter => self.save_comment(),
                        KeyCode::Esc => review.editor = None,
                        KeyCode::Backspace => {
                            buffer.pop();
                        }
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => buffer.clear(),
                        KeyCode::Char(c) if !c.is_control() => buffer.push(c),
                        _ => {}
                    }
                    return;
                }
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                match key.code {
                    KeyCode::Char('q') if self.only.is_some() => self.done = true,
                    KeyCode::Esc | KeyCode::Char('q') => {
                        if review.selection.take().is_none() && !std::mem::take(&mut review.showing_comments) {
                            self.back();
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => review.move_cursor(1, shift),
                    KeyCode::Up | KeyCode::Char('k') => review.move_cursor(-1, shift),
                    KeyCode::PageDown | KeyCode::Char(' ') => review.move_cursor(review.page as isize, shift),
                    KeyCode::PageUp => review.move_cursor(-(review.page as isize), shift),
                    KeyCode::Home | KeyCode::Char('g') => review.jump(0),
                    KeyCode::End | KeyCode::Char('G') => review.jump(review.rows.len().saturating_sub(1)),
                    KeyCode::Char('J') => review.next_of(RowKind::File, true),
                    KeyCode::Char('K') => review.next_of(RowKind::File, false),
                    KeyCode::Char('n') => review.next_of(RowKind::Hunk, true),
                    KeyCode::Char('p') => review.next_of(RowKind::Hunk, false),
                    KeyCode::Char('v') => review.selection = if review.selection.is_some() { None } else { Some(review.cursor) },
                    KeyCode::Char('c') => self.act(Action::Comment, inbox),
                    KeyCode::Char('C') => self.act(Action::ReviewComment, inbox),
                    KeyCode::Char('l') => review.showing_comments = !review.showing_comments,
                    KeyCode::Char('a') => self.act(Action::Approve, inbox),
                    KeyCode::Char('r') => self.act(Action::RequestChanges, inbox),
                    KeyCode::Char('x') => self.act(Action::ReturnToPlanning, inbox),
                    KeyCode::Char('?') => self.help = true,
                    KeyCode::Char(c @ '1'..='9') if review.showing_comments => {
                        let index = c.to_digit(10).unwrap_or(1) as usize - 1;
                        if index < review.comments.len() {
                            self.act(Action::EditComment(index), inbox);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent, inbox: &mut dyn Inbox) {
        let at = Position::new(mouse.column, mouse.row);
        if self.help {
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                self.help = false;
            }
            return;
        }
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            if let Some(action) = self.buttons.iter().rev().find(|(r, _)| r.contains(at)).map(|(_, a)| *a) {
                return self.act(action, inbox);
            }
            if let Some(index) = self.list_rows.iter().find(|(r, _)| r.contains(at)).map(|(_, i)| *i) {
                if let Screen::List { cursor, .. } = &mut self.screen {
                    *cursor = index;
                }
                return self.act(Action::Open(index), inbox);
            }
        }
        match &mut self.screen {
            Screen::List { items, cursor } => match mouse.kind {
                MouseEventKind::ScrollDown => *cursor = (*cursor + 1).min(items.len().saturating_sub(1)),
                MouseEventKind::ScrollUp => *cursor = cursor.saturating_sub(1),
                _ => {}
            },
            Screen::Review(review) if review.confirm.is_none() && review.editor.is_none() => match mouse.kind {
                MouseEventKind::ScrollDown => review.scroll_by(SCROLL_STEP as isize),
                MouseEventKind::ScrollUp => review.scroll_by(-(SCROLL_STEP as isize)),
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(row) = review.row_at(at) {
                        if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                            review.selection.get_or_insert(review.cursor);
                        } else {
                            review.selection = None;
                        }
                        review.cursor = row;
                        review.dragging = true;
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) if review.dragging => {
                    if let Some(row) = review.row_at(at) {
                        review.selection.get_or_insert(review.cursor);
                        review.cursor = row;
                    }
                }
                MouseEventKind::Up(_) => review.dragging = false,
                _ => {}
            },
            _ => {}
        }
    }

    fn back(&mut self) {
        if self.only.is_some() {
            self.done = true;
        } else {
            self.screen = Screen::List { items: Vec::new(), cursor: 0 };
            self.status.clear();
        }
    }

    fn save_comment(&mut self) {
        let Screen::Review(review) = &mut self.screen else { return };
        let Some((editing, buffer)) = review.editor.take() else { return };
        let text = buffer.trim().to_string();
        match editing {
            Editing::Existing(index) if text.is_empty() => {
                review.comments.remove(index);
                self.status = "Comment deleted.".into();
            }
            Editing::Existing(index) => review.comments[index].text = text,
            Editing::New(_) if text.is_empty() => {}
            Editing::New(target) => {
                review.comments.push(Comment { text, target });
                review.selection = None;
            }
        }
    }

    fn act(&mut self, action: Action, inbox: &mut dyn Inbox) {
        match &mut self.screen {
            Screen::List { items, .. } => {
                if let Action::Open(index) = action {
                    if let Some(id) = items.get(index).map(|i| i.id) {
                        self.open(inbox, id);
                    }
                }
            }
            Screen::Question(question) => match action {
                Action::Pick(index) => question.chosen = if question.chosen == Some(index) { None } else { Some(index) },
                Action::Send => {
                    let option = question.chosen.and_then(|i| question.options.get(i)).cloned();
                    let answer = Object::new().with("option", option).with("text", question.answer.as_str());
                    self.send(inbox, Value::Object(answer), "Answer sent.");
                }
                Action::Dismiss => self.send(inbox, Value::Object(Object::new().with("dismissed", true)), "Dismissed."),
                Action::Back => self.back(),
                _ => {}
            },
            Screen::Review(review) => match action {
                Action::Comment => match review.selected_target() {
                    Ok(target) => review.editor = Some((Editing::New(Some(target)), String::new())),
                    Err(message) => self.status = message,
                },
                Action::ReviewComment => review.editor = Some((Editing::New(None), String::new())),
                Action::EditComment(index) => {
                    let text = review.comments[index].text.clone();
                    review.editor = Some((Editing::Existing(index), text));
                }
                Action::Comments => review.showing_comments = !review.showing_comments,
                Action::Back => {
                    if review.confirm.take().is_none() {
                        self.back();
                    }
                }
                Action::Cancel => review.confirm = None,
                Action::Confirm => {
                    if review.confirm.is_some_and(|c| c.opened.elapsed() < self.guard) {
                        return;
                    }
                    if let Some(Confirm { decision, .. }) = review.confirm.take() {
                        let annotations = review.annotations();
                        let value = Object::new().with("decision", decision).with("annotations", Value::Array(annotations));
                        self.send(inbox, Value::Object(value), "Decision sent.");
                    }
                }
                other => {
                    if let Some(decision) = other.decision() {
                        if decision == "request-changes" && review.comments.is_empty() {
                            self.status = "Requesting changes needs at least one comment.".into();
                        } else {
                            review.confirm = Some(Confirm { decision, send_focused: false, opened: Instant::now() });
                        }
                    }
                }
            },
        }
    }

    fn send(&mut self, inbox: &mut dyn Inbox, decision: Value, done: &str) {
        let Some((id, key)) = self.current() else { return };
        match inbox.decide(id, &key, decision) {
            Ok(()) => self.finish(done),
            Err(error) => self.status = clean(&error),
        }
    }

    // -- drawing --------------------------------------------------------------

    pub fn draw(&mut self, frame: &mut Frame) {
        self.buttons.clear();
        self.list_rows.clear();
        let area = frame.area();
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            if let Screen::Review(review) = &mut self.screen {
                review.view = Rect::default();
                review.lines.clear();
            }
            let message = format!("Make this pane at least {MIN_WIDTH}×{MIN_HEIGHT} to review.");
            frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: false }), area);
            return;
        }
        let [main, bar, status] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1), Constraint::Length(1)]).areas(frame.area());
        let buttons: Vec<(&str, Action)> = match &mut self.screen {
            Screen::List { items, cursor } => {
                Self::draw_list(frame, main, items, *cursor, &mut self.list_rows);
                vec![]
            }
            Screen::Question(question) => {
                Self::draw_question(frame, main, question, &mut self.buttons);
                vec![("Send", Action::Send), ("Dismiss", Action::Dismiss), ("Back", Action::Back)]
            }
            Screen::Review(review) => {
                review.draw(frame, main, &mut self.buttons);
                let comments = if review.showing_comments { "Hide comments" } else { "Comments" };
                vec![
                    ("Approve", Action::Approve),
                    ("Request changes", Action::RequestChanges),
                    ("Return to planning", Action::ReturnToPlanning),
                    ("Comment", Action::Comment),
                    ("Review comment", Action::ReviewComment),
                    (comments, Action::Comments),
                    ("Back", Action::Back),
                ]
            }
        };
        let mut x = bar.x;
        for (label, action) in buttons {
            let width = label.chars().count() as u16 + 2;
            if x + width > bar.right() {
                break;
            }
            let area = Rect::new(x, bar.y, width, 1);
            let style = match action {
                Action::Approve => Style::new().fg(Color::Black).bg(Color::Green),
                Action::RequestChanges => Style::new().fg(Color::Black).bg(Color::Yellow),
                Action::Dismiss | Action::ReturnToPlanning => Style::new().fg(Color::Black).bg(Color::Magenta),
                _ => Style::new().fg(Color::Black).bg(Color::Gray),
            };
            frame.render_widget(Paragraph::new(format!(" {label} ")).style(style), area);
            self.buttons.push((area, action));
            x += width + 1;
        }
        let help = match &self.screen {
            Screen::List { .. } => "↑↓ move · Enter or click open · q quit · ? help",
            Screen::Question(_) => "1-9 pick · type an answer · Enter send · Ctrl-D dismiss · Esc back",
            Screen::Review(_) => "click or v + j/k select · c comment · a approve · r request changes · ? help",
        };
        let line = if self.status.is_empty() { help.to_string() } else { self.status.clone() };
        frame.render_widget(Paragraph::new(line).style(Style::new().fg(Color::DarkGray)), status);
        if let Screen::Review(review) = &self.screen {
            if let Some(confirm) = review.confirm {
                self.draw_confirm(frame, confirm, review.comments.len());
            }
        }
        if self.help {
            draw_help(frame);
        }
    }

    fn draw_list(frame: &mut Frame, area: Rect, items: &[Item], cursor: usize, hits: &mut Vec<(Rect, usize)>) {
        let block = Block::bordered().title(" senpai inbox ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if items.is_empty() {
            frame.render_widget(Paragraph::new("Nothing is waiting for you.").style(Style::new().fg(Color::DarkGray)), inner);
            return;
        }
        let mut y = inner.y;
        for (index, item) in items.iter().enumerate() {
            let height = 2 + item.places.len().min(2) as u16;
            if y + height > inner.bottom() {
                break;
            }
            let area = Rect::new(inner.x, y, inner.width, height);
            let kind = match item.kind.as_str() {
                "diff" => "diff review",
                "document" => "document review",
                other => other,
            };
            let age = if item.age < 60 { "just now".to_string() } else { format!("{} min ago", item.age / 60) };
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(format!(" {kind} "), Style::new().fg(Color::Black).bg(Color::Cyan)),
                    Span::raw(format!("  {age}")),
                ]),
                Line::from(Span::styled(item.title.replace('\n', " "), Style::new().add_modifier(Modifier::BOLD))),
            ];
            for place in item.places.iter().take(2) {
                lines.push(Line::from(Span::styled(place.clone(), Style::new().fg(Color::DarkGray))));
            }
            let style = if index == cursor { Style::new().bg(Color::Indexed(236)) } else { Style::new() };
            frame.render_widget(Paragraph::new(lines).style(style), area);
            hits.push((area, index));
            y += height + 1;
        }
    }

    fn draw_question(frame: &mut Frame, area: Rect, question: &Question, hits: &mut Vec<(Rect, Action)>) {
        let block = Block::bordered().title(" question ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let title = Paragraph::new(question.title.as_str()).wrap(Wrap { trim: false }).style(Style::new().add_modifier(Modifier::BOLD));
        let title_height = (wrap(&question.title, inner.width as usize).len() as u16).min(inner.height.saturating_sub(4));
        frame.render_widget(title, Rect::new(inner.x, inner.y, inner.width, title_height));
        let mut y = inner.y + title_height + 1;
        for (index, option) in question.options.iter().enumerate() {
            if y >= inner.bottom() {
                break;
            }
            let chosen = question.chosen == Some(index);
            let label = format!("{} {}. {option}", if chosen { "●" } else { "○" }, index + 1);
            let rect = Rect::new(inner.x, y, (label.chars().count() as u16 + 1).min(inner.width), 1);
            let style = if chosen { Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD) } else { Style::new() };
            frame.render_widget(Paragraph::new(label).style(style), rect);
            hits.push((rect, Action::Pick(index)));
            y += 1;
        }
        if y + 1 < inner.bottom() {
            let prompt = if question.options.is_empty() { "Answer: " } else { "Note (optional): " };
            let field = Line::from(vec![
                Span::styled(prompt, Style::new().fg(Color::DarkGray)),
                Span::raw(question.answer.as_str()),
                Span::styled("▏", Style::new().fg(Color::Cyan)),
            ]);
            frame.render_widget(Paragraph::new(field), Rect::new(inner.x, y + 1, inner.width, 1));
        }
    }

    fn draw_confirm(&mut self, frame: &mut Frame, confirm: Confirm, comments: usize) {
        let area = frame.area();
        let width = 52.min(area.width);
        let rect = Rect::new(area.x + (area.width - width) / 2, area.y + area.height / 3, width, 6).intersection(area);
        frame.render_widget(Clear, rect);
        let label = match confirm.decision {
            "approve" => "Approve",
            "request-changes" => "Request changes",
            _ => "Return to planning",
        };
        let block = Block::bordered().title(format!(" {label}? "));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let plural = if comments == 1 { "" } else { "s" };
        let line = |dy: u16| Rect::new(inner.x, inner.y + dy, inner.width, 1).intersection(area);
        frame.render_widget(Paragraph::new(format!("Send with {comments} comment{plural}.")), line(0));
        let hint = "Tab moves · Enter chooses · other keys cancel";
        frame.render_widget(Paragraph::new(hint).style(Style::new().fg(Color::DarkGray)), line(1));
        let cancel = Rect::new(inner.x, inner.y + 3, 10, 1).intersection(area);
        let send = Rect::new(inner.x + 12, inner.y + 3, 8, 1).intersection(area);
        let focused = Style::new().fg(Color::Black).bg(Color::White).add_modifier(Modifier::BOLD);
        let quiet = Style::new().fg(Color::Black).bg(Color::DarkGray);
        let (send_style, cancel_style) = if confirm.send_focused { (focused, quiet) } else { (quiet, focused) };
        frame.render_widget(Paragraph::new(" Cancel ").style(cancel_style), cancel);
        frame.render_widget(Paragraph::new(" Send ").style(send_style), send);
        self.buttons.push((send, Action::Confirm));
        self.buttons.push((cancel, Action::Cancel));
    }
}

fn draw_help(frame: &mut Frame) {
    let text = "\
Keys
  j/k ↑/↓        move           Space/PgDn PgUp  page
  J/K            next/previous file
  n/p            next/previous hunk
  v              start or clear a selection (then j/k)
  c              comment on the selected lines
  C              comment on the whole review
  l              show comments; 1-9 edits one (empty deletes)
  a / r / x      approve / request changes / return to planning;
                 the dialog starts on Cancel: Tab to Send, then Enter
  Esc            clear selection, then back      q quit

Mouse
  click a line to select it; drag or shift-click for a range
  wheel scrolls; click the buttons at the bottom
  every decision asks for confirmation

Press any key to close.";
    let area = frame.area();
    let width = 64.min(area.width);
    let height = 24.min(area.height);
    let rect = Rect::new(area.x + (area.width - width) / 2, area.y + (area.height - height) / 2, width, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(text).block(Block::bordered().title(" help ")), rect);
}

impl Review {
    fn move_cursor(&mut self, delta: isize, extend: bool) {
        if extend {
            self.selection.get_or_insert(self.cursor);
        }
        let last = self.rows.len().saturating_sub(1) as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
    }

    fn jump(&mut self, row: usize) {
        self.cursor = row.min(self.rows.len().saturating_sub(1));
    }

    fn next_of(&mut self, kind: RowKind, forward: bool) {
        let found = if forward {
            (self.cursor + 1..self.rows.len()).find(|&i| self.rows[i].kind == kind)
        } else {
            (0..self.cursor).rev().find(|&i| self.rows[i].kind == kind)
        };
        if let Some(row) = found {
            self.cursor = row;
            self.scroll = row;
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        let last = self.rows.len().saturating_sub(1) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, last) as usize;
        self.cursor = self.cursor.clamp(self.scroll, self.scroll + self.page.saturating_sub(1));
    }

    fn row_at(&self, at: Position) -> Option<usize> {
        if !self.view.contains(at) {
            return None;
        }
        self.lines.iter().find(|(y, _)| *y == at.y).map(|(_, row)| *row)
    }

    fn selected(&self) -> (usize, usize) {
        let anchor = self.selection.unwrap_or(self.cursor);
        (anchor.min(self.cursor), anchor.max(self.cursor))
    }

    /// The selected lines as a comment target: one file, one side.
    fn selected_target(&self) -> Result<(Anchor, Anchor), String> {
        let (first, last) = self.selected();
        let anchors: Vec<&Anchor> = self.rows[first..=last].iter().filter_map(|r| r.anchor.as_ref()).collect();
        let (Some(start), Some(end)) = (anchors.first(), anchors.last()) else {
            return Err("Select one or more lines to comment on.".into());
        };
        let same = |a: &&Anchor| a.repository == start.repository && a.path == start.path && a.side == start.side;
        if !anchors.iter().all(same) || end.line < start.line {
            return Err("Select lines from one side of one file.".into());
        }
        Ok(((*start).clone(), (*end).clone()))
    }

    fn annotations(&self) -> Vec<Value> {
        self.comments
            .iter()
            .map(|comment| {
                let target = match &comment.target {
                    None => Object::new().with("scope", "review"),
                    Some((start, end)) => {
                        let mut target = Object::new().with("scope", "line");
                        if let Some(repository) = &start.repository {
                            target.insert("repository", Value::from(repository));
                        }
                        target.insert("path", Value::from(&start.path));
                        if let Some(side) = start.side {
                            target.insert("side", Value::from(side));
                        }
                        target
                            .with("start_line", Value::Int(start.line))
                            .with("end_line", Value::Int(end.line))
                    }
                };
                Value::Object(Object::new().with("text", comment.text.as_str()).with("target", target))
            })
            .collect()
    }

    fn commented(&self, row: &Row) -> bool {
        let Some(anchor) = &row.anchor else { return false };
        self.comments.iter().any(|c| {
            c.target.as_ref().is_some_and(|(s, e)| {
                s.repository == anchor.repository
                    && s.path == anchor.path
                    && s.side == anchor.side
                    && (s.line..=e.line).contains(&anchor.line)
            })
        })
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, hits: &mut Vec<(Rect, Action)>) {
        let kind = if self.document { " document review " } else { " diff review " };
        let block = Block::bordered().title(kind);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut header: Vec<Line> = self.title.lines().map(|l| Line::from(l.to_string())).collect();
        header.truncate(3);
        if let Some(note) = &self.note {
            header.push(Line::from(Span::styled(note.clone(), Style::new().fg(Color::DarkGray))));
        }
        let header_height = (header.len() as u16).min(inner.height / 3).max(1);
        frame.render_widget(
            Paragraph::new(header).style(Style::new().add_modifier(Modifier::BOLD)).wrap(Wrap { trim: false }),
            Rect::new(inner.x, inner.y, inner.width, header_height),
        );
        let mut body = Rect::new(inner.x, inner.y + header_height + 1, inner.width, inner.height.saturating_sub(header_height + 1));
        let editor_height = u16::from(self.editor.is_some()) * 2;
        body.height = body.height.saturating_sub(editor_height);
        if self.showing_comments {
            let panel = (self.comments.len() as u16 + 2).max(3).min(body.height / 2);
            if panel >= 3 {
                body.height = body.height.saturating_sub(panel);
                self.draw_comments(frame, Rect::new(body.x, body.bottom(), body.width, panel), hits);
            }
        }
        if let Some((editing, buffer)) = &self.editor {
            let label = match editing {
                Editing::Existing(_) => "Edit comment (empty deletes): ",
                Editing::New(None) => "Review comment: ",
                Editing::New(Some(_)) => "Comment: ",
            };
            let line = Line::from(vec![
                Span::styled(label, Style::new().fg(Color::Cyan)),
                Span::raw(buffer.as_str()),
                Span::styled("▏", Style::new().fg(Color::Cyan)),
            ]);
            let y = inner.bottom().saturating_sub(editor_height) + 1;
            frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
        }
        self.view = body;
        self.page = body.height.max(1) as usize;
        self.draw_rows(frame, body);
    }

    fn draw_rows(&mut self, frame: &mut Frame, body: Rect) {
        let gutter = self.rows.iter().map(|r| r.gutter.chars().count()).max().unwrap_or(0);
        let text_width = (body.width as usize).saturating_sub(gutter + 3).max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        let height = body.height as usize;
        let wrapped = |row: &Row| wrap(&row.text, text_width);
        loop {
            let used: usize = (self.scroll..=self.cursor.min(self.rows.len().saturating_sub(1)))
                .map(|i| wrapped(&self.rows[i]).len())
                .sum();
            if used <= height || self.scroll >= self.cursor {
                break;
            }
            self.scroll += 1;
        }
        let (first, last) = self.selected();
        let selecting = self.selection.is_some();
        self.lines.clear();
        let mut lines = Vec::new();
        for index in self.scroll..self.rows.len() {
            if lines.len() >= height {
                break;
            }
            let row = &self.rows[index];
            let mut style = row_style(row.kind);
            let marked = (selecting && (first..=last).contains(&index)) || index == self.cursor;
            if marked {
                style = style.bg(Color::Indexed(237));
            }
            let mark = if self.commented(row) { "●" } else if index == self.cursor { "▶" } else { " " };
            for (part, piece) in wrapped(row).into_iter().enumerate() {
                if lines.len() >= height {
                    break;
                }
                let gutter_text = if part == 0 { format!("{:>gutter$}", row.gutter) } else { " ".repeat(gutter) };
                self.lines.push((body.y + lines.len() as u16, index));
                lines.push(Line::from(vec![
                    Span::styled(format!("{mark} "), Style::new().fg(Color::Cyan)),
                    Span::styled(gutter_text, Style::new().fg(Color::DarkGray)),
                    Span::raw(" "),
                    Span::styled(piece, style),
                ]));
            }
        }
        frame.render_widget(Paragraph::new(lines), body);
    }

    fn draw_comments(&self, frame: &mut Frame, area: Rect, hits: &mut Vec<(Rect, Action)>) {
        let block = Block::bordered().title(" comments · click or 1-9 to edit ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if self.comments.is_empty() {
            frame.render_widget(Paragraph::new("No comments yet.").style(Style::new().fg(Color::DarkGray)), inner);
            return;
        }
        for (index, comment) in self.comments.iter().enumerate() {
            let y = inner.y + index as u16;
            if y >= inner.bottom() {
                break;
            }
            let place = match &comment.target {
                None => "review".to_string(),
                Some((start, end)) => {
                    let lines = if start.line == end.line { start.line.to_string() } else { format!("{}-{}", start.line, end.line) };
                    let side = start.side.map(|s| format!(" ({s})")).unwrap_or_default();
                    format!("{}:{lines}{side}", clean(&start.path.display()))
                }
            };
            let rect = Rect::new(inner.x, y, inner.width, 1);
            frame.render_widget(Paragraph::new(format!("{}. {place} — {}", index + 1, comment.text)), rect);
            hits.push((rect, Action::EditComment(index)));
        }
    }
}

// ---------------------------------------------------------------------------
// the terminal
// ---------------------------------------------------------------------------

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), DisableMouseCapture, crossterm::event::DisableBracketedPaste, LeaveAlternateScreen);
}

fn run(app: &mut App, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> std::io::Result<()> {
    let mut inbox = Service;
    app.refresh(&mut inbox);
    let mut last = Instant::now();
    while !app.done {
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(200))? {
            app.on_event(event::read()?, &mut inbox);
        }
        if last.elapsed() >= REFRESH {
            app.refresh(&mut inbox);
            last = Instant::now();
        }
    }
    Ok(())
}

const USAGE: &str = "usage: senpai tui [-h] [--item ID]\n";

/// `senpai tui [--item ID]`.
pub fn main(argv: &[PyStr]) -> i32 {
    let args: Vec<String> = argv.iter().map(PyStr::display).collect();
    let only = match args.as_slice() {
        [] => None,
        [flag] if flag == "-h" || flag == "--help" => {
            print!("{USAGE}\nReview inbox items in the terminal, with keys or the mouse.\n");
            return crate::EXIT_OK;
        }
        [flag, id] if flag == "--item" => match id.parse::<i128>() {
            Ok(id) => Some(id),
            Err(_) => {
                eprintln!("{USAGE}senpai tui: error: --item needs a number");
                return crate::EXIT_ERROR;
            }
        },
        _ => {
            eprintln!("{USAGE}senpai tui: error: unrecognized arguments");
            return crate::EXIT_ERROR;
        }
    };
    if enable_raw_mode().is_err() {
        eprintln!("senpai tui: needs an interactive terminal");
        return crate::EXIT_ERROR;
    }
    let _restore = Restore;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
    let mut stdout = std::io::stdout();
    if execute!(stdout, EnterAlternateScreen, EnableMouseCapture, crossterm::event::EnableBracketedPaste).is_err() {
        return crate::EXIT_ERROR;
    }
    let mut terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(terminal) => terminal,
        Err(_) => return crate::EXIT_ERROR,
    };
    let mut app = App::new(only);
    match run(&mut app, &mut terminal) {
        Ok(()) => crate::EXIT_OK,
        Err(error) => {
            drop(_restore);
            eprintln!("senpai tui: {error}");
            crate::EXIT_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;
    use ratatui::backend::TestBackend;

    struct Fake {
        items: Vec<Item>,
        payloads: Vec<(i128, Object)>,
        decided: Vec<(i128, Value)>,
        refuse: Option<String>,
    }

    impl Inbox for Fake {
        fn items(&mut self) -> Result<Vec<Item>, String> {
            Ok(self.items.clone())
        }
        fn payload(&mut self, id: i128) -> Result<Object, String> {
            self.payloads.iter().find(|(i, _)| *i == id).map(|(_, p)| p.clone()).ok_or_else(|| "gone".into())
        }
        fn decide(&mut self, id: i128, key: &str, decision: Value) -> Result<(), String> {
            assert_eq!(key, format!("key-{id}"), "decisions carry the shown item's key");
            if let Some(reason) = &self.refuse {
                return Err(reason.clone());
            }
            self.decided.push((id, decision));
            self.items.retain(|i| i.id != id);
            Ok(())
        }
    }

    fn object(text: &str) -> Object {
        json::loads_str(text).unwrap().as_object().unwrap().clone()
    }

    fn diff_inbox() -> Fake {
        let payload = object(
            r#"{"kind":"diff","title":"ship it","key":"key-7","payload":{"guide":null,"repositories":[{"root":"/r","files":[
              {"repository":"/r","path":"a.txt","old_path":null,"status":"modified","binary":false,"added":1,"removed":1,"lines":[
                {"type":"meta","old_line":null,"new_line":null,"text":"@@ -1,2 +1,2 @@"},
                {"type":"context","old_line":1,"new_line":1,"text":"one"},
                {"type":"remove","old_line":2,"new_line":null,"text":"two"},
                {"type":"add","old_line":null,"new_line":2,"text":"TWO"}]},
              {"repository":"/r","path":"b.txt","old_path":null,"status":"added","binary":false,"added":1,"removed":0,"lines":[
                {"type":"meta","old_line":null,"new_line":null,"text":"@@ -0,0 +1 @@"},
                {"type":"add","old_line":null,"new_line":1,"text":"new"}]}]}]}}"#,
        );
        Fake {
            items: vec![Item { id: 7, kind: "diff".into(), title: "ship it".into(), places: vec!["/r".into()], age: 90 }],
            payloads: vec![(7, payload)],
            decided: Vec::new(),
            refuse: None,
        }
    }

    fn app(only: Option<i128>) -> App {
        let mut app = App::new(only);
        app.guard = Duration::ZERO;
        app
    }

    fn confirm(app: &mut App, inbox: &mut Fake) {
        app.on_event(key(KeyCode::Tab), inbox);
        app.on_event(key(KeyCode::Enter), inbox);
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn keys(app: &mut App, inbox: &mut Fake, text: &str) {
        for c in text.chars() {
            app.on_event(key(KeyCode::Char(c)), inbox);
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE })
    }

    fn render(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn find(screen: &str, needle: &str) -> (u16, u16) {
        for (y, line) in screen.lines().enumerate() {
            if let Some(x) = line.find(needle) {
                return (line[..x].chars().count() as u16, y as u16);
            }
        }
        panic!("{needle:?} not on screen:\n{screen}");
    }

    #[test]
    fn item_mode_opens_the_item_and_approves_after_confirmation() {
        let mut inbox = diff_inbox();
        let mut app = app(Some(7));
        app.refresh(&mut inbox);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let screen = render(&mut app, &mut terminal);
        assert!(screen.contains("ship it") && screen.contains("modified a.txt") && screen.contains("TWO"), "{screen}");
        keys(&mut app, &mut inbox, "a");
        assert!(inbox.decided.is_empty(), "approve waits for confirmation");
        assert!(render(&mut app, &mut terminal).contains("Approve?"));
        app.on_event(key(KeyCode::Enter), &mut inbox);
        assert!(inbox.decided.is_empty(), "Enter on the default button cancels");
        keys(&mut app, &mut inbox, "a");
        confirm(&mut app, &mut inbox);
        assert_eq!(inbox.decided.len(), 1);
        assert_eq!(json::dumps(&inbox.decided[0].1, json::COMPACT), r#"{"decision":"approve","annotations":[]}"#);
        assert!(app.done);
    }

    #[test]
    fn selected_lines_become_a_line_comment() {
        let mut inbox = diff_inbox();
        let mut app = app(Some(7));
        app.refresh(&mut inbox);
        keys(&mut app, &mut inbox, "r");
        assert!(app.status.contains("needs at least one comment"));
        // The cursor starts on the first commentable row ("one"); select through "two".
        keys(&mut app, &mut inbox, "vjc");
        assert_eq!(app.status, "Select lines from one side of one file.");
        app.on_event(key(KeyCode::Esc), &mut inbox);
        keys(&mut app, &mut inbox, "jc");
        keys(&mut app, &mut inbox, "rename it");
        app.on_event(key(KeyCode::Enter), &mut inbox);
        keys(&mut app, &mut inbox, "r");
        confirm(&mut app, &mut inbox);
        let sent = json::dumps(&inbox.decided[0].1, json::COMPACT);
        assert_eq!(
            sent,
            r#"{"decision":"request-changes","annotations":[{"text":"rename it","target":{"scope":"line","repository":"/r","path":"a.txt","side":"new","start_line":2,"end_line":2}}]}"#
        );
    }

    #[test]
    fn mouse_selects_lines_comments_and_decides() {
        let mut inbox = diff_inbox();
        let mut app = app(Some(7));
        app.refresh(&mut inbox);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let screen = render(&mut app, &mut terminal);
        let (_, one) = find(&screen, "one");
        let (x, two) = find(&screen, "two");
        app.on_event(mouse(MouseEventKind::Down(MouseButton::Left), x, one), &mut inbox);
        app.on_event(mouse(MouseEventKind::Drag(MouseButton::Left), x, two), &mut inbox);
        app.on_event(mouse(MouseEventKind::Up(MouseButton::Left), x, two), &mut inbox);
        let screen = render(&mut app, &mut terminal);
        let (cx, cy) = find(&screen, " Comment ");
        app.on_event(mouse(MouseEventKind::Down(MouseButton::Left), cx + 1, cy), &mut inbox);
        assert_eq!(app.status, "Select lines from one side of one file.", "context and removed lines mix sides");
        app.on_event(mouse(MouseEventKind::Down(MouseButton::Left), x, two), &mut inbox);
        let screen = render(&mut app, &mut terminal);
        let (cx, cy) = find(&screen, " Comment ");
        app.on_event(mouse(MouseEventKind::Down(MouseButton::Left), cx + 1, cy), &mut inbox);
        app.on_event(Event::Paste("keep\nthis".into()), &mut inbox);
        app.on_event(key(KeyCode::Enter), &mut inbox);
        let screen = render(&mut app, &mut terminal);
        assert!(screen.contains('●'), "commented line is marked:\n{screen}");
        let (ax, ay) = find(&screen, " Request changes ");
        app.on_event(mouse(MouseEventKind::Down(MouseButton::Left), ax + 1, ay), &mut inbox);
        let screen = render(&mut app, &mut terminal);
        let (sx, sy) = find(&screen, " Send ");
        app.on_event(mouse(MouseEventKind::Down(MouseButton::Left), sx + 1, sy), &mut inbox);
        let sent = json::dumps(&inbox.decided[0].1, json::COMPACT);
        assert!(sent.contains(r#""text":"keep this""#) && sent.contains(r#""side":"old","start_line":2,"end_line":2"#), "{sent}");
    }

    #[test]
    fn wheel_scrolls_and_file_jumps_work() {
        let mut inbox = diff_inbox();
        let mut app = app(Some(7));
        app.refresh(&mut inbox);
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        render(&mut app, &mut terminal);
        app.on_event(mouse(MouseEventKind::ScrollDown, 10, 8), &mut inbox);
        let Screen::Review(review) = &app.screen else { panic!() };
        assert_eq!(review.scroll, SCROLL_STEP);
        keys(&mut app, &mut inbox, "gJ");
        let Screen::Review(review) = &app.screen else { panic!() };
        assert!(review.rows[review.cursor].text.contains("b.txt"));
    }

    #[test]
    fn comments_can_be_edited_and_deleted() {
        let mut inbox = diff_inbox();
        let mut app = app(Some(7));
        app.refresh(&mut inbox);
        keys(&mut app, &mut inbox, "C");
        keys(&mut app, &mut inbox, "overall");
        app.on_event(key(KeyCode::Enter), &mut inbox);
        keys(&mut app, &mut inbox, "l1");
        app.on_event(Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)), &mut inbox);
        app.on_event(key(KeyCode::Enter), &mut inbox);
        assert_eq!(app.status, "Comment deleted.");
        let Screen::Review(review) = &app.screen else { panic!() };
        assert!(review.comments.is_empty());
    }

    #[test]
    fn refusal_from_the_service_is_shown_and_nothing_closes() {
        let mut inbox = diff_inbox();
        inbox.refuse = Some("this review is no longer waiting for a decision".into());
        let mut app = app(Some(7));
        app.refresh(&mut inbox);
        keys(&mut app, &mut inbox, "a");
        confirm(&mut app, &mut inbox);
        assert_eq!(app.status, "this review is no longer waiting for a decision");
        assert!(!app.done);
    }

    #[test]
    fn item_decided_elsewhere_ends_item_mode() {
        let mut inbox = diff_inbox();
        let mut app = app(Some(7));
        app.refresh(&mut inbox);
        inbox.items.clear();
        app.refresh(&mut inbox);
        assert!(app.done);
        assert!(app.status.contains("no longer waiting"));
    }

    #[test]
    fn document_lines_comment_without_repository_or_side() {
        let payload = object(
            r##"{"kind":"document","title":"plan ok?","key":"key-2","payload":{"path":"/p/plan.md",
              "rows":[{"n":1,"kind":"heading","text":"# Plan"},{"n":2,"kind":"blank","text":""},{"n":3,"kind":"text","text":"Do it."}],
              "blocks":[{"start":1,"end":1},{"start":3,"end":3}]}}"##,
        );
        let mut inbox = Fake {
            items: vec![Item { id: 2, kind: "document".into(), title: "plan ok?".into(), places: vec![], age: 0 }],
            payloads: vec![(2, payload)],
            decided: Vec::new(),
            refuse: None,
        };
        let mut app = app(Some(2));
        app.refresh(&mut inbox);
        keys(&mut app, &mut inbox, "jjc");
        keys(&mut app, &mut inbox, "which?");
        app.on_event(key(KeyCode::Enter), &mut inbox);
        keys(&mut app, &mut inbox, "r");
        confirm(&mut app, &mut inbox);
        assert_eq!(
            json::dumps(&inbox.decided[0].1, json::COMPACT),
            r#"{"decision":"request-changes","annotations":[{"text":"which?","target":{"scope":"line","path":"/p/plan.md","start_line":3,"end_line":3}}]}"#
        );
    }

    #[test]
    fn inbox_list_opens_by_click_and_questions_are_answered() {
        let question = object(r#"{"kind":"question","title":"Which port?","key":"key-4","options":["8080","9090"]}"#);
        let mut inbox = Fake {
            items: vec![Item { id: 4, kind: "question".into(), title: "Which port?".into(), places: vec![], age: 0 }],
            payloads: vec![(4, question)],
            decided: Vec::new(),
            refuse: None,
        };
        let mut app = app(None);
        app.refresh(&mut inbox);
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let screen = render(&mut app, &mut terminal);
        let (x, y) = find(&screen, "Which port?");
        app.on_event(mouse(MouseEventKind::Down(MouseButton::Left), x, y), &mut inbox);
        let screen = render(&mut app, &mut terminal);
        let (ox, oy) = find(&screen, "2. 9090");
        app.on_event(mouse(MouseEventKind::Down(MouseButton::Left), ox, oy), &mut inbox);
        keys(&mut app, &mut inbox, "free");
        app.on_event(key(KeyCode::Enter), &mut inbox);
        assert_eq!(json::dumps(&inbox.decided[0].1, json::COMPACT), r#"{"option":"9090","text":"free"}"#);
        assert!(!app.done, "list mode stays open");
        assert_eq!(app.status, "Answer sent.");
    }

    #[test]
    fn help_and_wrapping() {
        assert_eq!(wrap("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!(wrap("界界界", 4), vec!["界界", "界"]);
        let mut inbox = diff_inbox();
        let mut app = app(None);
        app.refresh(&mut inbox);
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        keys(&mut app, &mut inbox, "?");
        assert!(render(&mut app, &mut terminal).contains("every decision asks for confirmation"));
        keys(&mut app, &mut inbox, "x");
        assert!(!render(&mut app, &mut terminal).contains("every decision asks"));
    }

    #[test]
    fn typing_that_runs_into_command_mode_never_approves() {
        let mut inbox = diff_inbox();
        let mut app = app(Some(7));
        app.refresh(&mut inbox);
        // A review comment, then Enter, then more typing meant for the comment.
        keys(&mut app, &mut inbox, "C");
        keys(&mut app, &mut inbox, "This breaks X.");
        app.on_event(key(KeyCode::Enter), &mut inbox);
        keys(&mut app, &mut inbox, "Maybe explain why, okay yes");
        app.on_event(key(KeyCode::Enter), &mut inbox);
        // `c` on a header row only shows a message; the typing goes on.
        keys(&mut app, &mut inbox, "gc");
        keys(&mut app, &mut inbox, "looks okay, approve? y yes");
        app.on_event(key(KeyCode::Enter), &mut inbox);
        assert!(inbox.decided.is_empty(), "{:?}", inbox.decided);
    }

    #[test]
    fn a_new_dialog_ignores_input_for_a_moment() {
        let mut inbox = diff_inbox();
        let mut app = App::new(Some(7));
        app.refresh(&mut inbox);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let screen = render(&mut app, &mut terminal);
        let (ax, ay) = find(&screen, " Approve ");
        app.on_event(mouse(MouseEventKind::Down(MouseButton::Left), ax + 1, ay), &mut inbox);
        let screen = render(&mut app, &mut terminal);
        let (sx, sy) = find(&screen, " Send ");
        app.on_event(mouse(MouseEventKind::Down(MouseButton::Left), sx + 1, sy), &mut inbox);
        confirm(&mut app, &mut inbox);
        assert!(inbox.decided.is_empty(), "clicks and keys right after opening are ignored");
        std::thread::sleep(CONFIRM_GUARD);
        app.on_event(mouse(MouseEventKind::Down(MouseButton::Left), sx + 1, sy), &mut inbox);
        assert_eq!(inbox.decided.len(), 1);
    }

    #[test]
    fn control_characters_in_the_change_are_shown_not_obeyed() {
        let mut inbox = diff_inbox();
        let payload = &mut inbox.payloads[0].1;
        let mut text = json::dumps(&Value::Object(payload.clone()), json::COMPACT);
        text = text.replace(r#""text":"TWO""#, r#""text":"a\u001b[2Jb\rc\bd\u009b\u007f""#);
        text = text.replace(r#""title":"ship it""#, r#""title":"ship\u001b]52;c;eA==\u0007 it""#);
        *payload = object(&text);
        let mut app = app(Some(7));
        app.refresh(&mut inbox);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        render(&mut app, &mut terminal);
        let buffer = terminal.backend().buffer();
        for cell in buffer.content() {
            assert!(!cell.symbol().chars().any(char::is_control), "control character on screen: {:?}", cell.symbol());
        }
        let screen = render(&mut app, &mut terminal);
        assert!(screen.contains("a␛[2Jb␍c␈d"), "{screen}");
    }

    #[test]
    fn small_terminals_never_panic() {
        for (width, height) in [(1, 1), (20, 4), (39, 9), (40, 10), (45, 11), (80, 11), (80, 12)] {
            for keys_pressed in ["", "l", "a", "C", "?"] {
                let mut inbox = diff_inbox();
                let mut app = app(Some(7));
                app.refresh(&mut inbox);
                keys(&mut app, &mut inbox, keys_pressed);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                render(&mut app, &mut terminal);
                app.on_event(mouse(MouseEventKind::Down(MouseButton::Left), width / 2, height / 2), &mut inbox);
                render(&mut app, &mut terminal);
            }
        }
    }

    #[test]
    fn wheel_up_keeps_the_view_where_it_was_scrolled() {
        let mut inbox = diff_inbox();
        let mut app = app(Some(7));
        app.refresh(&mut inbox);
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        keys(&mut app, &mut inbox, "G");
        render(&mut app, &mut terminal);
        let before = match &app.screen { Screen::Review(r) => r.scroll, _ => unreachable!() };
        app.on_event(mouse(MouseEventKind::ScrollUp, 10, 8), &mut inbox);
        render(&mut app, &mut terminal);
        let Screen::Review(review) = &app.screen else { panic!() };
        assert!(review.scroll < before || before == 0, "scrolled up from {before} to {}", review.scroll);
    }
}
