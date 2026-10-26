//! `OpsError` → `NTSTATUS` (P3-3a card §3 criterion 8).
//!
//! The rule is the same one that put the disposition table in `fs-semantics`: **one implementation**. The table itself is
//! `fs_windows::error::to_status`, and this module only decides which [`Failure`] an [`OpsError`] denotes. Nothing here
//! may re-derive an `NtStatus` from first principles — a second copy of "`NotFound` is `STATUS_OBJECT_NAME_NOT_FOUND`"
//! would drift, and the drift would only show up on a real mount.
//!
//! # The one fold, recorded
//!
//! [`OpsError`] has no `InvalidRecord`, because the mock's namespace cannot produce one; the core's `remove_at` returns
//! `NamespaceError::InvalidRecord` where the mock returns `NotFound`. That means the guard-mismatch case reaches the
//! adapter as `NotFound` here and as `InvalidRecord` in P3-3b — and those map to **different** `NTSTATUS` values
//! (`LeafMissing` vs `Internal`). It is recorded as an open question for P3-3b rather than silently "fixed" by mapping
//! `InvalidRecord`-shaped failures to `NotFound`, because that would make a real backend's internal error look like a
//! missing file.
//!
//! A second, smaller fold: [`OpsError::OutOfRange`] maps to `InvalidParameter`. Windows has no distinct "offset beyond
//! end of file" NTSTATUS for a read (a short read is the normal answer), so an out-of-range request is a caller bug.

use fs_semantics::disposition::Failure;
use fs_windows::error::{to_status, NtStatus};

use crate::ops::OpsError;

/// Which [`Failure`] an [`OpsError`] denotes.
///
/// **Exhaustive on purpose**, no fallback arm: a new `OpsError` variant must come here and be given a Windows answer, or
/// the build fails. `tests/criterion_8_error_map.rs` scans this file so the fallback cannot reappear in a later edit.
#[must_use]
pub const fn to_failure(error: &OpsError) -> Failure {
    match error {
        OpsError::NotFound => Failure::LeafMissing,
        OpsError::NameCollision => Failure::NameCollision,
        OpsError::TypeMismatch => Failure::NameCollision,
        OpsError::NotEmpty => Failure::DirectoryNotEmpty,
        OpsError::NotADirectory => Failure::NotADirectory,
        OpsError::IsADirectory => Failure::IsADirectory,
        OpsError::ReadOnly => Failure::AccessDenied,
        OpsError::OutOfRange => Failure::InvalidParameter,
        OpsError::InvalidParameter => Failure::InvalidParameter,
        OpsError::SharingViolation => Failure::SharingViolation,
        OpsError::DeletePending => Failure::DeletePending,
        OpsError::AccessDenied => Failure::AccessDenied,
        // The core's `InvalidRecord` maps to `Internal` (see `fs_namespace::failure_for`); the adapter's variant carries
        // the same meaning, so it lands on the same status instead of pretending the entry is missing.
        OpsError::InvalidRecord(_) => Failure::Internal,
        OpsError::Other(_) => Failure::Internal,
    }
}

/// The `NTSTATUS` the adapter reports for `error`.
#[must_use]
pub const fn to_status_of(error: &OpsError) -> NtStatus {
    to_status(to_failure(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant is reachable from the mapping, so the table cannot silently shrink when a variant is renamed.
    #[test]
    fn every_ops_error_has_an_answer() {
        let all = [
            OpsError::NotFound,
            OpsError::NameCollision,
            OpsError::TypeMismatch,
            OpsError::NotEmpty,
            OpsError::NotADirectory,
            OpsError::IsADirectory,
            OpsError::ReadOnly,
            OpsError::OutOfRange,
            OpsError::InvalidParameter,
            OpsError::SharingViolation,
            OpsError::DeletePending,
            OpsError::AccessDenied,
            OpsError::InvalidRecord("guard mismatch".to_owned()),
            OpsError::Other("x".to_owned()),
        ];
        for error in all {
            let status = to_status_of(&error);
            assert!(
                status.is_error(),
                "cell {error:?}: every OpsError maps to an error status, never Success"
            );
        }
    }
}
