//! Offset/length arithmetic (P3-1 §1 `length`, same family as KI-9).
//!
//! Every arithmetic surface a caller can hand us is checked here, once: `offset + length` may overflow,
//! a write may extend past EOF (the gap must be zeroed, not left with stale bytes), and a read that starts at
//! or past EOF is `EndOfFile` rather than "zero bytes read" — the difference is what makes a truncated file
//! look alive to a caller that only checks the count.

use crate::error::Failure;

/// A resolved byte range inside a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// First byte.
    pub start: u64,
    /// Number of bytes.
    pub length: u64,
}

impl Range {
    /// One past the last byte.
    #[must_use]
    pub fn end(self) -> u64 {
        self.start + self.length
    }
}

/// Resolves a read request against the file size.
///
/// A read is clamped to EOF (Windows returns the short count, not an error) **except** when it starts at or
/// past EOF: that is `EndOfFile`, and returning zero bytes there would let a caller loop forever.
///
/// # Errors
///
/// [`Failure::InvalidParameter`] on overflow, [`Failure::EndOfFile`] when the start is at or past the size.
pub fn read_range(offset: u64, length: u64, size: u64) -> Result<Range, Failure> {
    let end = offset
        .checked_add(length)
        .ok_or(Failure::InvalidParameter)?;
    if offset >= size {
        return Err(Failure::EndOfFile);
    }
    let clamped_end = end.min(size);
    Ok(Range {
        start: offset,
        length: clamped_end - offset,
    })
}

/// What a `truncate` must do to reach `new_size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truncate {
    /// Nothing to do.
    Unchanged,
    /// Drop everything past `new_size`.
    Shrink { keep: u64 },
    /// Append zeros from `old_size` to `new_size`.
    ///
    /// The zeroing is not an optimisation detail: KI-9's family of bugs is "the file grew and the gap still
    /// holds whatever was in those blocks", which reads as data corruption to the user.
    Extend { zero_from: u64, zero_len: u64 },
}

/// Plans a truncate from `old_size` to `new_size`.
///
/// # Errors
///
/// [`Failure::InvalidParameter`] when the lengths overflow the arithmetic.
pub fn plan_truncate(old_size: u64, new_size: u64) -> Result<Truncate, Failure> {
    match new_size.cmp(&old_size) {
        std::cmp::Ordering::Equal => Ok(Truncate::Unchanged),
        std::cmp::Ordering::Less => Ok(Truncate::Shrink { keep: new_size }),
        std::cmp::Ordering::Greater => {
            let zero_len = new_size
                .checked_sub(old_size)
                .ok_or(Failure::InvalidParameter)?;
            Ok(Truncate::Extend {
                zero_from: old_size,
                zero_len,
            })
        }
    }
}
