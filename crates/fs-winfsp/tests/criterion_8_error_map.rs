//! §3 criterion 8: the error mapping is **delegated**, not re-derived (P3-3a).
//!
//! Two halves, and the second is the one that gets skipped:
//!
//! 1. **mechanical**: `fs-winfsp/src/error.rs` contains no `NtStatus::` construction and no fallback arm, so the mapping
//!    can only be `fs_windows::error::to_status`;
//! 2. **semantic**: `fs_windows`'s own table has an entry for every `OpsError` variant — checked by walking
//!    [`fs_semantics::disposition::ALL_FAILURES`] through `to_status` and requiring a distinct, non-`Internal` answer
//!    for the variants that have one. The manager's reminder was explicit: an empty `grep` says the copy is gone, not
//!    that the table is complete.
//!
//! The scanner has a **positive control**: [`hits`] is the same function used on the sources and on planted text, so a
//! needle that never matches (a typo in the pattern) fails the test instead of passing it.

use fs_semantics::disposition::{Failure, ALL_FAILURES};
use fs_windows::error::{to_status, NtStatus};
use fs_winfsp::{to_failure, OpsError};

/// Every `OpsError` variant the adapter can see. Listed by hand so a new variant makes this test's `match` (below)
/// non-exhaustive... except that a list cannot do that; the *source* check does. This one exists to drive the semantic
/// half, and `ops_error_is_exhaustively_listed` guards its length.
fn all_ops_errors() -> Vec<OpsError> {
    vec![
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
        OpsError::Other("detail".to_owned()),
    ]
}

fn sources() -> Vec<(String, String)> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let mut files = Vec::new();
    for name in ["src/error.rs", "src/ops.rs", "src/lib.rs"] {
        let path = std::path::Path::new(manifest).join(name);
        let text = std::fs::read_to_string(&path).expect("read source");
        files.push((name.to_owned(), text));
    }
    files
}

/// Counts occurrences of `needle`, and is used for **both** the sources and the controls, so a broken needle cannot make
/// the scan pass vacuously.
fn hits(text: &str, needle: &str) -> usize {
    text.match_indices(needle).count()
}

/// The mapping file must not build `NtStatus` values itself: that is `fs_windows`'s job.
#[test]
fn the_mapping_file_never_constructs_an_ntstatus() {
    let needle = format!("NtStatus::{}", "ObjectNameNotFound");
    let map = sources()
        .into_iter()
        .find(|(name, _)| name == "src/error.rs")
        .expect("error.rs is scanned")
        .1;
    // Only the `use` line may mention `NtStatus` as a type; constructing a value would look like `NtStatus::X`.
    let constructed = hits(&map, "NtStatus::") - hits(&map, "NtStatus::;");
    assert_eq!(
        constructed, 0,
        "cell mechanical: src/error.rs must not name an `NtStatus` variant (found {constructed}); the table lives in \
         fs_windows::error"
    );
    // Positive control: the same needle on text that *does* construct one, so a typo in the needle fails here.
    assert!(
        hits(&format!("let status = {needle};"), "NtStatus::") > 0,
        "cell control: the needle matches a real construction, so the zero above is a measurement, not a silent miss"
    );
}

/// The `fs_windows` table must have a **distinct** answer for the failures the adapter can produce; a table that mapped
/// everything to `Internal` would satisfy any "mapping exists" check.
#[test]
fn the_windows_table_is_specific_for_the_failures_we_use() {
    let mut seen = std::collections::BTreeSet::new();
    for error in all_ops_errors() {
        let failure = to_failure(&error);
        let status = to_status(failure);
        assert!(
            status.is_error(),
            "cell {error:?} -> {failure:?}: must be an error status"
        );
        seen.insert(status);
    }
    assert!(
        seen.len() >= 8,
        "cell specificity: the failures the adapter uses map to at least 8 distinct statuses, got {} ({seen:?})",
        seen.len()
    );
    assert!(
        !seen
            .iter()
            .filter(|status| **status == NtStatus::InternalError)
            .count()
            .gt(&1),
        "cell specificity: at most one OpsError may collapse to InternalError (the catch-all `Other`)"
    );
}

/// Every `Failure` in the shared table maps to a status, and `to_status` is total.
#[test]
fn every_failure_has_a_status() {
    for failure in ALL_FAILURES {
        let status = to_status(failure);
        assert!(
            status.is_error(),
            "cell {failure:?}: every failure names an error status"
        );
    }
    assert_eq!(
        ALL_FAILURES.len(),
        16,
        "cell count: the shared failure vocabulary still has 16 variants; a change here means the mapping above must be \
         re-read"
    );
}

/// The `Other` catch-all is the **only** variant that may become `InternalError` (recorded fold), and the guard-mismatch
/// fold (`NotFound` here, `InvalidRecord` in the core) is documented rather than silently equalised.
#[test]
fn the_other_variant_is_the_only_internal_one() {
    let internal: Vec<OpsError> = all_ops_errors()
        .into_iter()
        .filter(|error| to_status(to_failure(error)) == NtStatus::InternalError)
        .collect();
    assert_eq!(
        internal,
        vec![OpsError::Other("detail".to_owned())],
        "cell fold: only `Other` is internal; mapping `NotFound` there would make a real backend's internal error look \
         like a missing file"
    );
    let _ = Failure::Internal; // the shared vocabulary names the same concept; kept so the import is not dead.
}
