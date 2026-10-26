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
