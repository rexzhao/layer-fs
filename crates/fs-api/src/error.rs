//! Typed failures for the file API (ADR-004).
//!
//! Every variant is a distinct, classifiable outcome. Nothing retries.

use std::fmt;

use cas_store::StoreError;
use fs_namespace::{InvalidPath, NamespaceError};

/// Why a file operation failed.
#[derive(Debug)]
pub enum FsError {
    /// The path is not a valid namespace path.
    InvalidPath(InvalidPath),
    /// Nothing exists at the path.
    NotFound {
        /// The path that was not found.
        path: String,
    },
    /// Something already exists where the operation needed a free name.
    AlreadyExists {
        /// The occupied path.
        path: String,
    },
    /// A path component that must be a directory is a file.
    NotADirectory {
        /// The offending path.
        path: String,
    },
    /// A file operation was applied to a directory.
    IsADirectory {
        /// The offending path.
        path: String,
    },
    /// `rmdir` was applied to a non-empty directory.
    DirectoryNotEmpty {
        /// The non-empty directory.
        path: String,
    },
    /// The root directory cannot be removed.
    IsRoot,
    /// A rename would move a directory inside itself.
    IntoOwnSubtree {
        /// The source.
        from: String,
        /// The destination inside it.
        to: String,
    },
    /// The namespace does not exist.
    NamespaceNotFound {
        /// The requested namespace.
        name: String,
    },
    /// An offset or length does not fit the file's addressable range.
    OffsetOutOfRange {
        /// The requested offset.
        offset: u64,
    },
    /// A stored object is referenced but cannot be read back.
    DanglingReference {
        /// The object the namespace points at.
        object: String,
    },
    /// A stored record could not be interpreted.
    Corrupt {
        /// What was wrong.
        detail: String,
    },
    /// The underlying store or namespace layer failed.
    Store(StoreError),
    /// The namespace layer failed for a reason of its own.
    Namespace(NamespaceError),
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::InvalidPath(error) => write!(f, "invalid path: {error}"),
            FsError::NotFound { path } => write!(f, "no such path: {path}"),
            FsError::AlreadyExists { path } => write!(f, "already exists: {path}"),
            FsError::NotADirectory { path } => write!(f, "not a directory: {path}"),
            FsError::IsADirectory { path } => write!(f, "is a directory: {path}"),
            FsError::DirectoryNotEmpty { path } => write!(f, "directory is not empty: {path}"),
            FsError::IsRoot => f.write_str("the root directory cannot be removed"),
            FsError::IntoOwnSubtree { from, to } => {
                write!(f, "cannot move {from} into its own subtree ({to})")
            }
            FsError::NamespaceNotFound { name } => write!(f, "no such namespace: {name}"),
            FsError::OffsetOutOfRange { offset } => {
                write!(f, "offset {offset} is beyond the addressable range")
            }
            FsError::DanglingReference { object } => {
                write!(
                    f,
                    "namespace references object {object}, which is not in the store"
                )
            }
            FsError::Corrupt { detail } => write!(f, "corrupt file record: {detail}"),
            FsError::Store(error) => write!(f, "{error}"),
            FsError::Namespace(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for FsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FsError::InvalidPath(error) => Some(error),
            FsError::Store(error) => Some(error),
            FsError::Namespace(error) => Some(error),
            _ => None,
        }
    }
}

impl From<InvalidPath> for FsError {
    fn from(error: InvalidPath) -> Self {
        FsError::InvalidPath(error)
    }
}

impl From<NamespaceError> for FsError {
    fn from(error: NamespaceError) -> Self {
        match error {
            NamespaceError::InvalidPath(inner) => FsError::InvalidPath(inner),
            NamespaceError::NotFound { path } => FsError::NotFound { path },
            NamespaceError::AlreadyExists { path } => FsError::AlreadyExists { path },
            NamespaceError::NotADirectory { path } => FsError::NotADirectory { path },
            NamespaceError::IsADirectory { path } => FsError::IsADirectory { path },
            NamespaceError::DirectoryNotEmpty { path } => FsError::DirectoryNotEmpty { path },
            NamespaceError::IsRoot => FsError::IsRoot,
            NamespaceError::IntoOwnSubtree { from, to } => FsError::IntoOwnSubtree { from, to },
            NamespaceError::NamespaceNotFound { name } => FsError::NamespaceNotFound { name },
            NamespaceError::Store(error) => FsError::Store(error),
            other => FsError::Namespace(other),
        }
    }
}

impl From<StoreError> for FsError {
    fn from(error: StoreError) -> Self {
        FsError::Store(error)
    }
}

impl From<cas_core::EnvelopeError> for FsError {
    fn from(error: cas_core::EnvelopeError) -> Self {
        FsError::Corrupt {
            detail: error.to_string(),
        }
    }
}

/// The result of a file operation.
pub type FsResult<T> = Result<T, FsError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_failing_condition() {
        assert!(FsError::IsRoot.to_string().contains("root"));
        assert!(FsError::NotFound { path: "/x".into() }
            .to_string()
            .contains("/x"));
        assert!(FsError::DanglingReference {
            object: "abc".into()
        }
        .to_string()
        .contains("abc"));
        assert!(FsError::OffsetOutOfRange { offset: 7 }
            .to_string()
            .contains('7'));
    }

    #[test]
    fn namespace_errors_keep_their_variant() {
        let converted: FsError = NamespaceError::AlreadyExists { path: "/a".into() }.into();
        assert!(matches!(converted, FsError::AlreadyExists { .. }));
        let converted: FsError = InvalidPath::NotAbsolute.into();
        assert!(matches!(
            converted,
            FsError::InvalidPath(InvalidPath::NotAbsolute)
        ));
    }
}
