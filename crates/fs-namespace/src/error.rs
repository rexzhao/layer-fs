//! Typed failures for namespace operations (ADR-003 §3).
//!
//! Every variant corresponds to one row of the violation enumerations in
//! ADR-003 §3.1–§3.5. There is no catch-all: a caller can classify exactly what
//! went wrong, and no operation retries or repairs on its own.

use std::fmt;

use cas_store::StoreError;

/// Why a path is not a valid namespace path (ADR-003 §3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidPath {
    /// The path does not start with `/`.
    NotAbsolute,
    /// A segment is empty: `//`, or a trailing `/`.
    EmptySegment,
    /// A segment is exactly `.`.
    DotSegment,
    /// A segment is exactly `..`.
    DotDotSegment,
    /// A segment is longer than [`MAX_NAME_BYTES`].
    NameTooLong {
        /// Byte length of the offending segment.
        len: usize,
    },
    /// A segment contains NUL or `/`.
    ForbiddenByte {
        /// The byte that was refused.
        byte: u8,
    },
    /// The path has more segments than [`MAX_DEPTH`].
    TooDeep {
        /// Number of segments found.
        len: usize,
    },
    /// The name bytes are not valid UTF-8.
    NotUtf8,
}

/// What an operation had to do with an entry that already existed, or that did
/// not (ADR-003 §3.2, §3.3).
#[derive(Debug)]
pub enum NamespaceError {
    /// The resolution of a path component failed before the operation ran.
    InvalidPath(InvalidPath),
    /// A path or one of its parents does not exist.
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
        /// The file that was used as a directory.
        path: String,
    },
    /// A directory operation was applied to a file.
    IsADirectory {
        /// The directory that was used as a file.
        path: String,
    },
    /// `rmdir` was applied to a directory that still has entries.
    DirectoryNotEmpty {
        /// The non-empty directory.
        path: String,
    },
    /// The root directory cannot be removed.
    IsRoot,
    /// `rename` would move a directory inside itself.
    IntoOwnSubtree {
        /// The source directory.
        from: String,
        /// The destination that lies inside it.
        to: String,
    },
    /// The namespace does not exist.
    NamespaceNotFound {
        /// The requested namespace.
        name: String,
    },
    /// The database's namespace tables are not the ones this build creates.
    SchemaMismatch {
        /// Which recorded value disagreed.
        what: &'static str,
    },
    /// A write was attempted on a read-only namespace handle.
    ReadOnly,
    /// A stored row holds a value this build cannot interpret.
    InvalidRecord {
        /// The column at fault.
        field: &'static str,
        /// What was found.
        detail: String,
    },
    /// The store reported a failure.
    Store(StoreError),
}

impl fmt::Display for InvalidPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InvalidPath::NotAbsolute => f.write_str("path must be absolute (start with '/')"),
            InvalidPath::EmptySegment => {
                f.write_str("path has an empty segment ('//' or a trailing '/')")
            }
            InvalidPath::DotSegment => f.write_str("path must not contain a '.' segment"),
            InvalidPath::DotDotSegment => f.write_str("path must not contain a '..' segment"),
            InvalidPath::NameTooLong { len } => write!(
                f,
                "name is {len} bytes, longer than the {} byte limit",
                MAX_NAME_BYTES
            ),
            InvalidPath::ForbiddenByte { byte } => {
                write!(f, "name contains the forbidden byte {byte:#04x}")
            }
            InvalidPath::TooDeep { len } => write!(
                f,
                "path has {len} segments, more than the {MAX_DEPTH} limit"
            ),
            InvalidPath::NotUtf8 => f.write_str("name is not valid UTF-8"),
        }
    }
}

impl fmt::Display for NamespaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamespaceError::InvalidPath(error) => write!(f, "invalid path: {error}"),
            NamespaceError::NotFound { path } => write!(f, "no such path: {path}"),
            NamespaceError::AlreadyExists { path } => write!(f, "already exists: {path}"),
            NamespaceError::NotADirectory { path } => write!(f, "not a directory: {path}"),
            NamespaceError::IsADirectory { path } => write!(f, "is a directory: {path}"),
            NamespaceError::DirectoryNotEmpty { path } => {
                write!(f, "directory is not empty: {path}")
            }
            NamespaceError::IsRoot => f.write_str("the root directory cannot be removed"),
            NamespaceError::IntoOwnSubtree { from, to } => {
                write!(f, "cannot move {from} into its own subtree ({to})")
            }
            NamespaceError::NamespaceNotFound { name } => {
                write!(f, "no such namespace: {name}")
            }
            NamespaceError::SchemaMismatch { what } => {
                write!(f, "namespace schema mismatch in {what}")
            }
            NamespaceError::ReadOnly => f.write_str("namespace is open read-only"),
            NamespaceError::InvalidRecord { field, detail } => {
                write!(f, "invalid namespace record in {field}: {detail}")
            }
            NamespaceError::Store(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for InvalidPath {}

impl std::error::Error for NamespaceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NamespaceError::Store(error) => Some(error),
            NamespaceError::InvalidPath(_) => None,
            _ => None,
        }
    }
}

impl From<InvalidPath> for NamespaceError {
    fn from(error: InvalidPath) -> Self {
        NamespaceError::InvalidPath(error)
    }
}

impl From<StoreError> for NamespaceError {
    fn from(error: StoreError) -> Self {
        NamespaceError::Store(error)
    }
}

impl From<rusqlite::Error> for NamespaceError {
    fn from(error: rusqlite::Error) -> Self {
        NamespaceError::Store(StoreError::from(error))
    }
}

/// The result of a namespace operation.
pub type NamespaceResult<T> = Result<T, NamespaceError>;

/// Largest accepted single name, in bytes (ADR-003 §3.1 P5).
pub const MAX_NAME_BYTES: usize = 255;

/// Largest accepted path depth, in segments (ADR-003 §3.1 P8).
pub const MAX_DEPTH: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_failing_rule() {
        assert!(InvalidPath::NotAbsolute.to_string().contains("absolute"));
        assert!(InvalidPath::DotSegment.to_string().contains("'.'"));
        assert!(InvalidPath::NameTooLong { len: 300 }
            .to_string()
            .contains("255"));
        assert!(InvalidPath::TooDeep { len: 99 }.to_string().contains('9'));
        assert!(InvalidPath::ForbiddenByte { byte: 0 }
            .to_string()
            .contains("0x00"));
        assert!(NamespaceError::IsRoot.to_string().contains("root"));
        assert!(NamespaceError::IntoOwnSubtree {
            from: "/a".into(),
            to: "/a/b".into()
        }
        .to_string()
        .contains("own subtree"));
        assert!(NamespaceError::DirectoryNotEmpty { path: "/d".into() }
            .to_string()
            .contains("/d"));
    }

    /// QA round 2, C5: every `InvalidPath` and `NamespaceError` variant produces a
    /// non-empty message, and the ones carrying data surface it. Enumerating the
    /// arms is deliberate: a spot check of five variants cannot see an empty sixth.
    #[test]
    fn every_variant_has_a_diagnosable_message() {
        let invalid_paths = [
            InvalidPath::NotAbsolute,
            InvalidPath::EmptySegment,
            InvalidPath::DotSegment,
            InvalidPath::DotDotSegment,
            InvalidPath::NameTooLong { len: 300 },
            InvalidPath::ForbiddenByte { byte: 0 },
            InvalidPath::TooDeep { len: 65 },
            InvalidPath::NotUtf8,
        ];
        for error in invalid_paths {
            let text = error.to_string();
            assert!(!text.is_empty(), "{error:?} has an empty message");
            assert!(
                text.chars().any(|c| c.is_ascii_alphabetic()),
                "{error:?} message {text:?} has no words"
            );
        }

        let path = |value: &str| value.to_owned();
        let errors = [
            NamespaceError::InvalidPath(InvalidPath::DotSegment),
            NamespaceError::NotFound {
                path: path("/gone"),
            },
            NamespaceError::AlreadyExists {
                path: path("/here"),
            },
            NamespaceError::NotADirectory {
                path: path("/file"),
            },
            NamespaceError::IsADirectory { path: path("/dir") },
            NamespaceError::DirectoryNotEmpty {
                path: path("/full"),
            },
            NamespaceError::IsRoot,
            NamespaceError::IntoOwnSubtree {
                from: path("/src"),
                to: path("/src/in"),
            },
            NamespaceError::NamespaceNotFound { name: path("ns1") },
            NamespaceError::SchemaMismatch {
                what: "namespace_policy.ddl",
            },
            NamespaceError::ReadOnly,
            NamespaceError::InvalidRecord {
                field: "file.kind",
                detail: path("unknown tag 9"),
            },
        ];
        for error in &errors {
            let text = error.to_string();
            assert!(!text.is_empty(), "{error:?} has an empty message");
        }

        // Data-carrying variants must surface their data.
        let carrying: [(NamespaceError, &str); 8] = [
            (
                NamespaceError::NotFound {
                    path: path("/needle"),
                },
                "/needle",
            ),
            (
                NamespaceError::AlreadyExists {
                    path: path("/taken"),
                },
                "/taken",
            ),
            (NamespaceError::NotADirectory { path: path("/nd") }, "/nd"),
            (NamespaceError::IsADirectory { path: path("/id") }, "/id"),
            (
                NamespaceError::DirectoryNotEmpty { path: path("/dn") },
                "/dn",
            ),
            (
                NamespaceError::NamespaceNotFound { name: path("myns") },
                "myns",
            ),
            (
                NamespaceError::SchemaMismatch {
                    what: "namespace_policy.ddl",
                },
                "namespace_policy.ddl",
            ),
            (
                NamespaceError::InvalidRecord {
                    field: "file.kind",
                    detail: path("tag 9"),
                },
                "tag 9",
            ),
        ];
        for (error, needle) in carrying {
            let text = error.to_string();
            assert!(
                text.contains(needle),
                "{error:?} message {text:?} omits {needle:?}"
            );
        }
    }
}
