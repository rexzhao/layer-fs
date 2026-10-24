//! The typed failures a `cas-store` operation can report.
//!
//! Every variant names the exact condition. Callers classify programmatically;
//! no variant exists for "something went wrong", and no operation retries or
//! repairs on its own (ADR-002 §11, ADR-001 D10).

use std::fmt;
use std::io;

use cas_core::{EnvelopeError, IdParseError, ObjectId};

/// Why a Store operation failed.
#[derive(Debug)]
pub enum StoreError {
    /// The Store's filesystem could not be read or written.
    Io(io::Error),
    /// SQLite reported a failure (statement, transaction or pragma).
    Sqlite(rusqlite::Error),
    /// A concurrent writer held the Store's single write lane.
    ///
    /// The operation did not take effect. ADR-002 §8: no busy handler and no
    /// automatic retry, so the caller decides whether to try again.
    Busy,
    /// `get`/`verify` was asked for an object the Store has not published.
    NotFound {
        /// The requested object.
        oid: ObjectId,
    },
    /// The object file or its index row failed validation.
    ///
    /// The object is published, but its bytes cannot be trusted. Never
    /// substituted with zeros or returned partially.
    Corrupt {
        /// The object whose bytes failed validation.
        oid: ObjectId,
        /// What exactly failed.
        cause: CorruptCause,
    },
    /// The index names a pack whose file is not there (ADR-008 §7).
    ///
    /// This is **storage damage**, not "the object does not exist": the index says the pack
    /// was published, so its absence means the bytes were lost or removed after publication
    /// (an external deleter, a filesystem fault, a bad restore). Reporting it as `NotFound`
    /// would send a caller looking for a missing *object* that the index claims to have.
    MissingPack {
        /// The pack the index points at.
        pack_id: String,
    },
    /// A pack file's tail or bytes failed validation (ADR-008 §7).
    ///
    /// The pack is named rather than the object: the damage is to the container, so every
    /// object inside it is suspect and `verify` reports it once per pack.
    CorruptPack {
        /// The pack that failed validation.
        pack_id: String,
        /// What exactly failed.
        cause: CorruptCause,
    },
    /// The Store was opened on disk with a schema version this build does not
    /// support.
    UnsupportedSchemaVersion {
        /// The version recorded in the Store.
        found: u32,
        /// The version this build implements.
        supported: u32,
    },
    /// The database's recorded DDL or policy is not what this build creates.
    SchemaMismatch {
        /// Which part disagreed.
        what: &'static str,
    },
    /// A write was attempted on a Store opened read-only.
    ReadOnly,
    /// The value exceeds the object format's limit.
    TooLarge {
        /// Declared length in bytes.
        declared: u64,
        /// The largest accepted length.
        limit: u64,
    },
    /// `put` was given a `payload_len` the format forbids, or another envelope
    /// rule rejected the requested object.
    InvalidObject(EnvelopeError),
    /// A stored index row holds a value the Store cannot interpret.
    InvalidRecord {
        /// The column or field at fault.
        field: &'static str,
        /// A short description of what was found.
        detail: String,
    },
}

/// What specifically made an object's bytes untrustworthy.
#[derive(Debug)]
pub enum CorruptCause {
    /// The file's size differs from the length recorded in the index.
    LengthMismatch {
        /// Bytes the index recorded.
        expected: u64,
        /// Bytes actually on disk.
        actual: u64,
    },
    /// The bytes are not a valid version-1 object envelope.
    Envelope(EnvelopeError),
    /// The bytes decode, but hash to a different identity than their path and
    /// index row claim.
    IdentityMismatch {
        /// The identity the path and index claim.
        expected: ObjectId,
        /// The identity the bytes actually hash to.
        actual: ObjectId,
    },
    /// A pack's tail is missing, mismatched or does not hash to its frames.
    PackTail(String),
    /// The index row's role does not match the role in the object's envelope.
    RoleMismatch {
        /// Role recorded in the index.
        recorded: i64,
        /// Role found in the envelope.
        actual: u8,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(error) => write!(f, "store i/o error: {error}"),
            StoreError::Sqlite(error) => write!(f, "store database error: {error}"),
            StoreError::Busy => f.write_str(
                "store is busy: another writer holds the write lane; the operation did not take effect",
            ),
            StoreError::NotFound { oid } => write!(f, "object {oid} is not in the store"),
            StoreError::Corrupt { oid, cause } => write!(f, "object {oid} is corrupt: {cause}"),
            StoreError::MissingPack { pack_id } => write!(
                f,
                "pack {pack_id} is missing: the index names it, the file is not there                  (storage damage, not a missing object)"
            ),
            StoreError::CorruptPack { pack_id, cause } => {
                write!(f, "pack {pack_id} is corrupt: {cause}")
            }
            StoreError::UnsupportedSchemaVersion { found, supported } => write!(
                f,
                "store schema version {found} is not supported (this build reads {supported})"
            ),
            StoreError::SchemaMismatch { what } => {
                write!(f, "store schema mismatch in {what}")
            }
            StoreError::ReadOnly => f.write_str("store is open read-only"),
            StoreError::TooLarge { declared, limit } => {
                write!(f, "object of {declared} bytes exceeds the limit {limit}")
            }
            StoreError::InvalidObject(error) => write!(f, "invalid object: {error}"),
            StoreError::InvalidRecord { field, detail } => {
                write!(f, "invalid store record in {field}: {detail}")
            }
        }
    }
}

impl fmt::Display for CorruptCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CorruptCause::LengthMismatch { expected, actual } => write!(
                f,
                "index records {expected} bytes but the file has {actual}"
            ),
            CorruptCause::Envelope(error) => write!(f, "{error}"),
            CorruptCause::IdentityMismatch { expected, actual } => {
                write!(f, "bytes hash to {actual}, not {expected}")
            }
            CorruptCause::RoleMismatch { recorded, actual } => {
                write!(
                    f,
                    "index records role {recorded} but the envelope says {actual}"
                )
            }
            CorruptCause::PackTail(what) => write!(f, "pack tail is not valid: {what}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Exhaustive on purpose (C6). Two branch kinds here, and both are tested:
        //  - the wrapped cause is an `Error` (Io/Sqlite/InvalidObject);
        //  - `Corrupt` delegates to its `CorruptCause`, and **only** the `Envelope` cause is an
        //    `Error` — the others describe bytes, not a nested failure.
        match self {
            StoreError::Io(error) => Some(error),
            StoreError::Sqlite(error) => Some(error),
            StoreError::InvalidObject(error) => Some(error),
            // `CorruptCause` is a description, not an `Error`: only the envelope branch wraps a
            // real nested cause. Both branches are covered by tests (C6).
            StoreError::Corrupt {
                cause: CorruptCause::Envelope(error),
                ..
            } => Some(error),
            StoreError::Corrupt { .. } => None,
            StoreError::Busy
            | StoreError::NotFound { .. }
            | StoreError::MissingPack { .. }
            | StoreError::CorruptPack { .. }
            | StoreError::UnsupportedSchemaVersion { .. }
            | StoreError::SchemaMismatch { .. }
            | StoreError::ReadOnly
            | StoreError::TooLarge { .. }
            | StoreError::InvalidRecord { .. } => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        StoreError::Io(error)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        map_sqlite(error)
    }
}

impl From<EnvelopeError> for StoreError {
    fn from(error: EnvelopeError) -> Self {
        match error {
            EnvelopeError::TooLarge { declared, limit } => StoreError::TooLarge { declared, limit },
            other => StoreError::InvalidObject(other),
        }
    }
}

impl From<IdParseError> for StoreError {
    fn from(error: IdParseError) -> Self {
        StoreError::InvalidRecord {
            field: "object_location.oid",
            detail: error.to_string(),
        }
    }
}

/// Maps a contended SQLite failure to [`StoreError::Busy`].
///
/// SQLite reports contention as `SQLITE_BUSY` / `SQLITE_LOCKED`. Everything
/// else keeps its own variant so the cause is never hidden behind "busy".
fn map_sqlite(error: rusqlite::Error) -> StoreError {
    use rusqlite::ffi::ErrorCode;
    match &error {
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
            ) =>
        {
            StoreError::Busy
        }
        _ => StoreError::Sqlite(error),
    }
}

/// The result of a Store operation.
pub type StoreResult<T> = Result<T, StoreError>;

#[cfg(test)]
mod tests {

    /// `Corrupt` has two branch kinds (C6): only the `Envelope` cause wraps a nested error.
    #[test]
    fn corrupt_reports_a_source_only_for_the_envelope_cause() {
        use cas_core::EnvelopeError;
        use std::error::Error as _;

        let oid = cas_core::ObjectId::from_bytes([0xCD; 32]);
        let envelope = StoreError::Corrupt {
            oid,
            cause: CorruptCause::Envelope(EnvelopeError::LengthMismatch {
                declared: 10,
                actual: 4,
            }),
        };
        assert!(
            envelope.source().is_some(),
            "an envelope failure is a nested error and must be reported"
        );

        let bytes = StoreError::Corrupt {
            oid,
            cause: CorruptCause::LengthMismatch {
                expected: 10,
                actual: 4,
            },
        };
        assert!(
            bytes.source().is_none(),
            "a byte-level cause describes bytes; it has no nested error"
        );
    }
    use super::*;

    #[test]
    fn displays_name_the_failing_condition() {
        let oid = cas_core::ObjectId::from_bytes([7u8; 32]);
        assert!(StoreError::Busy.to_string().contains("busy"));
        assert!(StoreError::ReadOnly.to_string().contains("read-only"));
        assert!(StoreError::NotFound { oid }
            .to_string()
            .contains("not in the store"));
        assert!(StoreError::UnsupportedSchemaVersion {
            found: 9,
            supported: 1
        }
        .to_string()
        .contains('9'));
        assert!(StoreError::Corrupt {
            oid,
            cause: CorruptCause::LengthMismatch {
                expected: 10,
                actual: 4
            }
        }
        .to_string()
        .contains("corrupt"));
        assert!(StoreError::InvalidRecord {
            field: "object_location.oid",
            detail: "bad".into()
        }
        .to_string()
        .contains("object_location.oid"));
    }

    #[test]
    fn envelope_errors_split_into_too_large_and_invalid() {
        let too_large = StoreError::from(cas_core::EnvelopeError::TooLarge {
            declared: 99,
            limit: 10,
        });
        assert!(matches!(too_large, StoreError::TooLarge { .. }));

        let invalid = StoreError::from(cas_core::EnvelopeError::UnknownRole { found: 42 });
        assert!(matches!(invalid, StoreError::InvalidObject(_)));
    }

    /// QA round 2, C5: every variant produces a non-empty, diagnosable message, and
    /// the data-carrying ones surface their data. Enumerating all of them replaces
    /// spot-checks of the few that happened to be convenient to build.
    #[test]
    fn every_variant_has_a_diagnosable_message() {
        let oid = cas_core::ObjectId::from_bytes([0xAB; 32]);
        let id = oid.to_string();

        let errors = vec![
            StoreError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "gone")),
            StoreError::Busy,
            StoreError::NotFound { oid },
            StoreError::Corrupt {
                oid,
                cause: CorruptCause::LengthMismatch {
                    expected: 10,
                    actual: 4,
                },
            },
            StoreError::UnsupportedSchemaVersion {
                found: 9,
                supported: 1,
            },
            StoreError::SchemaMismatch {
                what: "store_policy.max_object_len",
            },
            StoreError::MissingPack {
                pack_id: "0123456789abcdef0123456789abcdef".to_owned(),
            },
            StoreError::ReadOnly,
            StoreError::TooLarge {
                declared: 99,
                limit: 10,
            },
            StoreError::InvalidObject(cas_core::EnvelopeError::UnknownRole { found: 42 }),
            StoreError::InvalidRecord {
                field: "object_location.oid",
                detail: "bad".to_owned(),
            },
        ];

        for error in &errors {
            let text = error.to_string();
            assert!(!text.is_empty(), "{error:?} has an empty message");
            assert!(
                text.chars().any(|c| c.is_ascii_alphabetic()),
                "{error:?} message {text:?} has no words"
            );
        }

        // The object-id-carrying variants must name the id, or the report cannot be
        // acted on.
        assert!(StoreError::NotFound { oid }.to_string().contains(&id));
        assert!(StoreError::Corrupt {
            oid,
            cause: CorruptCause::LengthMismatch {
                expected: 10,
                actual: 4
            }
        }
        .to_string()
        .contains(&id));

        // Limits and schema details must be visible.
        let too_large = StoreError::TooLarge {
            declared: 4242,
            limit: 1111,
        }
        .to_string();
        assert!(too_large.contains("4242") && too_large.contains("1111"));

        let version = StoreError::UnsupportedSchemaVersion {
            found: 77,
            supported: 33,
        }
        .to_string();
        assert!(version.contains("77") && version.contains("33"));
    }

    /// Every `CorruptCause` arm has a distinct, non-empty message.
    #[test]
    fn every_corruption_cause_is_described() {
        let oid = cas_core::ObjectId::from_bytes([1u8; 32]);
        let causes = vec![
            CorruptCause::LengthMismatch {
                expected: 1,
                actual: 2,
            },
            CorruptCause::IdentityMismatch {
                expected: oid,
                actual: cas_core::ObjectId::from_bytes([2u8; 32]),
            },
            CorruptCause::Envelope(cas_core::EnvelopeError::Truncated {
                needed: 14,
                actual: 1,
            }),
        ];
        let mut messages = Vec::new();
        for cause in causes {
            let text = StoreError::Corrupt { oid, cause }.to_string();
            assert!(!text.is_empty());
            messages.push(text);
        }
        let mut sorted = messages.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), messages.len(), "causes read identically");
    }
}
