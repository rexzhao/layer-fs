//! Typed failures for the overlay backend.

use std::fmt;

use fs_namespace::NamespaceError;

/// Why an overlay operation failed.
#[derive(Debug)]
pub enum OverlayError {
    /// Another handle already has this overlay open (ADR-006 W2).
    ///
    /// The overlay is opened with `locking_mode=EXCLUSIVE`, so a second opener — in
    /// this process or another — cannot share it. Reported rather than waited for:
    /// there is no correct way to serialise two independent writers here.
    WorkspaceInUse {
        /// The overlay file that is already open.
        path: String,
    },
    /// The overlay file was written by a different schema version or DDL.
    SchemaMismatch {
        /// What was expected.
        expected: String,
        /// What the file holds.
        found: String,
    },
    /// A pragma did not take effect.
    PragmaNotApplied {
        /// The pragma's name.
        pragma: &'static str,
        /// The value the file reports.
        found: String,
    },
    /// A rule scope was entered while another was open on this thread (TX4).
    NestedTransaction,
    /// The base snapshot could not be read.
    Base(fs_snapshot::SnapshotError),
    /// A stored value does not match the schema.
    InvalidRecord {
        /// The field at fault.
        field: &'static str,
        /// What was found.
        detail: String,
    },
    /// The tree rules rejected the operation.
    Namespace(NamespaceError),
    /// SQLite failed.
    Sqlite(rusqlite::Error),
}

impl fmt::Display for OverlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OverlayError::WorkspaceInUse { path } => {
                write!(f, "workspace overlay is already open: {path}")
            }
            OverlayError::SchemaMismatch { expected, found } => {
                write!(
                    f,
                    "overlay schema mismatch: expected {expected}, found {found}"
                )
            }
            OverlayError::PragmaNotApplied { pragma, found } => {
                write!(f, "pragma {pragma} did not take effect (found {found})")
            }
            OverlayError::NestedTransaction => {
                f.write_str("an overlay scope was entered while another was still open")
            }
            OverlayError::Base(error) => write!(f, "base snapshot: {error}"),
            OverlayError::InvalidRecord { field, detail } => {
                write!(f, "invalid overlay record in {field}: {detail}")
            }
            OverlayError::Namespace(error) => write!(f, "{error}"),
            OverlayError::Sqlite(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for OverlayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Exhaustive on purpose (C6): a new variant that wraps a cause must be added here, or
        // `chain()` would silently skip it. Direct cause only, no recursion.
        match self {
            OverlayError::Base(error) => Some(error),
            OverlayError::Namespace(error) => Some(error),
            OverlayError::Sqlite(error) => Some(error),
            OverlayError::WorkspaceInUse { .. }
            | OverlayError::SchemaMismatch { .. }
            | OverlayError::PragmaNotApplied { .. }
            | OverlayError::NestedTransaction
            | OverlayError::InvalidRecord { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for OverlayError {
    fn from(error: rusqlite::Error) -> Self {
        OverlayError::Sqlite(error)
    }
}

impl From<fs_snapshot::SnapshotError> for OverlayError {
    fn from(error: fs_snapshot::SnapshotError) -> Self {
        OverlayError::Base(error)
    }
}

impl From<NamespaceError> for OverlayError {
    fn from(error: NamespaceError) -> Self {
        OverlayError::Namespace(error)
    }
}

impl From<cas_core::EnvelopeError> for OverlayError {
    fn from(error: cas_core::EnvelopeError) -> Self {
        OverlayError::InvalidRecord {
            field: "object",
            detail: error.to_string(),
        }
    }
}

impl From<cas_store::StoreError> for OverlayError {
    fn from(error: cas_store::StoreError) -> Self {
        match error {
            cas_store::StoreError::Sqlite(error) => OverlayError::Sqlite(error),
            other => OverlayError::InvalidRecord {
                field: "store",
                detail: other.to_string(),
            },
        }
    }
}

/// The result of an overlay operation.
pub type OverlayResult<T> = Result<T, OverlayError>;
