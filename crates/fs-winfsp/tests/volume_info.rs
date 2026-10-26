//! `GetVolumeInfo`'s body: the label length is **bytes**, the buffer is **units**, and a long label truncates.
//!
//! Each of those is a different mistake, so each gets its own assertion: a length reported in units is half the truth, a
//! buffer without a terminator reads past its end, and refusing a long label makes the volume invisible over a cosmetic
//! field.

use fs_winfsp::volume::{volume_info, MAX_LABEL_UNITS};

#[test]
fn the_label_length_is_reported_in_bytes() {
    let info = volume_info("wcas", 1024, 512).expect("volume");
    assert_eq!(info.total_size, 1024);
    assert_eq!(info.free_size, 512);
    assert_eq!(
        info.volume_label_length, 8,
        "cell length: four characters are **eight bytes** — `VolumeLabelLength` is a byte count"
    );
    assert_eq!(
        &info.volume_label[..4],
        &[b'w' as u16, b'c' as u16, b'a' as u16, b's' as u16],
        "cell units: the buffer is UTF-16, as the SDK field is"
    );
    assert_eq!(
        info.volume_label[4], 0,
        "cell terminator: the buffer is NUL-terminated"
    );
}

/// A label at the limit fits with its terminator; one past the limit truncates instead of failing.
#[test]
fn a_long_label_truncates_and_keeps_its_terminator() {
    let at_limit = "x".repeat(MAX_LABEL_UNITS);
    let info = volume_info(&at_limit, 1, 0).expect("at limit");
    assert_eq!(
        info.volume_label_length as usize,
        MAX_LABEL_UNITS * 2,
        "cell at limit: 31 units = 62 bytes, and unit 31 is still the terminator"
    );
    assert_eq!(
        info.volume_label[31], 0,
        "cell at limit: terminator present"
    );

    let over = "y".repeat(64);
    let info = volume_info(&over, 1, 0).expect("over limit");
    assert_eq!(
        info.volume_label_length as usize,
        MAX_LABEL_UNITS * 2,
        "cell over limit: truncated to what fits — the volume stays visible"
    );
    assert_eq!(
        info.volume_label[31], 0,
        "cell over limit: still terminated, so nobody reads past the buffer"
    );
}

/// A label with an interior NUL is refused: the OS would end the string there and drop the rest silently.
#[test]
fn an_interior_nul_is_refused() {
    assert_eq!(
        volume_info("a\0b", 1, 0),
        Err(fs_windows::error::NtStatus::ObjectNameInvalid),
        "cell nul: refused rather than truncated"
    );
}
