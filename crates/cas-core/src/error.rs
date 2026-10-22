//! Typed failures for decoding and identity parsing.
//!
//! Every refusal names the exact rule that failed so a caller can report a
//! corrupt object precisely. There is no catch-all variant and no partial
//! success: a decode either yields a complete checked envelope or one of these
//! errors.

use std::fmt;

/// Why a byte string is not a valid object envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeError {
    /// The input is shorter than the fixed envelope header.
    Truncated {
        /// Bytes the header needs.
        needed: usize,
        /// Bytes supplied.
        actual: usize,
    },
    /// Bytes 0..4 are not the format magic.
    BadMagic {
        /// The four bytes that were read.
        found: [u8; 4],
    },
    /// The version byte is not [`crate::VERSION`].
    UnsupportedVersion {
        /// The version byte that was read.
        found: u8,
    },
    /// The role byte does not name a version-1 role.
    UnknownRole {
        /// The role byte that was read.
        found: u8,
    },
    /// `payload_len` and `value_len` disagree with the input length.
    LengthMismatch {
        /// Length declared by the header.
        declared: u64,
        /// Bytes actually present.
        actual: u64,
    },
    /// A length field or the total exceeds what this format accepts.
    TooLarge {
        /// The declared length, in bytes.
        declared: u64,
        /// The largest accepted length for that field.
        limit: u64,
    },
    /// The bytes decode, but their identity is not the one that was expected.
    IdentityMismatch {
        /// The identity the caller looked up.
        expected: crate::ObjectId,
        /// The identity the bytes actually hash to.
        actual: crate::ObjectId,
    },
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnvelopeError::Truncated { needed, actual } => write!(
                f,
                "object envelope is truncated: header needs {needed} bytes, found {actual}"
            ),
            EnvelopeError::BadMagic { found } => {
                write!(f, "object envelope has wrong magic {found:02x?}")
            }
            EnvelopeError::UnsupportedVersion { found } => {
                write!(f, "unsupported object envelope version {found}")
            }
            EnvelopeError::UnknownRole { found } => {
                write!(f, "unknown object role tag {found}")
            }
            EnvelopeError::LengthMismatch { declared, actual } => write!(
                f,
                "object envelope length mismatch: header declares {declared} bytes, found {actual}"
            ),
            EnvelopeError::TooLarge { declared, limit } => {
                write!(f, "declared length {declared} exceeds the limit {limit}")
            }
            EnvelopeError::IdentityMismatch { expected, actual } => write!(
                f,
                "object identity mismatch: expected {expected}, bytes hash to {actual}"
            ),
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// Why a string is not a hex-encoded [`crate::ObjectId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdParseError {
    /// The string does not have exactly 64 characters.
    Length {
        /// Characters found.
        actual: usize,
    },
    /// A character was not a hex digit.
    NotHex {
        /// Byte offset of the offending character.
        offset: usize,
        /// The offending character.
        found: char,
    },
}

impl fmt::Display for IdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdParseError::Length { actual } => write!(
                f,
                "object id must be 64 hex characters, found {actual} characters"
            ),
            IdParseError::NotHex { offset, found } => {
                write!(f, "object id has a non-hex character {found:?} at {offset}")
            }
        }
    }
}

impl std::error::Error for IdParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_name_the_failing_rule() {
        assert!(EnvelopeError::BadMagic { found: *b"LFSO" }
            .to_string()
            .contains("magic"));
        assert!(EnvelopeError::UnsupportedVersion { found: 9 }
            .to_string()
            .contains('9'));
        assert!(EnvelopeError::UnknownRole { found: 42 }
            .to_string()
            .contains("42"));
        assert!(EnvelopeError::Truncated {
            needed: 14,
            actual: 3
        }
        .to_string()
        .contains("truncated"));
        assert!(EnvelopeError::TooLarge {
            declared: 99,
            limit: 16
        }
        .to_string()
        .contains("exceeds"));
        assert!(IdParseError::Length { actual: 4 }.to_string().contains('4'));
        assert!(IdParseError::NotHex {
            offset: 0,
            found: 'z'
        }
        .to_string()
        .contains('z'));
    }
}
