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

/// `FSP_FSCTL_VOLUME_PARAMS` is 504 bytes and this frontend writes only the leading scalars.
#[test]
fn volume_params_is_an_opaque_504_byte_block() {
    use fs_winfsp::abi::VolumeParams;

    assert_eq!(
        size_of::<VolumeParams>(),
        504,
        "cell size: fsctl.h:267 asserts 504 — the same number the SDK does"
    );
    let mut params = VolumeParams::zeroed();
    assert!(
        params.as_bytes().iter().all(|byte| *byte == 0),
        "cell default: every unset field is 0, which the SDK documents as the safe default"
    );

    params.set_version_size();
    params.set_sector_size(4096);
    params.set_sectors_per_allocation_unit(1);
    params.set_max_component_length(255);
    params.set_file_info_timeout(0);

    let bytes = params.as_bytes();
    assert_eq!(
        u16::from_ne_bytes([bytes[0], bytes[1]]),
        504,
        "cell version: `Version` carries the struct size (fsctl.h:193)"
    );
    assert_eq!(
        u16::from_ne_bytes([bytes[2], bytes[3]]),
        4096,
        "cell sector: fsctl.h:195"
    );
    assert_eq!(
        u16::from_ne_bytes([bytes[4], bytes[5]]),
        1,
        "cell sectors per unit: 4096-byte allocation with 4096-byte sectors (fsctl.h:196)"
    );
    assert_eq!(
        u16::from_ne_bytes([bytes[6], bytes[7]]),
        255,
        "cell name length: fsctl.h:197"
    );
    assert_eq!(
        u32::from_ne_bytes([bytes[32], bytes[33], bytes[34], bytes[35]]),
        0,
        "cell timeout: FileInfoTimeout at offset 32 (fsctl.h:204) — zero disables caching"
    );
    // The bitfield tail stays zero: no case sensitivity, no persistent ACLs, no reparse support claimed.
    assert!(
        bytes[36..].iter().all(|byte| *byte == 0),
        "cell tail: the bitfields are left at their defaults rather than hand-encoded"
    );
}

/// The `FSP_FSCTL_DIR_INFO` record: fixed header, 8-alignment, and the name at the end.
///
/// A wrong header size or a missing alignment does not fail loudly — the OS reads the name out of the middle of the next
/// record, which shows up as "garbage file names in Explorer" rather than as an error.
#[test]
fn a_dir_info_record_has_the_sdk_layout() {
    use fs_winfsp::abi::dir_info;

    assert_eq!(dir_info::SIZE_AT, 0, "cell size field: fsctl.h:301");
    assert_eq!(
        dir_info::FILE_INFO_AT,
        8,
        "cell file info: fsctl.h:302, 8-aligned after the u16 Size"
    );
    assert_eq!(
        dir_info::NEXT_AT,
        80,
        "cell next offset: fsctl.h:303-308 (24-byte union)"
    );
    assert_eq!(dir_info::NAME_AT, 104, "cell name: fsctl.h:309");
    assert_eq!(
        dir_info::HEADER,
        size_of::<fs_winfsp::abi::FileInfo>() + 32,
        "cell header: 8 + sizeof(FileInfo=72) + 24 = 104, which is the SDK's own assertion (fsctl.h:311)"
    );

    // Names of odd length must still leave the next record 8-aligned.
    for units in [1usize, 2, 3, 5, 31] {
        let size = dir_info::record_size(units);
        assert_eq!(
            size,
            dir_info::HEADER + units * 2,
            "cell size {units}: header plus the name, no terminator counted"
        );
        assert_eq!(
            dir_info::aligned(size) % 8,
            0,
            "cell align {units}: the next record starts at a multiple of 8"
        );
        assert!(
            dir_info::aligned(size) >= size,
            "cell align {units}: rounding up never shrinks a record"
        );
    }
}
