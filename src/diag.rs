//! Diagnostics on stderr, mirrored into a recovery `stderr.log` while one is
//! open, the way the Python client redirected `sys.stderr`.

use std::fs::File;
use std::io::Write;
use std::sync::Mutex;

static LOG: Mutex<Option<File>> = Mutex::new(None);

pub fn write(text: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.flush();
    if let Ok(mut guard) = LOG.lock() {
        if let Some(file) = guard.as_mut() {
            let _ = file.write_all(text.as_bytes());
            let _ = file.flush();
        }
    }
}

pub fn set_log(file: Option<File>) {
    if let Ok(mut guard) = LOG.lock() {
        *guard = file;
    }
}

/// Incremental UTF-8 decoding with replacement, so a multi-byte character
/// split across reads is not mangled.
#[derive(Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub fn push(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        let keep = incomplete_tail(&self.pending);
        let split = self.pending.len() - keep;
        let text = String::from_utf8_lossy(&self.pending[..split]).into_owned();
        self.pending.drain(..split);
        text
    }

    pub fn finish(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
}

/// Length of a trailing, possibly incomplete multi-byte sequence.
fn incomplete_tail(bytes: &[u8]) -> usize {
    for back in 1..=3.min(bytes.len()) {
        let byte = bytes[bytes.len() - back];
        if byte & 0xC0 == 0x80 {
            continue;
        }
        let width = match byte {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => return 0,
        };
        return if width > back { back } else { 0 };
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_characters_are_joined() {
        let mut stream = Utf8Stream::default();
        let bytes = "é!".as_bytes();
        assert_eq!(stream.push(&bytes[..1]), "");
        assert_eq!(stream.push(&bytes[1..]), "é!");
        assert_eq!(stream.push(b"\xff"), "\u{FFFD}");
        assert_eq!(stream.push(b"\xe2\x82"), "");
        assert_eq!(stream.finish(), "\u{FFFD}");
    }
}
