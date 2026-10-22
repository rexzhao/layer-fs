//! Object identity: a domain-separated BLAKE3 digest over canonical bytes.
//!
//! The identity of an object is the BLAKE3 digest of a fixed domain string
//! followed by the object's complete canonical envelope:
//!
//! ```text
//! ObjectId = BLAKE3("wcas/object/v1\0" || magic || version || role
//!                   || payload_len || value_len || value)
//! ```
//!
//! The domain string carries its own version and ends in a NUL so no other
//! layer's hashes, and no later format version's, can collide with these by
//! construction. The envelope bytes include the role and both length fields,
//! which is what keeps two objects with equal values but different roles
//! distinct.

use std::fmt;
use std::str::FromStr;

use blake3::Hasher;

use crate::error::IdParseError;
use crate::role::Role;
use crate::{EnvelopeError, MAX_OBJECT_LEN};

/// Domain separator mixed in ahead of every object's canonical bytes.
///
/// Frozen by format version 1 (see `docs/adr/ADR-001-object-encoding.md`).
/// Changing it changes every identity in a Store, so it is versioned both by
/// this constant's value and by the envelope's version byte.
pub(crate) const DOMAIN: &[u8] = b"wcas/object/v1\x00";

/// Raw digest width of an [`ObjectId`], in bytes.
pub const ID_BYTES: usize = 32;

/// Length of an [`ObjectId`]'s lowercase hex form.
pub const ID_HEX_LEN: usize = ID_BYTES * 2;

/// The canonical identity of one object.
///
/// Two objects have the same `ObjectId` exactly when their canonical envelopes
/// are byte-identical. Display and `FromStr` use 64 lowercase hex characters,
/// the form used for on-disk object paths and CLI output.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId([u8; ID_BYTES]);

impl ObjectId {
    /// Wraps a raw digest.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; ID_BYTES]) -> Self {
        Self(bytes)
    }

    /// The raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; ID_BYTES] {
        &self.0
    }

    /// A new hasher with the domain separator already mixed in.
    ///
    /// Feed data in exactly the order [`ObjectIdHasher::finish`] expects; see
    /// [`ObjectIdHasher`] for the streaming contract. Callers that only want
    /// [`ObjectIdHasher::start`] do not need this entry point.
    #[must_use]
    pub fn hasher() -> ObjectIdHasher {
        ObjectIdHasher::start()
    }

    /// The identity of one complete object, hashed in a single pass.
    ///
    /// The result equals what [`ObjectId::hasher`] yields for the same envelope
    /// and value, so streaming and whole-buffer callers agree.
    pub fn hash(role: Role, value: &[u8]) -> Result<Self, EnvelopeError> {
        let mut hasher = Self::hasher();
        hasher.update_envelope(role, value.len())?;
        hasher.update_value(value);
        hasher.finish()
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({self})")
    }
}

impl FromStr for ObjectId {
    type Err = IdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = s.as_bytes();
        if bytes.len() != ID_HEX_LEN {
            return Err(IdParseError::Length {
                actual: bytes.len(),
            });
        }
        let mut digest = [0u8; ID_BYTES];
        for (index, pair) in bytes.chunks_exact(2).enumerate() {
            let high = hex_digit(pair[0]).ok_or(IdParseError::NotHex {
                offset: index * 2,
                found: char::from(pair[0]),
            })?;
            let low = hex_digit(pair[1]).ok_or(IdParseError::NotHex {
                offset: index * 2 + 1,
                found: char::from(pair[1]),
            })?;
            digest[index] = (high << 4) | low;
        }
        Ok(Self(digest))
    }
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A streaming object-identity hasher.
///
/// Feed the envelope's fields, then the value, in this order:
///
/// ```
/// use cas_core::{ObjectId, ObjectIdHasher, Role};
///
/// let value = b"hello";
/// let mut stream = ObjectIdHasher::start();
/// stream.update_envelope(Role::Bytes, value.len()).unwrap();
/// stream.update_value(&value[..2]);
/// stream.update_value(&value[2..]);
/// assert_eq!(stream.finish().unwrap(), ObjectId::hash(Role::Bytes, value).unwrap());
/// ```
///
/// [`update_envelope`](Self::update_envelope) may be called only once and
/// before any value bytes; [`update_value`](Self::update_value) may be called
/// any number of times. The value bytes fed must total exactly the length given
/// to `update_envelope`; [`finish`](Self::finish) checks that and returns
/// [`EnvelopeError::ValueLengthMismatch`] instead of an identity when they do
/// not, so a truncated or over-long stream can never produce an identity. No
/// whole-object buffer is retained, so the memory used is the digest state plus
/// the caller's chunk.
#[derive(Debug)]
pub struct ObjectIdHasher {
    hasher: Hasher,
    declared: Option<usize>,
    fed: usize,
}

impl ObjectIdHasher {
    /// Starts a hasher with the domain separator already mixed in.
    #[must_use]
    pub fn start() -> Self {
        let mut hasher = Hasher::new();
        hasher.update(DOMAIN);
        Self {
            hasher,
            declared: None,
            fed: 0,
        }
    }

    /// Mixes in the envelope fields: magic, version, role and both lengths.
    ///
    /// `value_len` is the total length of the value that
    /// [`update_value`](Self::update_value) will supply.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError::TooLarge`] when `value_len` would put the framed
    /// object past [`MAX_OBJECT_LEN`]. Calling it twice is a programming error
    /// and panics, because the second header would be hashed into the same
    /// stream and no valid identity could result.
    pub fn update_envelope(&mut self, role: Role, value_len: usize) -> Result<(), EnvelopeError> {
        assert!(
            self.declared.is_none(),
            "update_envelope may be called only once per ObjectIdHasher"
        );
        let envelope = crate::Envelope {
            role,
            value_len,
            payload_len: 0,
        };
        // `checked_add` cannot fail on a 64-bit target (`usize` cannot exceed
        // `u64`, and adding the 14-byte header cannot overflow `u64` for any
        // value `usize` can hold), so the error arm is defensive and
        // unreachable there. It is kept rather than removed because a 128-bit
        // target would make it live, and the annotation stops anyone reading it
        // as a covered branch.
        let len = (crate::ENVELOPE_HEADER_LEN as u64)
            .checked_add(value_len as u64)
            .ok_or(EnvelopeError::TooLarge {
                declared: u64::MAX,
                limit: MAX_OBJECT_LEN,
            })?;
        if len > MAX_OBJECT_LEN {
            return Err(EnvelopeError::TooLarge {
                declared: len,
                limit: MAX_OBJECT_LEN,
            });
        }
        self.hasher.update(&envelope.header_bytes());
        self.declared = Some(value_len);
        Ok(())
    }

    /// Mixes in the next slice of the object's value.
    ///
    /// # Panics
    ///
    /// Panics when called before [`update_envelope`](Self::update_envelope),
    /// which would hash value bytes ahead of the header they belong to.
    pub fn update_value(&mut self, bytes: &[u8]) {
        assert!(
            self.declared.is_some(),
            "update_envelope must be called before update_value"
        );
        self.hasher.update(bytes);
        self.fed += bytes.len();
    }

    /// Finishes the hash and returns the object's identity.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError::ValueLengthMismatch`] when the value bytes fed
    /// do not total the length declared to
    /// [`update_envelope`](Self::update_envelope), and
    /// [`EnvelopeError::Truncated`] when no envelope was declared at all.
    pub fn finish(self) -> Result<ObjectId, EnvelopeError> {
        let declared = self.declared.ok_or(EnvelopeError::Truncated {
            needed: crate::ENVELOPE_HEADER_LEN,
            actual: 0,
        })?;
        if self.fed != declared {
            return Err(EnvelopeError::ValueLengthMismatch {
                declared,
                fed: self.fed,
            });
        }
        Ok(ObjectId(*self.hasher.finalize().as_bytes()))
    }
}
