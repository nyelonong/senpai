//! The on-demand inbox: one local service that shows every pending review and
//! question from every agent session on one browser page.
//!
//! The service only queues, shows and relays. The reviewer process still
//! builds the payload, and the client still fingerprints, rechecks and writes
//! the ledger, so a service that misbehaves or dies can never approve
//! anything: its clients see the connection close and fail with exit 2.
//!
//! Messages are single lines of JSON over a Unix socket in a private
//! directory. A client that disconnects withdraws its item.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::diag;
use crate::error::{os_error, Error, Result};
use crate::html::{self, Prepared, Session};
use crate::json::{self, Object, Value};
use crate::os;
use crate::pystr::{py_strip, PyStr};
use crate::result::ReviewResult;

const HOME_ENV: &str = "SENPAI_HOME";
const INBOX_ENV: &str = "SENPAI_INBOX";
const IDLE_ENV: &str = "SENPAI_SERVE_IDLE";
const NO_BROWSER_ENV: &str = "HTML_REVIEW_NO_BROWSER";
const URL_FILE_ENV: &str = "HTML_REVIEW_URL_FILE";
const SURFACE_ENV: &str = "SENPAI_SURFACE";
const IDLE_SECONDS: f64 = 600.0;
const START_WAIT: Duration = Duration::from_secs(10);
const TAB_ALIVE: Duration = Duration::from_secs(8);
const MAX_MESSAGE_BYTES: u64 = 256 << 20;
const MAX_ANSWER_BYTES: usize = 1 << 20;
/// `sockaddr_un.sun_path` holds 104 bytes on macOS, including the final NUL.
const MAX_SOCKET_PATH: usize = 104;
const INBOX_PAGE: &str = include_str!("../assets/inbox.html");

/// Reviews go through the inbox unless `SENPAI_INBOX` turns it off.
pub fn enabled() -> bool {
    let value = std::env::var(INBOX_ENV).unwrap_or_default();
    !matches!(py_strip(&value).to_lowercase().as_str(), "0" | "false" | "no" | "off")
}

// ---------------------------------------------------------------------------
// files
// ---------------------------------------------------------------------------

fn home() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    let user_home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::Other("HOME is not set; set SENPAI_HOME".into()))?;
    let user_home = PathBuf::from(user_home);
    if cfg!(target_os = "macos") {
        return Ok(user_home.join("Library/Application Support/senpai"));
    }
    let state = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
    Ok(state.unwrap_or_else(|| user_home.join(".local/state")).join("senpai"))
}

/// The service directory, private to this user.
fn ensure_home() -> Result<PathBuf> {
    let dir = home()?;
    let failure = |e: std::io::Error| Error::Other(format!("unable to prepare {}: {e}", dir.display()));
    std::fs::create_dir_all(&dir).map_err(failure)?;
    let metadata = std::fs::metadata(&dir).map_err(failure)?;
    // SAFETY: geteuid has no preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(Error::Other(format!("{} belongs to another user", dir.display())));
    }
    if metadata.mode() & 0o077 != 0 {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(failure)?;
    }
    Ok(dir)
}

fn socket_path() -> Result<PathBuf> {
    Ok(home()?.join("senpai.sock"))
}

/// Identifies the installed binary, so a client never talks to a service
/// built from different code.
fn build_id() -> String {
    let stamp = std::env::current_exe()
        .and_then(std::fs::metadata)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| format!("{}.{:09}", d.as_secs(), d.subsec_nanos()))
        .unwrap_or_default();
    format!("{}+{stamp}", env!("CARGO_PKG_VERSION"))
}

// ---------------------------------------------------------------------------
// messages
// ---------------------------------------------------------------------------

fn read_message(reader: &mut impl BufRead) -> std::io::Result<Option<Value>> {
    let mut line = Vec::new();
    reader.take(MAX_MESSAGE_BYTES).read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Ok(None);
    }
    if !line.ends_with(b"\n") {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "incomplete message"));
    }
    json::loads(&PyStr::decode_surrogateescape(&line))
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.0))
}

fn write_message(stream: &mut impl Write, value: &Value) -> std::io::Result<()> {
    let mut line = json::dumps(value, json::COMPACT);
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

fn text_field(object: &Object, key: &str) -> Option<PyStr> {
    object.get(key).and_then(Value::as_pystr).cloned()
}

// ---------------------------------------------------------------------------
// client side
// ---------------------------------------------------------------------------

fn spawn_service(dir: &std::path::Path) -> Result<()> {
    let exe = std::env::current_exe().map_err(|e| Error::Launcher(format!("unable to locate senpai: {e}")))?;
    let log_path = dir.join("serve.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|e| Error::Launcher(format!("unable to open {}: {e}", log_path.display())))?;
    let log_err = log.try_clone().map_err(|e| Error::Launcher(e.to_string()))?;
    let mut command = Command::new(exe);
    command.arg("serve").current_dir("/").stdin(Stdio::null()).stdout(log).stderr(log_err);
    // SAFETY: setsid is async-signal-safe and touches no Rust state. The new
    // session keeps the service out of the reviewer's process group, which the
    // client kills when the review ends.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|e| Error::Launcher(format!("unable to start the senpai service: {e}")))?;
    thread::spawn(move || child.wait());
    Ok(())
}

fn peer_uid(stream: &UnixStream) -> std::io::Result<libc::uid_t> {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "linux")]
    {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: getsockopt writes at most `size` bytes into `credentials`.
        let status = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut size,
            )
        };
        if status != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(credentials.uid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (0, 0);
        // SAFETY: getpeereid only writes the two out-parameters.
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(uid)
    }
}

fn same_user(stream: &UnixStream) -> bool {
    // SAFETY: geteuid has no preconditions.
    peer_uid(stream).is_ok_and(|uid| uid == unsafe { libc::geteuid() })
}

/// Connects only to a service run by this user, whatever owns the socket path.
fn connect(socket: &std::path::Path) -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(socket)?;
    if !same_user(&stream) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the inbox socket is served by another user",
        ));
    }
    Ok(stream)
}

/// Connects to the service, starting it first when none is running. A lock
/// file makes concurrent clients start at most one service.
fn connect_or_start() -> Result<UnixStream> {
    let dir = ensure_home()?;
    let socket = dir.join("senpai.sock");
    if socket.as_os_str().len() >= MAX_SOCKET_PATH {
        return Err(Error::Launcher(format!(
            "the inbox socket path is too long for this system: {}; set {HOME_ENV} to a shorter directory",
            socket.display()
        )));
    }
    match connect(&socket) {
        Ok(stream) => return Ok(stream),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(Error::Launcher(format!("{error}: {}", socket.display())))
        }
        Err(_) => {}
    }
    let lock_path = dir.join("serve.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| Error::Launcher(format!("unable to open {}: {e}", lock_path.display())))?;
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a descriptor this function owns; the lock is released
    // when `lock` is closed on return.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(Error::Launcher(format!("unable to lock {}", lock_path.display())));
    }
    if let Ok(stream) = connect(&socket) {
        return Ok(stream);
    }
    spawn_service(&dir)?;
    let deadline = Instant::now() + START_WAIT;
    loop {
        match connect(&socket) {
            Ok(stream) => return Ok(stream),
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                return Err(Error::Launcher(format!(
                    "the senpai service did not start ({}); see {}",
                    os_error(&error, None),
                    dir.join("serve.log").display()
                )))
            }
        }
    }
}

fn wait_for_exit() {
    let Ok(socket) = socket_path() else { return };
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && UnixStream::connect(&socket).is_ok() {
        thread::sleep(Duration::from_millis(50));
    }
}

/// Sends one request and returns its first reply with the open connection.
/// A service that closes before replying showed nothing, so it is safe to
/// retry; a service from another build restarts once it is idle.
fn exchange(message: Object) -> Result<(BufReader<UnixStream>, Object)> {
    let message = Value::Object(message.with("build", build_id()));
    let mut last = String::from("the senpai service closed the connection before accepting the request");
    for _ in 0..3 {
        let stream = connect_or_start()?;
        let mut writer = stream.try_clone().map_err(|e| Error::Launcher(e.to_string()))?;
        if let Err(error) = write_message(&mut writer, &message) {
            last = format!("unable to reach the senpai service: {}", os_error(&error, None));
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        let mut reader = BufReader::new(stream);
        match read_message(&mut reader) {
            Ok(Some(Value::Object(reply))) => {
                match reply.get("error").and_then(Value::as_pystr) {
                    Some(code) if code == "restart" => {
                        wait_for_exit();
                        continue;
                    }
                    Some(text) => return Err(Error::Launcher(text.display())),
                    None => return Ok((reader, reply)),
                }
            }
            Ok(Some(_)) => return Err(Error::Launcher("the senpai service sent a malformed reply".into())),
            Ok(None) | Err(_) => thread::sleep(Duration::from_millis(200)),
        }
    }
    Err(Error::Launcher(last))
}

/// One request that needs no further reply, such as the terminal UI's.
pub fn request(message: Object) -> Result<Object> {
    exchange(message).map(|(_, reply)| reply)
}

/// Where a new item is brought to the person's attention.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Surface {
    Browser,
    Rex,
    Quiet,
}

fn rex_target() -> Option<(String, String)> {
    let get = |name| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    Some((get("REX_SESSION")?, get("REX_BLOCK")?))
}

/// `SENPAI_SURFACE`: `auto` (a Rex split inside Rex, else the browser),
/// `browser`, `rex` or `none`. `HTML_REVIEW_NO_BROWSER` still turns the
/// browser off.
fn surface(browser: bool) -> Surface {
    let value = std::env::var(SURFACE_ENV).unwrap_or_default();
    let browser_or_quiet = if browser { Surface::Browser } else { Surface::Quiet };
    match py_strip(&value).to_lowercase().as_str() {
        "browser" => browser_or_quiet,
        "rex" => Surface::Rex,
        "none" => Surface::Quiet,
        _ if rex_target().is_some() => Surface::Rex,
        _ => browser_or_quiet,
    }
}

/// Opens `senpai tui --item ID` in a split beside the agent's Rex pane.
/// Failure is reported, never fatal: the item still waits in the inbox.
fn open_rex_split(id: &Value) -> std::result::Result<(), String> {
    let (session, block) = rex_target().ok_or("REX_SESSION and REX_BLOCK are not set")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let id = match id {
        Value::Int(id) => id.to_string(),
        _ => return Err("the service did not name the item".into()),
    };
    let mut command = Command::new("rex");
    command.args([
        "split", "-s", &session, "-b", &block, "--split=right", "--focus=false", "--label", "senpai", "--shell=none", "--",
    ]);
    // The split runs with Rex's environment, not this process's, so the
    // service location is passed on explicitly.
    if let Some(home) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
        let mut assignment = std::ffi::OsString::from(format!("{HOME_ENV}="));
        assignment.push(home);
        command.arg("/usr/bin/env").arg(assignment);
    }
    let mut child = command
        .arg(exe)
        .args(["tui", "--item", &id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("unable to run rex: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("rex split exited with {status}")),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("rex split did not finish".into());
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// Shows a new item where `surface` says; a failed Rex split falls back to
/// the browser when it is allowed.
fn present(surface: Surface, browser: bool, id: &Value, inbox: &str, producer: &str) {
    if surface != Surface::Rex {
        return;
    }
    if let Err(error) = open_rex_split(id) {
        diag::write(&format!("{producer}: unable to open the review in Rex ({error})\n"));
        if browser {
            html::open_browser(inbox);
        }
    }
}

fn url_file() -> Option<Vec<u8>> {
    std::env::var_os(URL_FILE_ENV).map(|v| os::path_bytes(std::path::Path::new(&v)))
}

/// Ends this reviewer when the client that started it is gone, even if it was
/// killed outright, so its connection closes and the inbox withdraws the item.
fn exit_with_parent() {
    // SAFETY: getppid has no preconditions.
    let parent = unsafe { libc::getppid() };
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(500));
        // SAFETY: getppid has no preconditions.
        if unsafe { libc::getppid() } != parent {
            std::process::exit(crate::EXIT_ERROR);
        }
    });
}

/// Puts a prepared review in the inbox and waits for the person's decision.
/// The page limits apply as on a one-off page: past `lifetime`, or past `idle`
/// without the page ever being opened, the review is cancelled.
pub fn submit_review(prepared: &Prepared, browser: bool, lifetime: f64, idle: f64) -> Result<ReviewResult> {
    let message = Object::new()
        .with("op", "submit")
        .with("mode", prepared.mode)
        .with("review_id", &prepared.review_id)
        .with("document", prepared.document.clone())
        .with("title", &prepared.title)
        .with("places", Value::Array(prepared.places.iter().map(Value::from).collect()))
        .with("payload", prepared.payload.clone())
        .with("browser", surface(browser) == Surface::Browser)
        .with("lifetime", Value::Float(lifetime))
        .with("idle", Value::Float(idle));
    exit_with_parent();
    let (mut reader, reply) = exchange(message)?;
    let url = text_field(&reply, "url").map(|u| u.display()).unwrap_or_default();
    let inbox = text_field(&reply, "inbox").map(|u| u.display()).unwrap_or_default();
    diag::write(&format!("{}: review page {url}\n", html::PRODUCER));
    diag::write(&format!("{}: inbox {inbox}\n", html::PRODUCER));
    let url_file = url_file();
    html::publish_url(url_file.as_deref(), &url);
    present(surface(browser), browser, reply.get("id").unwrap_or(&Value::Null), &inbox, html::PRODUCER);
    let message = read_message(&mut reader);
    html::withdraw_url(url_file.as_deref());
    let stopped = || Error::Other("the senpai service stopped before a decision; no approval was returned".into());
    let Ok(Some(Value::Object(reply))) = message else {
        return Err(stopped());
    };
    let Some(result) = reply.get("result").and_then(Value::as_object) else {
        return Err(stopped());
    };
    let decision = text_field(result, "decision").map(|d| d.display()).ok_or_else(stopped)?;
    let annotations = result.get("annotations").and_then(Value::as_array).cloned().ok_or_else(stopped)?;
    Ok(ReviewResult { decision, annotations, review_id: prepared.review_id.clone() })
}

// ---------------------------------------------------------------------------
// the service
// ---------------------------------------------------------------------------

struct Ask {
    options: Vec<PyStr>,
    answer: Mutex<Option<Value>>,
    answered: Condvar,
}

impl Ask {
    fn settle(&self, value: Value) -> bool {
        let mut answer = self.answer.lock().expect("ask state");
        if answer.is_some() {
            return false;
        }
        *answer = Some(value);
        self.answered.notify_all();
        true
    }

    fn wait(&self) -> Value {
        let mut answer = self.answer.lock().expect("ask state");
        while answer.is_none() {
            answer = self.answered.wait(answer).expect("ask state");
        }
        answer.clone().expect("answered")
    }
}

enum Found {
    Review(Arc<Session>, &'static str, PyStr, String),
    Question(Arc<Ask>, PyStr, String),
}

enum Kind {
    Review { session: Arc<Session>, mode: &'static str },
    Question(Arc<Ask>),
}

struct Item {
    id: u64,
    /// A random secret per item. Ids restart with the service; the key does
    /// not, so a decision from the terminal reaches only the item it showed.
    key: String,
    title: PyStr,
    places: Vec<PyStr>,
    created: Instant,
    kind: Kind,
}

struct Inbox {
    items: Vec<Item>,
    connections: usize,
    last_activity: Instant,
    last_poll: Option<Instant>,
    next_id: u64,
}

struct Service {
    token: String,
    url: String,
    build: String,
    socket: PathBuf,
    socket_id: Option<(u64, u64)>,
    idle: Duration,
    inbox: Mutex<Inbox>,
}

fn file_id(path: &std::path::Path) -> Option<(u64, u64)> {
    std::fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

impl Service {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inbox> {
        self.inbox.lock().expect("inbox state")
    }

    /// Exits. Callers that decided the service is idle still hold the inbox
    /// lock, so no item can be accepted between that decision and the exit.
    /// The socket is removed only while it is still this service's own.
    fn shutdown(&self) -> ! {
        if self.socket_id.is_some() && file_id(&self.socket) == self.socket_id {
            let _ = std::fs::remove_file(&self.socket);
        }
        diag::write("senpai serve: stopped\n");
        std::process::exit(crate::EXIT_OK);
    }

    fn add(&self, title: PyStr, places: Vec<PyStr>, kind: Kind) -> u64 {
        let mut inbox = self.lock();
        inbox.next_id += 1;
        let id = inbox.next_id;
        let key = match &kind {
            Kind::Review { session, .. } => session.token().to_string(),
            Kind::Question(_) => os::token_urlsafe(24),
        };
        inbox.items.push(Item { id, key, title, places, created: Instant::now(), kind });
        id
    }

    fn remove(&self, id: u64) {
        let mut inbox = self.lock();
        inbox.items.retain(|item| item.id != id);
        inbox.last_activity = Instant::now();
    }

    /// Brings a new item to the person's attention: a notification when the
    /// inbox is already open, otherwise the inbox itself.
    fn announce(&self, browser: bool, title: &PyStr) {
        if !browser {
            return;
        }
        let open = {
            let mut inbox = self.lock();
            let alive = inbox.last_poll.is_some_and(|t| t.elapsed() < TAB_ALIVE);
            if !alive {
                inbox.last_poll = Some(Instant::now());
            }
            !alive
        };
        if open {
            html::open_browser(&self.url);
        } else {
            notify(&title.display());
        }
    }

    /// The waiting items. Only the browser page counts as an open inbox tab.
    fn items_value(&self, from_page: bool) -> Value {
        let mut inbox = self.lock();
        if from_page {
            inbox.last_poll = Some(Instant::now());
        }
        let items: Vec<Value> = inbox
            .items
            .iter()
            .map(|item| {
                let base = Object::new()
                    .with("id", Value::Int(item.id as i128))
                    .with("title", &item.title)
                    .with("places", Value::Array(item.places.iter().map(Value::from).collect()))
                    .with("age", Value::Int(item.created.elapsed().as_secs() as i128));
                Value::Object(match &item.kind {
                    Kind::Review { session, mode } => {
                        base.with("kind", *mode).with("url", format!("/{}/", session.token()))
                    }
                    Kind::Question(ask) => base
                        .with("kind", "question")
                        .with("options", Value::Array(ask.options.iter().map(Value::from).collect())),
                })
            })
            .collect();
        Value::Array(items)
    }

    fn items_json(&self) -> Vec<u8> {
        let items = self.items_value(true);
        json::dumps(&Value::Object(Object::new().with("items", items)), json::COMPACT).into_bytes()
    }

    fn find(&self, id: i128) -> Option<Found> {
        self.lock().items.iter().find(|item| item.id as i128 == id).map(|item| match &item.kind {
            Kind::Review { session, mode } => Found::Review(Arc::clone(session), mode, item.title.clone(), item.key.clone()),
            Kind::Question(ask) => Found::Question(Arc::clone(ask), item.title.clone(), item.key.clone()),
        })
    }

    fn question(&self, id: i128) -> Option<Arc<Ask>> {
        self.lock().items.iter().find_map(|item| match &item.kind {
            Kind::Question(ask) if item.id as i128 == id => Some(Arc::clone(ask)),
            _ => None,
        })
    }

    fn session(&self, token: &str) -> Option<Arc<Session>> {
        self.lock().items.iter().find_map(|item| match &item.kind {
            Kind::Review { session, .. } if html::constant_time_eq(token, session.token()) => Some(Arc::clone(session)),
            _ => None,
        })
    }
}

fn notify(message: &str) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let spawned = Command::new("osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            "senpai",
            message,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = spawned {
        thread::spawn(move || child.wait());
    }
}

/// Withdraws an item when its client goes away: the client sends nothing
/// after its request, so the next read only returns at disconnect.
fn on_disconnect(mut reader: BufReader<UnixStream>, withdraw: impl FnOnce() + Send + 'static) {
    thread::spawn(move || {
        let mut buffer = [0u8; 512];
        while matches!(reader.read(&mut buffer), Ok(n) if n > 0) {}
        withdraw();
    });
}

fn seconds(message: &Object, key: &str) -> Option<Duration> {
    let value = match message.get(key) {
        Some(Value::Float(f)) => *f,
        Some(Value::Int(i)) => *i as f64,
        _ => return None,
    };
    Duration::try_from_secs_f64(value).ok().filter(|d| !d.is_zero())
}

/// Cancels a review that outlives its lifetime, or whose page was never
/// opened within the idle limit, exactly as a one-off page does.
fn expire(session: Arc<Session>, lifetime: Option<Duration>, idle: Option<Duration>) {
    let started = Instant::now();
    thread::spawn(move || {
        while !session.settled() {
            thread::sleep(Duration::from_millis(250));
            let elapsed = started.elapsed();
            if lifetime.is_some_and(|l| elapsed >= l) || idle.is_some_and(|i| elapsed >= i && !session.opened()) {
                session.stop();
            }
        }
    });
}

fn serve_connection(service: &Arc<Service>, stream: UnixStream) {
    let Ok(read_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    let Ok(Some(Value::Object(message))) = read_message(&mut reader) else { return };
    let reply = |writer: &mut UnixStream, value: Object| write_message(writer, &Value::Object(value));
    let op = text_field(&message, "op").map(|o| o.display()).unwrap_or_default();
    if op == "stop" {
        let _ = reply(&mut writer, Object::new().with("stopped", true));
        service.shutdown();
    }
    if !matches!(text_field(&message, "build"), Some(b) if b == service.build.as_str()) {
        let inbox = service.lock();
        if inbox.items.is_empty() && inbox.connections == 1 {
            let _ = reply(&mut writer, Object::new().with("error", "restart"));
            service.shutdown();
        }
        let count = inbox.items.len();
        drop(inbox);
        let text = format!(
            "a senpai service from another build is running with {count} pending item(s); \
             finish them or run `senpai serve --stop`"
        );
        let _ = reply(&mut writer, Object::new().with("error", text));
        return;
    }
    let title = text_field(&message, "title").unwrap_or_default();
    let browser = !matches!(message.get("browser"), Some(Value::Bool(false)));
    match op.as_str() {
        "status" => {
            let pending = service.lock().items.len();
            let _ = reply(
                &mut writer,
                Object::new().with("inbox", service.url.as_str()).with("pending", pending).with("build", service.build.as_str()),
            );
            if browser {
                html::open_browser(&service.url);
            }
        }
        "submit" => {
            let mode = match text_field(&message, "mode") {
                Some(m) if m == "diff" => "diff",
                Some(m) if m == "document" => "document",
                _ => {
                    let _ = reply(&mut writer, Object::new().with("error", "unknown review mode"));
                    return;
                }
            };
            let (Some(review_id), Some(payload)) = (text_field(&message, "review_id"), message.get("payload")) else {
                let _ = reply(&mut writer, Object::new().with("error", "malformed review submission"));
                return;
            };
            let places: Vec<PyStr> = message
                .get("places")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_pystr).cloned().collect())
                .unwrap_or_default();
            let prepared = Prepared {
                title: title.clone(),
                places: places.clone(),
                review_id,
                document: text_field(&message, "document"),
                payload: payload.clone(),
                mode,
            };
            let session = Arc::new(Session::new(&prepared, os::token_urlsafe(24), None));
            let item_url = format!("{}{}/", service.url.trim_end_matches(&format!("{}/", service.token)), session.token());
            let id = service.add(title.clone(), places, Kind::Review { session: Arc::clone(&session), mode });
            let accepted = Object::new()
                .with("url", item_url)
                .with("inbox", service.url.as_str())
                .with("id", Value::Int(id as i128));
            if reply(&mut writer, accepted).is_err() {
                session.stop();
            }
            service.announce(browser, &title);
            let watched = Arc::clone(&session);
            on_disconnect(reader, move || watched.stop());
            expire(Arc::clone(&session), seconds(&message, "lifetime"), seconds(&message, "idle"));
            let result = session.wait();
            service.remove(id);
            let _ = reply(&mut writer, Object::new().with("result", result.to_value()));
        }
        "ask" => {
            if title.strip().is_empty() {
                let _ = reply(&mut writer, Object::new().with("error", "a question needs text"));
                return;
            }
            let options: Vec<PyStr> = message
                .get("options")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_pystr).cloned().collect())
                .unwrap_or_default();
            let ask = Arc::new(Ask { options, answer: Mutex::new(None), answered: Condvar::new() });
            let places = text_field(&message, "place").into_iter().collect();
            let id = service.add(title.clone(), places, Kind::Question(Arc::clone(&ask)));
            let accepted = Object::new()
                .with("inbox", service.url.as_str())
                .with("question_id", Value::Int(id as i128));
            if reply(&mut writer, accepted).is_err() {
                ask.settle(Value::Null);
            }
            service.announce(browser, &title);
            let watched = Arc::clone(&ask);
            on_disconnect(reader, move || {
                watched.settle(Value::Null);
            });
            let answer = ask.wait();
            service.remove(id);
            let _ = reply(&mut writer, Object::new().with("answer", answer));
        }
        "items" => {
            let _ = reply(&mut writer, Object::new().with("items", service.items_value(false)));
        }
        "payload" => {
            let id = message.get("id").and_then(Value::as_int).unwrap_or(-1);
            let answer = match service.find(id) {
                Some(Found::Review(session, mode, title, key)) => {
                    session.mark_opened();
                    let payload = std::str::from_utf8(session.payload_json())
                        .ok()
                        .and_then(|t| json::loads_str(t).ok())
                        .unwrap_or(Value::Null);
                    Object::new().with("kind", mode).with("title", title).with("key", key).with("payload", payload)
                }
                Some(Found::Question(ask, title, key)) => Object::new()
                    .with("kind", "question")
                    .with("key", key)
                    .with("title", title)
                    .with("options", Value::Array(ask.options.iter().map(Value::from).collect())),
                None => Object::new().with("error", "that item is no longer waiting"),
            };
            let _ = reply(&mut writer, answer);
        }
        "decide" => {
            let id = message.get("id").and_then(Value::as_int).unwrap_or(-1);
            let decision = message.get("decision").cloned().unwrap_or(Value::Null);
            let key = text_field(&message, "key").map(|k| k.display()).unwrap_or_default();
            let found = service.find(id).filter(|found| {
                let (Found::Review(.., expected) | Found::Question(.., expected)) = found;
                html::constant_time_eq(&key, expected)
            });
            let outcome = match found {
                Some(Found::Review(session, ..)) => session.decide_value(&decision),
                Some(Found::Question(..)) => {
                    let mut answer = decision.as_object().cloned().unwrap_or_default();
                    answer.insert("id", Value::Int(id));
                    parse_answer_value(&Value::Object(answer), service).and_then(|(ask, value)| {
                        if ask.settle(value) {
                            Ok(())
                        } else {
                            Err("that question is no longer waiting".into())
                        }
                    })
                }
                None => Err("that item is no longer waiting".into()),
            };
            let _ = reply(
                &mut writer,
                match outcome {
                    Ok(()) => Object::new().with("decided", true),
                    Err(text) => Object::new().with("error", text),
                },
            );
        }
        _ => {
            let _ = reply(&mut writer, Object::new().with("error", "unknown request"));
        }
    }
}

fn parse_answer(body: &[u8], service: &Service) -> std::result::Result<(Arc<Ask>, Value), String> {
    let value = std::str::from_utf8(body)
        .ok()
        .and_then(|t| json::loads_str(t).ok())
        .ok_or("the answer is not valid JSON")?;
    parse_answer_value(&value, service)
}

fn parse_answer_value(value: &Value, service: &Service) -> std::result::Result<(Arc<Ask>, Value), String> {
    let object = value.as_object().ok_or("the answer must be a JSON object")?;
    let id = object.get("id").and_then(Value::as_int).ok_or("the answer needs the question id")?;
    let ask = service.question(id).ok_or("that question is no longer waiting")?;
    if matches!(object.get("dismissed"), Some(Value::Bool(true))) {
        return Ok((ask, Value::Object(Object::new().with("dismissed", true))));
    }
    let text = text_field(object, "text").map(|t| t.strip()).unwrap_or_default();
    let option = match object.get("option") {
        None | Some(Value::Null) => None,
        Some(Value::Str(choice)) if ask.options.contains(choice) => Some(choice.clone()),
        Some(_) => return Err("that option is not one of the choices".into()),
    };
    if option.is_none() && text.is_empty() {
        return Err("pick an option or write an answer".into());
    }
    Ok((ask, Value::Object(Object::new().with("option", option).with("text", text))))
}

fn serve_http(service: &Arc<Service>, mut request: tiny_http::Request) {
    let (path, _) = html::split_target(request.url());
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let Some(first) = parts.first().copied() else {
        return html::fail(request, 404, "not found");
    };
    if !html::constant_time_eq(first, &service.token) {
        return match service.session(first) {
            Some(session) => html::handle(&session, request),
            None => html::fail(request, 404, "not found"),
        };
    }
    match (request.method(), &parts[1..]) {
        (tiny_http::Method::Get, []) => {
            let boot = Object::new().with("itemsUrl", "api/items").with("answerUrl", "api/answer");
            let page = INBOX_PAGE.replace("__INBOX_BOOT__", &json::dumps(&Value::Object(boot), json::COMPACT));
            html::respond(request, 200, page.into_bytes(), "text/html; charset=utf-8");
        }
        (tiny_http::Method::Get, ["api", "items"]) => {
            let body = service.items_json();
            html::respond(request, 200, body, "application/json; charset=utf-8");
        }
        (tiny_http::Method::Post, ["api", "answer"]) => {
            let Some(size) = request.body_length() else {
                return html::fail(request, 411, "an answer needs a Content-Length");
            };
            if size > MAX_ANSWER_BYTES {
                return html::fail(request, 413, "the answer is too large");
            }
            let mut body = Vec::with_capacity(size);
            if request.as_reader().take(size as u64).read_to_end(&mut body).is_err() {
                return html::fail(request, 400, "the answer is not valid JSON");
            }
            match parse_answer(&body, service) {
                Ok((ask, answer)) => {
                    if ask.settle(answer) {
                        html::respond(request, 204, Vec::new(), "");
                    } else {
                        html::fail(request, 409, "that question is no longer waiting");
                    }
                }
                Err(message) => html::fail(request, 400, &message),
            }
        }
        (tiny_http::Method::Get | tiny_http::Method::Post, _) => html::fail(request, 404, "not found"),
        _ => html::fail(request, 501, "unsupported method"),
    }
}

fn idle_limit() -> Duration {
    let raw = std::env::var(IDLE_ENV).unwrap_or_default();
    let seconds = py_strip(&raw).parse::<f64>().ok().filter(|v| *v > 0.0).unwrap_or(IDLE_SECONDS);
    Duration::try_from_secs_f64(seconds).unwrap_or(Duration::from_secs(600))
}

fn run_service() -> Result<()> {
    let dir = ensure_home()?;
    let socket = dir.join("senpai.sock");
    for _ in 0..3 {
        if UnixStream::connect(&socket).is_ok() {
            return Err(Error::Other(format!("a senpai service is already running at {}", socket.display())));
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .map_err(|e| Error::Other(format!("unable to listen on {}: {}", socket.display(), os_error(&e, None))))?;
    let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|e| Error::Other(format!("unable to start the inbox page: {e}")))?;
    let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
    let token = os::token_urlsafe(24);
    let service = Arc::new(Service {
        url: format!("http://127.0.0.1:{port}/{token}/"),
        token,
        build: build_id(),
        socket_id: file_id(&socket),
        socket,
        idle: idle_limit(),
        inbox: Mutex::new(Inbox {
            items: Vec::new(),
            connections: 0,
            last_activity: Instant::now(),
            last_poll: None,
            next_id: 0,
        }),
    });
    diag::write(&format!("senpai serve: inbox {} (pid {})\n", service.url, std::process::id()));
    {
        let service = Arc::clone(&service);
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let service = Arc::clone(&service);
                thread::spawn(move || serve_http(&service, request));
            }
        });
    }
    {
        let service = Arc::clone(&service);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(250));
            let inbox = service.lock();
            if inbox.items.is_empty() && inbox.connections == 0 && inbox.last_activity.elapsed() >= service.idle {
                service.shutdown();
            }
        });
    }
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if !same_user(&stream) {
            continue;
        }
        service.lock().connections += 1;
        let service = Arc::clone(&service);
        thread::spawn(move || {
            serve_connection(&service, stream);
            let mut inbox = service.lock();
            inbox.connections -= 1;
            inbox.last_activity = Instant::now();
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

fn print_json(value: Object) {
    println!("{}", json::dumps(&Value::Object(value), json::DEFAULT));
}

fn browser_allowed() -> bool {
    html::env_flag_enabled(NO_BROWSER_ENV)
}

/// `senpai serve [--stop]`.
pub fn serve_main(argv: &[PyStr]) -> i32 {
    match argv.iter().map(PyStr::display).collect::<Vec<_>>().as_slice() {
        [] => match run_service() {
            Ok(()) => crate::EXIT_OK,
            Err(error) => {
                diag::write(&format!("senpai serve: {}\n", error.message()));
                crate::EXIT_ERROR
            }
        },
        [flag] if flag == "--stop" => {
            let refused = |text: String| {
                diag::write(&format!("senpai serve: {text}\n"));
                crate::EXIT_ERROR
            };
            let not_running = || {
                print_json(Object::new().with("running", false));
                crate::EXIT_OK
            };
            if !home().is_ok_and(|h| h.exists()) {
                return not_running();
            }
            let socket = match ensure_home() {
                Ok(dir) => dir.join("senpai.sock"),
                Err(error) => return refused(error.message()),
            };
            let mut stream = match connect(&socket) {
                Ok(stream) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    return refused(format!("{error}: {}", socket.display()))
                }
                Err(_) => return not_running(),
            };
            let sent = write_message(&mut stream, &Value::Object(Object::new().with("op", "stop")));
            let stopped = sent.is_ok() && read_message(&mut BufReader::new(stream)).is_ok();
            print_json(Object::new().with("stopped", stopped));
            if stopped {
                wait_for_exit();
                crate::EXIT_OK
            } else {
                crate::EXIT_ERROR
            }
        }
        [flag] if flag == "-h" || flag == "--help" => {
            print!("usage: senpai serve [--stop]\n\nRun the review inbox service, or stop the running one.\n");
            crate::EXIT_OK
        }
        _ => {
            eprintln!("usage: senpai serve [--stop]\nsenpai serve: error: unrecognized arguments");
            crate::EXIT_ERROR
        }
    }
}

/// `senpai inbox`: starts the service when needed and opens its page.
pub fn inbox_main(argv: &[PyStr]) -> i32 {
    if !argv.is_empty() {
        let help = argv.len() == 1 && (argv[0] == "-h" || argv[0] == "--help");
        let usage = "usage: senpai inbox\n\nOpen the review inbox, starting its service when needed.\n";
        if help {
            print!("{usage}");
            return crate::EXIT_OK;
        }
        eprintln!("{usage}senpai inbox: error: unrecognized arguments");
        return crate::EXIT_ERROR;
    }
    let message = Object::new().with("op", "status").with("browser", browser_allowed());
    match exchange(message) {
        Ok((_, reply)) => {
            let mut out = Object::new();
            for key in ["inbox", "pending"] {
                if let Some(value) = reply.get(key) {
                    out.insert(key, value.clone());
                }
            }
            print_json(out);
            crate::EXIT_OK
        }
        Err(error) => {
            diag::write(&format!("senpai inbox: {}\n", error.message()));
            crate::EXIT_ERROR
        }
    }
}

const ASK_USAGE: &str = "usage: senpai ask [-h] --question TEXT [--option TEXT] [--context TEXT]\n";

/// `senpai ask --question TEXT [--option TEXT]...`: asks the person in the
/// inbox and prints their answer. Exit 0 answered, 1 dismissed, 2 error.
pub fn ask_main(argv: &[PyStr]) -> i32 {
    let mut question: Option<PyStr> = None;
    let mut context: Option<PyStr> = None;
    let mut options: Vec<PyStr> = Vec::new();
    let mut index = 0;
    let usage_error = |text: &str| {
        eprintln!("{ASK_USAGE}senpai ask: error: {text}");
        crate::EXIT_ERROR
    };
    while index < argv.len() {
        let token = argv[index].display();
        index += 1;
        if token == "-h" || token == "--help" {
            print!(
                "{ASK_USAGE}\nAsk the person a question in the review inbox and print the answer.\n\n\
                 Exit 0 when answered, 1 when dismissed, 2 on error or timeout ({}).\n",
                crate::reviewer::TIMEOUT_ENV
            );
            return crate::EXIT_OK;
        }
        let (flag, inline) = match token.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(PyStr::from(v))),
            _ => (token.clone(), None),
        };
        let value = match inline {
            Some(v) => v,
            None => match argv.get(index) {
                Some(next) => {
                    index += 1;
                    next.clone()
                }
                None => return usage_error(&format!("argument {flag}: expected one argument")),
            },
        };
        match flag.as_str() {
            "--question" => question = Some(value),
            "--option" => options.push(value),
            "--context" => context = Some(value),
            _ => return usage_error(&format!("unrecognized arguments: {token}")),
        }
    }
    let Some(question) = question.filter(|q| !q.strip().is_empty()) else {
        return usage_error("the following arguments are required: --question");
    };
    let timeout = match crate::reviewer::resolve_timeout() {
        Ok(timeout) => timeout,
        Err(error) => return usage_error(&error.message()),
    };
    let fail = |text: String| {
        diag::write(&format!("senpai ask: {text}\n"));
        crate::EXIT_ERROR
    };
    let message = Object::new()
        .with("op", "ask")
        .with("title", &question)
        .with("place", context)
        .with("options", Value::Array(options.iter().map(Value::from).collect()))
        .with("browser", surface(browser_allowed()) == Surface::Browser);
    let (mut reader, reply) = match exchange(message) {
        Ok(pair) => pair,
        Err(error) => return fail(error.message()),
    };
    let question_id = reply.get("question_id").cloned().unwrap_or(Value::Null);
    if let Some(inbox) = text_field(&reply, "inbox") {
        diag::write(&format!("senpai ask: waiting for an answer in the inbox {}\n", inbox.display()));
        present(surface(browser_allowed()), browser_allowed(), &question_id, &inbox.display(), "senpai ask");
    }
    let _ = reader.get_ref().set_read_timeout(timeout.map(Duration::from_secs));
    let answer = match read_message(&mut reader) {
        Ok(Some(Value::Object(reply))) => reply.get("answer").cloned().unwrap_or(Value::Null),
        Ok(_) => Value::Null,
        Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
            return fail("the question timed out before a human answered".into());
        }
        Err(_) => Value::Null,
    };
    let Some(answer) = answer.as_object() else {
        return fail("the senpai service stopped before the question was answered".into());
    };
    let mut out = Object::new().with("version", crate::request::PROTOCOL_VERSION).with("question_id", question_id);
    if matches!(answer.get("dismissed"), Some(Value::Bool(true))) {
        print_json(out.with("dismissed", true));
        return crate::EXIT_NOT_APPROVED;
    }
    for key in ["option", "text"] {
        out.insert(key, answer.get(key).cloned().unwrap_or(Value::Null));
    }
    print_json(out);
    crate::EXIT_OK
}
