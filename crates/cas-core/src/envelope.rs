//! The canonical object envelope: framing, validation and decoding.
//!
//! Layout of every version-1 object, big-endian throughout:
//!
//! ```text
//! offset  size  field
//!      0     4  magic "WCAS"
//!      4     1  version
//!      5     1  role tag
//!      6     4  payload_len
//!     10     4  value_len
//!     14     n  value
//! ```
//!
//! `payload_len` is reserved for the Phase 1 content layout, where an object's
//! value may be accompanied by raw payload bytes in the same physical file. It
//! is part of the identity and is validated against the bytes present, but
//! nothing in this crate ever writes a non-zero value: [`Object::new`] and
//! [`Object::frame`] always produce `0`, and no later task may start writing a
//! non-zero one without an ADR and a format-version change.

use crate::error::EnvelopeError;
use crate::id::ObjectId;
use crate::role::Role;

/// The four bytes at the start of every object.
pub const MAGIC: [u8; 4] = *b"WCAS";

/// The only envelope version this crate writes or accepts.
pub const VERSION: u8 = 1;

/// Size of the fixed part that precedes the value.
pub const ENVELOPE_HEADER_LEN: usize = 4 + 1 + 1 + 4 + 4;

/// Largest accepted value, in bytes.
pub const MAX_VALUE_LEN: usize = 16 * 1024 * 1024;

/// Largest accepted framed object, in bytes: header plus value.
pub const MAX_OBJECT_LEN: u64 = (ENVELOPE_HEADER_LEN + MAX_VALUE_LEN) as u64;

/// The fixed envelope of one object.
///
/// An `Envelope` is the header alone. It is what a reader can hand back without
/// decoding the value, and what a streaming writer fills in before supplying
/// value bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Envelope {
    /// What the value means.
    pub role: Role,
    /// Length of the value that follows the header.
    pub value_len: usize,
    /// Length of the raw payload that follows the value; always 0 in version 1.
    pub payload_len: u32,
}

impl Envelope {
    /// The canonical header bytes.
    pub(crate) fn header_bytes(self) -> [u8; ENVELOPE_HEADER_LEN] {
        let mut header = [0u8; ENVELOPE_HEADER_LEN];
        header[0..4].copy_from_slice(&MAGIC);
        header[4] = VERSION;
        header[5] = self.role.tag();
        header[6..10].copy_from_slice(&self.payload_len.to_be_bytes());
        header[10..14].copy_from_slice(&(self.value_len as u32).to_be_bytes());
        header
    }

    /// Total framed length, header plus value plus payload.
    #[must_use]
    pub fn framed_len(&self) -> u64 {
        ENVELOPE_HEADER_LEN as u64 + self.value_len as u64 + u64::from(self.payload_len)
    }
}

/// One complete, validated object.
///
/// Holding an `Object` is an assertion that its bytes satisfy every version-1
/// rule: correct magic and version, a known role, lengths that agree with the
/// bytes present, and no trailing bytes. The only ways to obtain one are
/// [`Object::new`], [`Object::decode`] and [`Object::from_parts`], each of
/// which checks before returning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Object<'a> {
    role: Role,
    value: &'a [u8],
}

impl<'a> Object<'a> {
    /// Frames `value` under `role`.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError::TooLarge`] when the framed object would exceed
    /// [`MAX_OBJECT_LEN`].
    pub fn new(role: Role, value: &'a [u8]) -> Result<Self, EnvelopeError> {
        Self::from_parts(role, value, 0)
    }

    /// Frames `value` under `role` with a raw payload length of `payload_len`.
    ///
    /// Version 1 pins `payload_len` to zero, so a non-zero value is refused
    /// rather than accepted and then silently dropped: an object framed with it
    /// would hash as if the payload were absent. A future content layout that
    /// writes payload bytes must raise the format version first (ADR-001 D11).
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError::NonZeroPayload`] for a non-zero `payload_len`,
    /// [`EnvelopeError::TooLarge`] when the framed object would exceed
    /// [`MAX_OBJECT_LEN`] or when `value` exceeds [`MAX_VALUE_LEN`].
    pub fn from_parts(
        role: Role,
        value: &'a [u8],
        payload_len: u32,
    ) -> Result<Self, EnvelopeError> {
        if payload_len != 0 {
            return Err(EnvelopeError::NonZeroPayload { found: payload_len });
        }
        if value.len() > MAX_VALUE_LEN {
            return Err(EnvelopeError::TooLarge {
                declared: value.len() as u64,
                limit: MAX_VALUE_LEN as u64,
            });
        }
        let envelope = Envelope {
            role,
            value_len: value.len(),
            payload_len,
        };
        let framed = envelope.framed_len();
        if framed > MAX_OBJECT_LEN {
            return Err(EnvelopeError::TooLarge {
                declared: framed,
                limit: MAX_OBJECT_LEN,
            });
        }
        Ok(Self { role, value })
    }

    /// What the value means.
    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    /// The value bytes.
    #[must_use]
    pub const fn value(&self) -> &'a [u8] {
        self.value
    }

    /// This object's envelope.
    #[must_use]
    pub fn envelope(&self) -> Envelope {
        Envelope {
            role: self.role,
            value_len: self.value.len(),
            payload_len: 0,
        }
    }

    /// This object's identity, recomputed from its canonical bytes.
    ///
    /// # Errors
    ///
    /// Propagates [`EnvelopeError::TooLarge`] from the size checks.
    pub fn id(&self) -> Result<ObjectId, EnvelopeError> {
        ObjectId::hash(self.role, self.value)
    }

    /// Writes the canonical frame into `out` and returns the number of bytes
    /// written.
    ///
    /// `out` must be at least [`Envelope::framed_len`] bytes. The frame is
    /// exactly the bytes whose hash is the object's identity.
    pub fn frame(&self, out: &mut [u8]) -> usize {
        let envelope = self.envelope();
        let header = envelope.header_bytes();
        let len = envelope.framed_len() as usize;
        assert!(
            out.len() >= len,
            "frame buffer of {} bytes cannot hold a {len}-byte object",
            out.len()
        );
        out[..ENVELOPE_HEADER_LEN].copy_from_slice(&header);
        out[ENVELOPE_HEADER_LEN..len].copy_from_slice(self.value);
        len
    }

    /// Allocates the canonical frame.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.envelope().framed_len() as usize];
        let written = self.frame(&mut out);
        debug_assert_eq!(written, out.len());
        out
    }

    /// Decodes one complete canonical object from `bytes`.
    ///
    /// The input must be exactly one object: no leading bytes, no trailing
    /// bytes, and no missing value bytes.
    ///
    /// # Errors
    ///
    /// Returns the first rule that fails, in this order: header length, magic,
    /// version, role, non-zero `payload_len`
    /// ([`EnvelopeError::NonZeroPayload`]), declared size against
    /// [`MAX_OBJECT_LEN`], and declared length against the bytes present
    /// (which rejects both a truncated value and trailing bytes).
    pub fn decode(bytes: &'a [u8]) -> Result<Self, EnvelopeError> {
        let envelope = Self::read_header(bytes)?;
        let total = envelope.framed_len();
        if total != bytes.len() as u64 {
            return Err(EnvelopeError::LengthMismatch {
                declared: total,
                actual: bytes.len() as u64,
            });
        }
        let value_start = ENVELOPE_HEADER_LEN;
        let value_end = value_start + envelope.value_len;
        Ok(Self {
            role: envelope.role,
            value: &bytes[value_start..value_end],
        })
    }

    /// Reads and checks the header of a frame, without requiring the value to
    /// be present.
    ///
    /// # Errors
    ///
    /// The same header rules as [`Object::decode`].
    pub fn read_header(bytes: &[u8]) -> Result<Envelope, EnvelopeError> {
        if bytes.len() < ENVELOPE_HEADER_LEN {
            return Err(EnvelopeError::Truncated {
                needed: ENVELOPE_HEADER_LEN,
                actual: bytes.len(),
            });
        }
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&bytes[0..4]);
        if magic != MAGIC {
            return Err(EnvelopeError::BadMagic { found: magic });
        }
        let version = bytes[4];
        if version != VERSION {
            return Err(EnvelopeError::UnsupportedVersion { found: version });
        }
        let role =
            Role::from_tag(bytes[5]).ok_or(EnvelopeError::UnknownRole { found: bytes[5] })?;
        let payload_len = u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
        let value_len = u32::from_be_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]);
        if payload_len != 0 {
            return Err(EnvelopeError::NonZeroPayload { found: payload_len });
        }
        let envelope = Envelope {
            role,
            value_len: value_len as usize,
            payload_len,
        };
        if envelope.framed_len() > MAX_OBJECT_LEN {
            return Err(EnvelopeError::TooLarge {
                declared: envelope.framed_len(),
                limit: MAX_OBJECT_LEN,
            });
        }
        Ok(envelope)
    }
}

/// Recomputes the identity of a framed object and compares it with `expected`.
///
/// This is the read-path check: an object whose bytes no longer hash to the id
/// it was stored under is corrupt, and the caller must report that rather than
/// use the bytes.
///
/// # Errors
///
/// Returns the decode error when `bytes` is not a valid version-1 object, then
/// [`EnvelopeError::IdentityMismatch`] when it decodes but hashes to something
/// other than `expected`.
pub fn verify_id(expected: ObjectId, bytes: &[u8]) -> Result<Object<'_>, EnvelopeError> {
    let object = Object::decode(bytes)?;
    let actual = object.id()?;
    if actual != expected {
        return Err(EnvelopeError::IdentityMismatch { expected, actual });
    }
    Ok(object)
}
