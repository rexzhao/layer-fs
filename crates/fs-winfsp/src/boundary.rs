//! The FFI boundary's two rules, in one place (P3-4 card §2): UTF-16 crosses exactly once, and no panic crosses at all.
//!
//! Both are testable **without** WinFsp, which is why they live here rather than in the `extern "C"` layer: the boundary
//! rules are the part that is easy to get subtly wrong and hard to observe through a mounted volume, so they are pinned
//! where a unit test can fail on them.

use fs_windows::error::NtStatus;

/// Converts a NUL-terminated UTF-16 slice to UTF-8, stopping at the first NUL.
///
/// The adapter works in UTF-8 internally; every UTF-16 string is converted here, at the edge, so no interior code has to
/// reason about surrogate pairs.
///
/// # Errors
///
/// [`NtStatus::ObjectNameInvalid`] for an unpaired surrogate: Windows names are UTF-16 and a lone surrogate is not a name
/// this filesystem can represent — accepting it would store a byte sequence no other reader could reproduce.
pub fn utf16_to_utf8(units: &[u16]) -> Result<String, NtStatus> {
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    String::from_utf16(&units[..end]).map_err(|_| NtStatus::ObjectNameInvalid)
}

/// Converts a UTF-8 name to UTF-16 with a trailing NUL, the shape the API expects to fill.
///
/// # Errors
///
/// [`NtStatus::ObjectNameInvalid`] if the name contains a NUL: the receiving side would read it as the terminator and
/// silently truncate the name, which is worse than an error.
pub fn utf8_to_utf16(name: &str) -> Result<Vec<u16>, NtStatus> {
    if name.contains('\0') {
        return Err(NtStatus::ObjectNameInvalid);
    }
    let mut out: Vec<u16> = name.encode_utf16().collect();
    out.push(0);
    Ok(out)
}

/// Runs one FFI callback body, converting a panic into an error status.
///
/// A panic must never unwind across `extern "C"` — that is undefined behaviour, not an error code. `catch_unwind` turns it
/// into the same kind of failure a normal error produces, which means a bug in one callback surfaces as a failed operation
/// rather than as a corrupted process.
///
/// The `default` status is what a panic becomes; callers pass [`NtStatus::InternalError`].
pub fn guarded<F>(default: NtStatus, body: F) -> NtStatus
where
    F: FnOnce() -> NtStatus + std::panic::UnwindSafe,
{
    match std::panic::catch_unwind(body) {
        Ok(status) => status,
        Err(_) => default,
    }
}
