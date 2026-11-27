//! Typed errors for `fs-dedup`.

use std::fmt;
use std::io;

/// Result alias for `fs-dedup`.
pub type Result<T> = std::result::Result<T, Error>;

/// Domain error type for `fs-dedup`.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Sqlite(rusqlite::Error),
    NotFound(String),
    AlreadyExists(String),
    InvalidInput(String),
    Corrupt(String),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Sqlite(e) => write!(f, "SQLite error: {e}"),
            Self::NotFound(m) => write!(f, "not found: {m}"),
            Self::AlreadyExists(m) => write!(f, "already exists: {m}"),
            Self::InvalidInput(m) => write!(f, "invalid input: {m}"),
            Self::Corrupt(m) => write!(f, "corrupt store: {m}"),
            Self::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}
