//! `error` tests (P3-1 §1): the mapping is exhaustive by construction, and its tables cannot drift silently.

use fs_windows::error::{to_status, Failure, NtStatus, ALL_FAILURES, ALL_STATUSES};

/// The cardinality that makes the "compile-time guarantee" actually load-bearing.
///
/// `to_status`'s `match` is exhaustive, so a new [`Failure`] variant will not compile — but only *if* the
/// variant list used by the tests also grows. The literal counts below are what makes adding a variant a
/// visible, reviewable change to **this** file too, instead of a variant that silently never gets tested.
#[test]
fn the_variant_lists_have_literal_cardinalities() {
    assert_eq!(ALL_FAILURES.len(), 16, "sixteen typed failures");
    assert_eq!(
        ALL_STATUSES.len(),
        17,
        "seventeen NTSTATUS values, including Success"
    );
}

/// Every failure maps to an error status, and no two failures are mapped to the same status by accident: a
/// duplicate would make two different problems indistinguishable in a log.
#[test]
fn every_failure_maps_to_a_distinct_error_status() {
    let mut seen = std::collections::BTreeMap::new();
    for failure in ALL_FAILURES {
        let status = to_status(failure);
        assert!(
            status.is_error(),
            "cell failure={failure:?}: {status:?} is not an error status"
        );
        assert!(
            seen.insert(status, failure).is_none(),
            "cell failure={failure:?}: {status:?} is already used by {:?}",
            seen.get(&status)
        );
    }
    assert_eq!(
        seen.len(),
        16,
        "sixteen distinct statuses for sixteen failures"
    );
}

/// Codes and names are unique, and every name is a `STATUS_*` constant name (card §2.5: never assert on a
/// bare number, always on the name).
#[test]
fn codes_and_names_are_unique_and_well_formed() {
    let mut codes = std::collections::BTreeSet::new();
    let mut names = std::collections::BTreeSet::new();
    for status in ALL_STATUSES {
        assert_eq!(
            status.code() & 0xFFFF_0000,
            match status {
                NtStatus::Success => 0,
                _ => 0xC000_0000,
            },
            "cell status={status:?}: the severity bits must follow the NTSTATUS convention"
        );
        assert!(
            codes.insert(status.code()),
            "cell status={status:?}: duplicate code"
        );
        assert!(
            names.insert(status.name()),
            "cell status={status:?}: duplicate name"
        );
        assert!(
            status.name().starts_with("STATUS_"),
            "cell status={status:?}: {:?} is not a STATUS_ constant name",
            status.name()
        );
    }
    assert_eq!(codes.len(), 17, "seventeen distinct codes");
    assert_eq!(names.len(), 17, "seventeen distinct names");
}

/// `Success` is the only non-error value this model can return as a status.
#[test]
fn success_is_the_only_success_status() {
    let successes: Vec<NtStatus> = ALL_STATUSES
        .into_iter()
        .filter(|status| !status.is_error())
        .collect();
    assert_eq!(successes, vec![NtStatus::Success], "cell severity=success");
    assert_ne!(
        to_status(Failure::Internal),
        NtStatus::Success,
        "cell failure=Internal: an internal failure is never a success"
    );
}
