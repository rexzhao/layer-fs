//! Typed failures for the workspace layer.

use std::fmt;

use crate::schema::MAIN_BUSY_TIMEOUT_MS;

/// Why a workspace operation failed.
#[derive(Debug)]
pub enum WorkspaceError {
    /// The branch moved since this workspace last published (ADR-006 §10.3).
    ///
    /// Exactly the values that disagreed, so a caller can print "expected X, found Y"
    /// rather than guessing.
    BranchMoved {
        /// The branch whose head moved.
        branch: String,
        /// The commit the workspace was based on.
        expected: String,
        /// The commit the branch actually points at.
        actual: String,
    },
    /// `commit` was called when the workspace's view already equals the base.
    NothingToCommit,
    /// A name was already taken.
    AlreadyExists {
        /// The name.
        name: String,
    },
    /// A workspace, branch or commit was not found.
    NotFound {
        /// What was looked for.
        what: String,
    },
    /// `restore` was asked to replace a workspace that still has changes.
    WorkspaceDirty,
    /// A main-database statement waited its bounded time and still found the database
    /// locked (ADR-006 §11.1).
    ///
    /// The type callers see is the same one P1 used for contention; what changed is only
    /// how long the wait is allowed to be. It is never a reason to retry automatically.
    Busy {
        /// How long the connection was allowed to wait, in milliseconds.
        waited_ms: u64,
    },
    /// The workspace is registered but its overlay file is missing, or the reverse.
    Inconsistent {
        /// What disagreed.
        detail: String,
    },
    /// The main database was written by another build.
    SchemaMismatch {
        /// What was expected.
        expected: String,
        /// What the database holds.
        found: String,
    },
    /// The store has no workspace schema at all.
    NotInitialised,
    /// The tree rules rejected the operation.
    Namespace(fs_namespace::NamespaceError),
    /// The overlay backend failed.
    Overlay(fs_overlay::OverlayError),
    /// The snapshot encoder or reader failed.
    Snapshot(fs_snapshot::SnapshotError),
    /// The CAS store failed.
    Store(cas_store::StoreError),
    /// SQLite failed.
    Sqlite(rusqlite::Error),
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkspaceError::BranchMoved {
                branch,
                expected,
                actual,
            } => write!(
                f,
                "branch {branch} moved: expected commit {expected}, found {actual}"
            ),
            WorkspaceError::NothingToCommit => {
                f.write_str("the workspace's view already equals its base")
            }
            WorkspaceError::AlreadyExists { name } => write!(f, "already exists: {name}"),
            WorkspaceError::NotFound { what } => write!(f, "not found: {what}"),
            WorkspaceError::WorkspaceDirty => {
                f.write_str("the workspace has changes; discard them first")
            }
            WorkspaceError::Busy { waited_ms } => {
                write!(f, "the main database stayed locked for {waited_ms} ms")
            }
            WorkspaceError::Inconsistent { detail } => write!(f, "inconsistent state: {detail}"),
            WorkspaceError::SchemaMismatch { expected, found } => {
                write!(
                    f,
                    "workspace schema mismatch: expected {expected}, found {found}"
                )
            }
            WorkspaceError::NotInitialised => {
                f.write_str("the store has no workspace schema; run init first")
            }
            WorkspaceError::Namespace(error) => write!(f, "{error}"),
            WorkspaceError::Overlay(error) => write!(f, "{error}"),
            WorkspaceError::Snapshot(error) => write!(f, "{error}"),
            WorkspaceError::Store(error) => write!(f, "{error}"),
            WorkspaceError::Sqlite(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for WorkspaceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WorkspaceError::Namespace(error) => Some(error),
            WorkspaceError::Overlay(error) => Some(error),
            WorkspaceError::Snapshot(error) => Some(error),
            WorkspaceError::Store(error) => Some(error),
            WorkspaceError::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for WorkspaceError {
    fn from(error: rusqlite::Error) -> Self {
        // The busy timeout only changes *how long* a statement waits; what the caller
        // sees is still a typed contention error, never a success and never a retry.
        if let rusqlite::Error::SqliteFailure(code, _) = &error {
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) {
                return WorkspaceError::Busy {
                    waited_ms: MAIN_BUSY_TIMEOUT_MS,
                };
            }
        }
        WorkspaceError::Sqlite(error)
    }
}

impl From<fs_namespace::NamespaceError> for WorkspaceError {
    fn from(error: fs_namespace::NamespaceError) -> Self {
        WorkspaceError::Namespace(error)
    }
}

impl From<fs_overlay::OverlayError> for WorkspaceError {
    fn from(error: fs_overlay::OverlayError) -> Self {
        WorkspaceError::Overlay(error)
    }
}

impl From<fs_snapshot::SnapshotError> for WorkspaceError {
    fn from(error: fs_snapshot::SnapshotError) -> Self {
        WorkspaceError::Snapshot(error)
    }
}

impl From<cas_store::StoreError> for WorkspaceError {
    fn from(error: cas_store::StoreError) -> Self {
        // Contention on the CAS index is the same situation as contention on the metadata
        // database, and §11.1 says the caller sees one typed `Busy` either way — the fix
        // must not leak which of the two connections happened to be blocked.
        match error {
            cas_store::StoreError::Busy => WorkspaceError::Busy {
                waited_ms: MAIN_BUSY_TIMEOUT_MS,
            },
            other => WorkspaceError::Store(other),
        }
    }
}

/// The result of a workspace operation.
pub type WorkspaceResult<T> = Result<T, WorkspaceError>;
