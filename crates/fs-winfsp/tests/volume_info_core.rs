//! The `GetVolumeInfo` **body**, testable without a mount (361: the pure core goes in a testable function, the `unsafe` shell
//! keeps only the process-wide lookup).
//!
//! Four cells, and each one is a different answer — including the one that used to be a bug: a buffer that is too small is
//! "too small", never "unsupported" (WinFsp retries the former and treats the latter as terminal).

use std::ffi::c_void;

use fs_winfsp::abi::VolumeInfo;
use fs_winfsp::callbacks::{
    get_volume_info_for, NOT_IMPLEMENTED, OURS_BUFFER_TOO_SMALL, OURS_INVALID_PARAMETER,
};

fn volume(total: u64) -> VolumeInfo {
    VolumeInfo {
        total_size: total,
        free_size: total / 2,
        volume_label_length: 0,
        volume_label: [0; 32],
    }
}

#[test]
fn a_volume_is_written_into_a_large_enough_buffer() {
    let mut out = VolumeInfo {
        total_size: 0,
        free_size: 0,
        volume_label_length: 0,
        volume_label: [0; 32],
    };
    let capacity = u32::try_from(std::mem::size_of::<VolumeInfo>()).expect("capacity fits u32");
    let status = unsafe {
        get_volume_info_for(
            Some(volume(1 << 20)),
            std::ptr::addr_of_mut!(out).cast::<c_void>(),
            capacity,
        )
    };
    assert_eq!(status, 0, "cell ok: a full-size buffer succeeds");
    assert_eq!(
        out.total_size,
        1 << 20,
        "cell ok: and the volume is the one that was handed in"
    );
    assert_eq!(
        out.free_size,
        1 << 19,
        "cell ok: every field travelled, not just the first"
    );
}

#[test]
fn a_short_buffer_is_told_so_rather_than_refused() {
    let mut out = volume(0);
    let short = u32::try_from(std::mem::size_of::<VolumeInfo>()).expect("fits") - 1;
    let status = unsafe {
        get_volume_info_for(
            Some(volume(1 << 20)),
            std::ptr::addr_of_mut!(out).cast::<c_void>(),
            short,
        )
    };
    assert_eq!(
        status, OURS_BUFFER_TOO_SMALL,
        "cell short: **too small is not unsupported** — and the volume was configured, so this cannot be the `None` answer"
    );
    assert_eq!(
        out.total_size, 0,
        "cell short: nothing was written into the short buffer"
    );
}

#[test]
fn no_configured_volume_is_refused_and_that_is_a_different_answer_from_a_short_buffer() {
    let mut out = volume(0);
    let capacity = u32::try_from(std::mem::size_of::<VolumeInfo>()).expect("fits");
    let status = unsafe {
        get_volume_info_for(None, std::ptr::addr_of_mut!(out).cast::<c_void>(), capacity)
    };
    assert_eq!(status, NOT_IMPLEMENTED, "cell absent: no volume configured");
    assert_ne!(
        status, OURS_BUFFER_TOO_SMALL,
        "cell absent: **a missing volume and a small buffer must not share an answer**"
    );
    assert_eq!(out.total_size, 0, "cell absent: nothing was written");
}

#[test]
fn a_null_buffer_is_refused_before_anything_else() {
    let status = unsafe { get_volume_info_for(Some(volume(1 << 20)), std::ptr::null_mut(), 4096) };
    assert_eq!(
        status, OURS_INVALID_PARAMETER,
        "cell null: the pointer is checked first"
    );
}
