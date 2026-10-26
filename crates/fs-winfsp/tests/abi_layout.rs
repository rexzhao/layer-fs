//! Layout assertions for the hand-written WinFsp structures (P3-4 ruling (b)).
//!
//! These are the whole reason hand-writing was acceptable: a generator would follow a changed header silently, while an
//! assertion fails and says which field moved. The expected numbers come from the SDK's own
//! `FSP_FSCTL_STATIC_ASSERT`s plus the arithmetic written next to each one.

use fs_winfsp::abi::{FileInfo, VolumeInfo};
use std::mem::{offset_of, size_of};

/// `FSP_FSCTL_STATIC_ASSERT(72 == sizeof(FSP_FSCTL_FILE_INFO))` — `fsctl.h:291`.
///
/// The offsets are that size taken apart: four-byte pair at 0, then six eight-byte values (8..56), then the trailing
/// four-byte pair at 64..72.
#[test]
fn file_info_matches_the_sdk_layout() {
    assert_eq!(size_of::<FileInfo>(), 72, "cell size: fsctl.h:291");
    assert_eq!(offset_of!(FileInfo, file_attributes), 0);
    assert_eq!(offset_of!(FileInfo, reparse_tag), 4);
    assert_eq!(offset_of!(FileInfo, allocation_size), 8);
    assert_eq!(offset_of!(FileInfo, file_size), 16);
    assert_eq!(offset_of!(FileInfo, creation_time), 24);
    assert_eq!(offset_of!(FileInfo, last_access_time), 32);
    assert_eq!(offset_of!(FileInfo, last_write_time), 40);
    assert_eq!(offset_of!(FileInfo, change_time), 48);
    assert_eq!(offset_of!(FileInfo, index_number), 56);
    assert_eq!(offset_of!(FileInfo, hard_links), 64);
    assert_eq!(offset_of!(FileInfo, ea_size), 68);
    // Tail: 68 + 4 = 72 with no padding, because every member is 4- or 8-aligned and the struct is 8-aligned.
}

/// `FSP_FSCTL_STATIC_ASSERT(88 == sizeof(FSP_FSCTL_VOLUME_INFO))` — `fsctl.h:276`.
///
/// 8 + 8 + 2 + 64 label bytes = 82, rounded up to 88 by the 8-byte alignment the `u64` members impose.
#[test]
fn volume_info_matches_the_sdk_layout() {
    assert_eq!(size_of::<VolumeInfo>(), 88, "cell size: fsctl.h:276");
    assert_eq!(offset_of!(VolumeInfo, total_size), 0);
    assert_eq!(offset_of!(VolumeInfo, free_size), 8);
    assert_eq!(offset_of!(VolumeInfo, volume_label_length), 16);
    assert_eq!(
        offset_of!(VolumeInfo, volume_label),
        18,
        "cell label: 2 bytes of length then 64 bytes of label"
    );
    // 18 + 64 = 82; the struct rounds to 88 because `u64` members make the alignment 8.
}

/// The callback table is a **64-slot** array, not a ten-field struct: the first version of this file got that wrong, and the
/// SDK's own assertion (`sizeof == 64 * sizeof(pointer)`) is what caught it.
#[test]
fn the_interface_table_is_sixty_four_slots() {
    use fs_winfsp::abi::FileSystemInterface;

    assert_eq!(
        size_of::<FileSystemInterface>(),
        FileSystemInterface::SLOTS * size_of::<Option<unsafe extern "C" fn() -> i32>>(),
        "cell size: 64 pointers, no padding — the SDK asserts exactly this"
    );
    assert_eq!(FileSystemInterface::SLOTS, 64, "cell count: winfsp.h:1091");

    let empty = FileSystemInterface::empty();
    assert!(
        empty.slots.iter().all(Option::is_none),
        "cell empty: an unwired mount must have every slot `None`, not a half-filled table that looks implemented"
    );
}

/// The slot indices we rely on, checked against each other rather than against a comment.
///
/// A wrong index does not fail — it calls the wrong function, which is why these are pinned: the ordering of the first
/// sixteen entries is the one thing about this table that a reader cannot verify by eye.
#[test]
fn the_slot_indices_follow_the_sdk_order() {
    use fs_winfsp::abi::slot;

    // Ascending and distinct: if two constants ever name the same slot, one of the operations is silently unreachable.
    // Built at runtime so these are real comparisons rather than constant assertions: clippy is right that
    // `assert!(A < B)` on two `const`s is decided at compile time, and a compile-time check is a weaker statement than
    // "the values the binary actually uses", which is what this test is about.
    let mut seen: Vec<usize> = vec![
        slot::GET_VOLUME_INFO,
        slot::SET_VOLUME_LABEL,
        slot::GET_SECURITY_BY_NAME,
        slot::CREATE,
        slot::OPEN,
        slot::OVERWRITE,
        slot::CLEANUP,
        slot::CLOSE,
        slot::READ,
        slot::WRITE,
        slot::FLUSH,
        slot::GET_FILE_INFO,
        slot::SET_BASIC_INFO,
        slot::SET_FILE_SIZE,
        slot::CAN_DELETE,
        slot::RENAME,
    ];
    let original = seen.clone();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen, original,
        "cell order: the constants are distinct and ascending, like the header"
    );

    // The three pairs whose relative order is easy to get wrong and expensive to get wrong.
    for (left, right, why) in [
        (
            slot::OPEN,
            slot::OVERWRITE,
            "Open comes before Overwrite (winfsp.h:322 vs :345)",
        ),
        (
            slot::CLEANUP,
            slot::CLOSE,
            "Cleanup comes before Close (winfsp.h:410 vs :420)",
        ),
        (
            slot::READ,
            slot::GET_FILE_INFO,
            "Read comes before GetFileInfo (winfsp.h:441 vs :507)",
        ),
    ] {
        assert!(left < right, "cell order: {why}");
    }
    // Runtime comparison, for the same reason as above.
    let named = std::hint::black_box(slot::NAMED);
    let slots = std::hint::black_box(fs_winfsp::abi::FileSystemInterface::SLOTS);
    assert!(
        named < slots,
        "cell reserved: {named} named entries, the rest reserved, {slots} slots in total"
    );
}
