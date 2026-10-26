//! Which attribute bits may be **stored** (P3-2 G3; ADR-005-v3 draft §3).
//!
//! This is here rather than in the core for the same reason the disposition table is: the Windows frontend must refuse
//! derived bits **without** pulling SQLite into its dependency closure, and two copies of the rule (one per layer) is how
//! the two layers end up disagreeing. `fs-namespace` re-exports these names, so core code and frontend code read the same
//! numbers from the same place.

use crate::disposition::Failure;

/// `FILE_ATTRIBUTE_DIRECTORY`: derived from the entry's kind, never stored.
pub const DIRECTORY: u32 = 0x10;

/// `FILE_ATTRIBUTE_NORMAL`: derived ("no other bit set"), never stored.
pub const NORMAL: u32 = 0x80;

/// The bits that may be stored: `READONLY`, `HIDDEN`, `SYSTEM`, `ARCHIVE`.
pub const WRITABLE: u32 = 0x01 | 0x02 | 0x04 | 0x20;

/// Checks that `stored` is inside the storable value domain.
///
/// A derived bit is a refusal rather than a no-op: a caller that "set `DIRECTORY`" on a file would otherwise believe it
/// had changed the kind of an entry, and silent success on a request that cannot be honoured is worse than a rejection.
///
/// # Errors
///
/// [`Failure::InvalidParameter`] when a derived or unknown bit is present.
pub const fn validate(stored: u32) -> Result<(), Failure> {
    if stored & (DIRECTORY | NORMAL) != 0 {
        return Err(Failure::InvalidParameter);
    }
    if stored & !WRITABLE != 0 {
        return Err(Failure::InvalidParameter);
    }
    Ok(())
}

/// The attribute bits a frontend reports for `kind` given the stored bits.
///
/// Storage stays minimal (four writable bits), reporting is complete (`DIRECTORY`/`NORMAL` synthesised): the two are
/// faces of one decision, not a compromise between them.
#[must_use]
pub const fn report(kind_is_directory: bool, stored: u32) -> u32 {
    if kind_is_directory {
        stored | DIRECTORY
    } else if stored == 0 {
        NORMAL
    } else {
        stored
    }
}
