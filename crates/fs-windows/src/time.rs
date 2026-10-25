//! FILETIME conversions (P3-1 §1 `time`, ADR-005 v3 §4/§9).
//!
//! FILETIME counts **100 ns since 1601-01-01 UTC**; the store counts **i64 nanoseconds since 1970**. Two
//! boundaries fall out of that and both are refusals rather than clamping: a timestamp before 1601 cannot be
//! represented, and one whose 100 ns value exceeds `u64` cannot either.
//!
//! **Why the arithmetic is `i128`** (a bug this module shipped for one commit): the 1601→1970 offset is
//! ~1.164e19 ns, which is **larger than `i64::MAX`** (~9.223e18 ns). Computing it in `i64` therefore failed for
//! *every* input, not just edge cases, and the earliest representable Unix instant is about 1677-09-21 — so
//! `FILETIME 0` has no `i64` counterpart at all. The lesson is the one this project keeps relearning: an
//! untested boundary does not give "mostly right", it gives "wrong everywhere". The conversion also **floors** to the
//! 100 ns tick, because rounding up would make a file look newer than the store says it is, and `robocopy`-style
//! tools react to that.

use crate::error::Failure;

/// Seconds from 1601-01-01 to 1970-01-01.
pub const FILETIME_EPOCH_OFFSET_SECS: i64 = 11_644_473_600;

/// Ticks (100 ns) per second.
pub const TICKS_PER_SECOND: i64 = 10_000_000;

/// Converts Unix nanoseconds to a FILETIME, flooring to the 100 ns tick.
///
/// # Errors
///
/// [`Failure::InvalidParameter`] when the instant is before 1601 or does not fit in 64 bits.
pub fn unix_ns_to_filetime(ns: i64) -> Result<u64, Failure> {
    // The 1601→1970 offset is 1.16e19 nanoseconds, which does **not** fit in i64 (max 9.2e18) — the reference
    // implementation that computed the offset in `i64` failed on every input. Do the arithmetic in `i128`,
    // where both the offset and the sum are exact, and only then check the u64 result.
    let since_1601 = i128::from(ns) + i128::from(FILETIME_EPOCH_OFFSET_SECS) * 1_000_000_000;
    if since_1601 < 0 {
        return Err(Failure::InvalidParameter);
    }
    let ticks = since_1601 / 100; // `i128` division truncates toward zero; the value is non-negative here
    u64::try_from(ticks).map_err(|_| Failure::InvalidParameter)
}

/// Converts a FILETIME back to Unix nanoseconds.
///
/// # Errors
///
/// [`Failure::InvalidParameter`] when the value is outside `i64` nanoseconds.
pub fn filetime_to_unix_ns(filetime: u64) -> Result<i64, Failure> {
    let ns = i128::from(filetime) * 100;
    let unix = ns - i128::from(FILETIME_EPOCH_OFFSET_SECS) * 1_000_000_000;
    i64::try_from(unix).map_err(|_| Failure::InvalidParameter)
}
