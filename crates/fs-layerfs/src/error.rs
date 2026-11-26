//! Typed errors for the LayerFS repository facade and CLI exit-code mapping.

use std::fmt;

/// Typed errors surfaced by `fs-layerfs`.
#[derive(Debug)]
pub enum Error {
    /// Invalid arguments, names, or store configuration.
    InvalidInput(String),
    /// Store, branch, commit, or path does not exist.
    NotFound(String),
    /// Store or workspace is currently locked/in-use by another writer.
    Busy(String),
    /// Branch already exists, merge conflict, or nothing to commit.
    Conflict(String),
    /// Platform or feature refusal.
    Unsupported(String),
    /// Any other storage/IO/internal error.
    Other(String),
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Exit code matching ADR-010 §8.3:
    /// `2` usage/invalid, `3` not found, `4` busy, `5` conflict, `1` other.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::InvalidInput(_) => 2,
            Self::NotFound(_) => 3,
            Self::Busy(_) => 4,
            Self::Conflict(_) => 5,
            Self::Unsupported(_) | Self::Other(_) => 1,
        }
    }

    pub(crate) fn from_sdk(refusal: layerfs_bridge::control::SdkRefusal) -> Self {
        match refusal.code.as_str() {
            "NoSuchBranch" | "NoSuchCommit" | "NoSuchLayer" | "NoSuchPath" | "NotFound" => {
                Self::NotFound(refusal.message)
            }
            "NameTaken" | "Conflict" | "UpToDate" | "NoCommit" | "NoMergeBase" => {
                Self::Conflict(refusal.message)
            }
            "Busy" | "InFlight" | "Capacity" => Self::Busy(refusal.message),
            "InvalidInput" | "ReservedName" | "IsDirectory" | "NotDirectory" => {
                Self::InvalidInput(refusal.message)
            }
            _ => Self::Other(format!("{}: {}", refusal.code, refusal.message)),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(m)
            | Self::NotFound(m)
            | Self::Busy(m)
            | Self::Conflict(m)
            | Self::Unsupported(m)
            | Self::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound(error.to_string()),
            std::io::ErrorKind::AlreadyExists => Self::Conflict(error.to_string()),
            _ => Self::Other(error.to_string()),
        }
    }
}
