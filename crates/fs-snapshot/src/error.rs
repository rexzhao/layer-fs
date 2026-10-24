//! Typed failures for the snapshot format (ADR-005 §7).
//!
//! Every decode failure is one of these; the order the decoder checks them in is
//! fixed, so the same damaged object always reports the same first failure.

use std::fmt;

use cas_store::StoreError;

/// Why a snapshot could not be encoded or decoded.
#[derive(Debug)]
pub enum SnapshotError {
    /// A pointer pointed at an object of the wrong role.
    WrongRole {
        /// The role the format required.
        expected: &'static str,
        /// The role found.
        found: &'static str,
    },
    /// The value's `fmt` byte is not the one this build writes.
    UnsupportedFormat {
        /// The format byte found.
        found: u8,
    },
    /// The bytes do not form a legal value: truncated, trailing, or a count that
    /// disagrees with the length.
    Malformed {
        /// What was wrong.
        detail: String,
    },
    /// Keys are not strictly increasing, or extents overlap or are out of order.
    ///
    /// Returned by the encoder when its input is unsorted (a caller bug) and by the
    /// decoder when a page's contents are out of order (damage).
    Unsorted {
        /// What was wrong.
        detail: String,
    },
    /// A page descends to a child whose first key disagrees with the key recorded
    /// for it.
    Corrupt {
        /// What was wrong.
        detail: String,
    },
    /// The root was written under a different profile.
    UnsupportedProfile {
        /// The profile id in the root, as hex.
        found: String,
    },
    /// A topology invariant (ADR-005 §9) was violated.
    TopologyViolation {
        /// Which invariant and why.
        detail: String,
    },
    /// The store failed.
    Store(StoreError),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::WrongRole { expected, found } => {
                write!(f, "expected a {expected} page, found {found}")
            }
            SnapshotError::UnsupportedFormat { found } => {
                write!(f, "unsupported value format {found}")
            }
            SnapshotError::Malformed { detail } => write!(f, "malformed snapshot value: {detail}"),
            SnapshotError::Unsorted { detail } => {
                write!(f, "keys are not strictly increasing: {detail}")
            }
            SnapshotError::Corrupt { detail } => write!(f, "corrupt snapshot structure: {detail}"),
            SnapshotError::UnsupportedProfile { found } => {
                write!(f, "snapshot written under an unknown profile {found}")
            }
            SnapshotError::TopologyViolation { detail } => {
                write!(f, "topology violation: {detail}")
            }
            SnapshotError::Store(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Exhaustive on purpose: a new variant that wraps a cause must be listed here, or a
        // `chain()` walks past it and the cause disappears (architect's C6 rule). Only the
        // *direct* cause is returned — never a recursive walk.
        match self {
            SnapshotError::Store(error) => Some(error),
            SnapshotError::WrongRole { .. }
            | SnapshotError::UnsupportedFormat { .. }
            | SnapshotError::Malformed { .. }
            | SnapshotError::Unsorted { .. }
            | SnapshotError::Corrupt { .. }
            | SnapshotError::UnsupportedProfile { .. }
            | SnapshotError::TopologyViolation { .. } => None,
        }
    }
}

impl From<StoreError> for SnapshotError {
    fn from(error: StoreError) -> Self {
        SnapshotError::Store(error)
    }
}

impl From<cas_core::EnvelopeError> for SnapshotError {
    fn from(error: cas_core::EnvelopeError) -> Self {
        SnapshotError::Malformed {
            detail: error.to_string(),
        }
    }
}

/// The result of a snapshot operation.
pub type SnapshotResult<T> = Result<T, SnapshotError>;

/// A malformed-value shorthand.
pub(crate) fn malformed(detail: impl Into<String>) -> SnapshotError {
    SnapshotError::Malformed {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    /// `Display` and `source` for every variant (C6); expected strings come from QA's probe.
    #[test]
    fn every_variant_has_a_display_and_a_source() {
        let cases: Vec<(SnapshotError, &str, Option<&str>)> = vec![
            (
                SnapshotError::WrongRole { expected: "Directory", found: "Bytes" },
                "expected a Directory page, found Bytes",
                None,
            ),
            (SnapshotError::UnsupportedFormat { found: 9 }, "unsupported value format 9", None),
            (SnapshotError::Malformed { detail: "short".into() }, "malformed snapshot value: short", None),
            (SnapshotError::Unsorted { detail: "keys".into() }, "keys are not strictly increasing: keys", None),
            (SnapshotError::Corrupt { detail: "child".into() }, "corrupt snapshot structure: child", None),
            (
                SnapshotError::UnsupportedProfile { found: "0x02".to_owned() },
                "snapshot written under an unknown profile 0x02",
                None,
            ),
            (
                SnapshotError::TopologyViolation { detail: "cycle".into() },
                "topology violation: cycle",
                None,
            ),
            (
                SnapshotError::Store(cas_store::StoreError::Busy),
                "store is busy: another writer holds the write lane; the operation did not take effect",
                Some("store is busy: another writer holds the write lane; the operation did not take effect"),
            ),
        ];
        for (error, display, source) in cases {
            assert_eq!(error.to_string(), display, "Display for {error:?}");
            assert_eq!(
                error.source().map(std::string::ToString::to_string),
                source.map(str::to_owned),
                "source for {error:?}"
            );
        }
    }
}
