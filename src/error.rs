use std::fmt;
use std::io;

use crate::pystr::{py_repr, PyStr};

/// Every failure fails the review closed with exit code 2. The kinds mirror the
/// Python client's exception classes so messages and handling stay the same.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    Usage(String),
    Git(String),
    Launcher(String),
    Result(String),
    Document(String),
    Other(String),
    Interrupted,
}

impl Error {
    pub fn message(&self) -> String {
        match self {
            Error::Usage(m)
            | Error::Git(m)
            | Error::Launcher(m)
            | Error::Result(m)
            | Error::Document(m)
            | Error::Other(m) => m.clone(),
            Error::Interrupted => String::new(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Python's `str(OSError)`: `[Errno N] description: 'path'`.
pub fn os_error(error: &io::Error, path: Option<&[u8]>) -> String {
    let Some(code) = error.raw_os_error() else {
        return error.to_string();
    };
    // SAFETY: strerror returns a pointer to a static, NUL-terminated message.
    let description = unsafe { std::ffi::CStr::from_ptr(libc::strerror(code)) }
        .to_string_lossy()
        .into_owned();
    match path {
        Some(p) => format!("[Errno {code}] {description}: {}", py_repr(&PyStr::decode_surrogateescape(p))),
        None => format!("[Errno {code}] {description}"),
    }
}
