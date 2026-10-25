//! File attributes (P3-1 §1 `attrs`, ADR-005 §3).
//!
//! Attributes are **not persisted** by this model: the P3 frontend accepts `SetFileAttributes`, reports a
//! documented constant set, and refuses the bits that are actually derived from the entry's kind. The two
//! mistakes this module exists to prevent are both about *equivalence*:
//!
//! - persisting `DIRECTORY` or `NORMAL` would let one state be written two ways, so the "what is this entry?"
//!   question would have two answers that must be kept in sync;
//! - reporting `NORMAL` alongside other bits would do the same for the read direction — Windows defines
//!   `NORMAL` as "no other attribute is set", so it is a *derived* report, never a stored bit.

use crate::error::Failure;

/// The four bits this model accepts on a write.
pub const STORABLE: [u32; 4] = [
    FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_SYSTEM,
    FILE_ATTRIBUTE_ARCHIVE,
];

/// Read-only.
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
/// Hidden.
pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
/// System.
pub const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0000_0004;
/// Directory — **derived**, never stored.
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
/// Archive.
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
/// Normal — **derived**: reported only when nothing else is set.
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;

/// What the entry is; `DIRECTORY` follows from this, not from a stored bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
}

impl Kind {
    /// Both kinds, for the completeness walk.
    pub const ALL: [Kind; 2] = [Kind::File, Kind::Directory];
}

/// The attribute word reported for an entry.
#[must_use]
pub fn report(kind: Kind, storable: u32) -> u32 {
    let mut word = storable & STORABLE.iter().copied().fold(0, |acc, bit| acc | bit);
    if kind == Kind::Directory {
        word |= FILE_ATTRIBUTE_DIRECTORY;
    }
    // NORMAL means "nothing else is set" — including DIRECTORY, which is why the check comes after it.
    if word == 0 {
        word = FILE_ATTRIBUTE_NORMAL;
    }
    word
}

/// Validates a requested attribute word and returns the part this model will keep.
///
/// Unknown bits are **refused** rather than ignored: a caller that asks for `ENCRYPTED` must learn that this
/// frontend will not encrypt, instead of believing it did.
///
/// # Errors
///
/// [`Failure::AccessDenied`] for `DIRECTORY`/`NORMAL` (derived bits, P3-1 §2.4: not supported), and
/// [`Failure::InvalidParameter`] for any other bit outside [`STORABLE`].
pub fn accept(requested: u32) -> Result<u32, Failure> {
    let derived = requested & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_NORMAL);
    if derived != 0 {
        return Err(Failure::AccessDenied);
    }
    let storable_mask = STORABLE.iter().copied().fold(0, |acc, bit| acc | bit);
    if requested & !storable_mask != 0 {
        return Err(Failure::InvalidParameter);
    }
    Ok(requested)
}
