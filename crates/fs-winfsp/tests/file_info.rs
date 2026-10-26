//! `GetFileInfo`'s body: three claims, three different mistakes.
//!
//! 1. the timestamps are **FILETIME** (1601 epoch), so a round trip through the SDK helper must return them unchanged;
//! 2. `AllocationSize` is a **rounded** size, so a 1-byte file does not look sparse;
//! 3. attributes are **pass-through** — the adapter already reports derived bits, and re-deriving here would put the
//!    `DIRECTORY`/`NORMAL` decision in two places.

use fs_windows::time;
use fs_winfsp::file_info::{allocation_size, file_info, ALLOCATION_UNIT};
use fs_winfsp::{Attributes, Kind, NodeInfo};

fn node(kind: Kind, size: u64, attributes: u32) -> NodeInfo {
    NodeInfo {
        file_id: 7,
        kind,
        size,
        // 2026-10-26T00:00:00Z-ish: an ordinary value, well inside the FILETIME range.
        times: fs_winfsp::file_info::times(
            1_793_000_000_000_000_000,
            1_793_000_001_000_000_000,
            1_793_000_002_000_000_000,
            1_793_000_003_000_000_000,
        ),
        attributes: Attributes(attributes),
    }
}

#[test]
fn timestamps_are_filetime_and_round_trip() {
    let info = file_info(&node(Kind::File, 10, 0x80)).expect("info");
    for (filetime, unix) in [
        (info.creation_time, 1_793_000_000_000_000_000i64),
        (info.last_access_time, 1_793_000_001_000_000_000),
        (info.last_write_time, 1_793_000_002_000_000_000),
        (info.change_time, 1_793_000_003_000_000_000),
    ] {
        assert_eq!(
            time::filetime_to_unix_ns(filetime).expect("back"),
            unix,
            "cell round trip: the value crossed the 1601 boundary through the shared helper, not through local arithmetic"
        );
    }
    // A Unix-epoch timestamp must not be reported as-is: it would be 1601 in FILETIME terms.
    assert!(
        info.creation_time > 0,
        "cell epoch: FILETIME counts from 1601, so a 2026 timestamp is a large positive"
    );
}

#[test]
fn allocation_size_is_rounded_up_not_exact() {
    for (size, expected) in [
        (0u64, 0u64),
        (1, ALLOCATION_UNIT),
        (ALLOCATION_UNIT, ALLOCATION_UNIT),
        (ALLOCATION_UNIT + 1, 2 * ALLOCATION_UNIT),
    ] {
        assert_eq!(
            allocation_size(size),
            expected,
            "cell size {size}: allocation rounds up to the unit — reporting the exact size would make ordinary files look sparse"
        );
    }
    let info = file_info(&node(Kind::File, 1, 0x80)).expect("info");
    assert_eq!(info.file_size, 1, "cell file size: exact");
    assert_eq!(
        info.allocation_size, ALLOCATION_UNIT,
        "cell allocation: rounded"
    );
}

#[test]
fn attributes_are_passed_through_not_re_derived() {
    let info = file_info(&node(Kind::Directory, 0, 0x12)).expect("info");
    assert_eq!(
        info.file_attributes, 0x12,
        "cell pass-through: the adapter already reported derived bits; re-deriving here would decide DIRECTORY/NORMAL in two places"
    );
    assert_eq!(
        info.index_number, 7,
        "cell index: the FileId is what NTFS calls FileIndex"
    );
    assert_eq!(
        info.hard_links, 0,
        "cell hard links: unimplemented on the SDK side, so 0 by contract"
    );
}
