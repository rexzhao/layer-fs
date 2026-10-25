//! `length` tests (P3-1 §1, KI-9 family): boundaries near `u64::MAX`, EOF rules, and zeroing on extend.

use fs_windows::error::Failure;
use fs_windows::length::{plan_truncate, read_range, Truncate};

#[test]
fn a_range_that_overflows_is_refused() {
    assert_eq!(
        read_range(u64::MAX, 2, u64::MAX),
        Err(Failure::InvalidParameter),
        "cell offset=u64::MAX len=2: offset + len overflows"
    );
    assert_eq!(
        read_range(u64::MAX - 1, 1, u64::MAX),
        Ok(fs_windows::length::Range {
            start: u64::MAX - 1,
            length: 1
        }),
        "cell offset=u64::MAX-1 len=1: exactly fits"
    );
}

#[test]
fn a_read_is_clamped_to_eof_but_a_read_at_eof_is_end_of_file() {
    assert_eq!(
        read_range(0, 100, 10),
        Ok(fs_windows::length::Range {
            start: 0,
            length: 10
        }),
        "cell offset=0 len=100 size=10: clamped to the file"
    );
    assert_eq!(
        read_range(10, 1, 10),
        Err(Failure::EndOfFile),
        "cell offset=size: reading at EOF is EOF, not a short read"
    );
    assert_eq!(
        read_range(11, 1, 10),
        Err(Failure::EndOfFile),
        "cell offset>size"
    );
    assert_eq!(
        read_range(9, 100, 10),
        Ok(fs_windows::length::Range {
            start: 9,
            length: 1
        }),
        "cell offset=size-1 len=100: one byte"
    );
}

/// Extending a file **must record the zero range**: KI-9's bug family is "the gap kept stale bytes".
#[test]
fn extending_records_the_range_that_must_be_zeroed() {
    assert_eq!(
        plan_truncate(10, 20),
        Ok(Truncate::Extend {
            zero_from: 10,
            zero_len: 10
        }),
        "cell old=10 new=20"
    );
    assert_eq!(
        plan_truncate(10, 10),
        Ok(Truncate::Unchanged),
        "cell old==new"
    );
    assert_eq!(
        plan_truncate(20, 10),
        Ok(Truncate::Shrink { keep: 10 }),
        "cell old=20 new=10: the bytes past `keep` are dropped, nothing is zeroed"
    );
}
