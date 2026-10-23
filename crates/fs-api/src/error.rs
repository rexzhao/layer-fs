//! The one error type the API surface reports.

use std::fmt;

/// Why an API operation failed.
///
/// One enum for the whole surface: a caller of `fs-api` should not have to know which of
/// the four crates underneath refused, only what happened to its request.
#[derive(Debug)]
pub enum ApiError {
    /// Path syntax (empty segment, `.`/`..`, forbidden byte, too deep, not absolute…).
    InvalidPath(fs_namespace::InvalidPath),
    /// The path rules refused the operation (not found, exists, not a directory, …).
    Namespace(fs_namespace::NamespaceError),
    /// The workspace layer refused (branch moved, nothing to commit, dirty, busy…).
    Workspace(fs_workspace::WorkspaceError),
    /// The overlay file failed.
    Overlay(fs_overlay::OverlayError),
    /// A snapshot could not be encoded or read.
    Snapshot(fs_snapshot::SnapshotError),
    /// The CAS store failed.
    Store(cas_store::StoreError),
    /// A host file could not be read or written (the CLI's `put`/`get`).
    Io(std::io::Error),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::InvalidPath(error) => write!(f, "{error}"),
            ApiError::Namespace(error) => write!(f, "{error}"),
            ApiError::Workspace(error) => write!(f, "{error}"),
            ApiError::Overlay(error) => write!(f, "{error}"),
            ApiError::Snapshot(error) => write!(f, "{error}"),
            ApiError::Store(error) => write!(f, "{error}"),
            ApiError::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ApiError::InvalidPath(error) => Some(error),
            ApiError::Namespace(error) => Some(error),
            ApiError::Workspace(error) => Some(error),
            ApiError::Overlay(error) => Some(error),
            ApiError::Snapshot(error) => Some(error),
            ApiError::Store(error) => Some(error),
            ApiError::Io(error) => Some(error),
        }
    }
}

impl ApiError {
    /// Whether the failure is "that path does not exist".
    ///
    /// A caller that wants create-or-replace behaviour (`put`) needs this answer without
    /// matching on two layers' worth of enums.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            ApiError::Namespace(fs_namespace::NamespaceError::NotFound { .. })
                | ApiError::Overlay(fs_overlay::OverlayError::Namespace(
                    fs_namespace::NamespaceError::NotFound { .. }
                ))
                | ApiError::Workspace(fs_workspace::WorkspaceError::Namespace(
                    fs_namespace::NamespaceError::NotFound { .. }
                ))
        )
    }
}

impl From<fs_namespace::InvalidPath> for ApiError {
    fn from(error: fs_namespace::InvalidPath) -> Self {
        ApiError::InvalidPath(error)
    }
}

impl From<fs_namespace::NamespaceError> for ApiError {
    fn from(error: fs_namespace::NamespaceError) -> Self {
        ApiError::Namespace(error)
    }
}

impl From<fs_workspace::WorkspaceError> for ApiError {
    fn from(error: fs_workspace::WorkspaceError) -> Self {
        ApiError::Workspace(error)
    }
}

impl From<fs_overlay::OverlayError> for ApiError {
    fn from(error: fs_overlay::OverlayError) -> Self {
        ApiError::Overlay(error)
    }
}

impl From<fs_snapshot::SnapshotError> for ApiError {
    fn from(error: fs_snapshot::SnapshotError) -> Self {
        ApiError::Snapshot(error)
    }
}

impl From<cas_store::StoreError> for ApiError {
    fn from(error: cas_store::StoreError) -> Self {
        ApiError::Store(error)
    }
}

impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        ApiError::Io(error)
    }
}

/// The result of an API operation.
pub type ApiResult<T> = Result<T, ApiError>;
