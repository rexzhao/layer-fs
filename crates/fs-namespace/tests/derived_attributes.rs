//! The **reporting** side of attributes: `Stat.attributes` is derived, the store keeps only the four writable bits.
//!
//! The four rows are the card's boundaries, and each one fails for a *different* plausible implementation:
//! reporting `NORMAL` alongside `READONLY`, forgetting `DIRECTORY` for a directory, reporting 0 for an ordinary file,
//! or exchanging stored and reported values.

use fs_namespace::{derived_attributes, Kind};

/// A read-only file reports `0x01` and **not** `NORMAL`: `NORMAL` means "no other bit is set".
#[test]
fn a_read_only_file_reports_only_read_only() {
    assert_eq!(
        derived_attributes(Kind::File, 0x01),
        0x01,
        "cell read-only: NORMAL must not be added when another bit is set"
    );
}

/// A directory reports `DIRECTORY`, whatever it stores.
#[test]
fn a_directory_reports_directory() {
    assert_eq!(
        derived_attributes(Kind::Directory, 0),
        0x10,
        "cell directory: DIRECTORY is derived from kind"
    );
    assert_eq!(
        derived_attributes(Kind::Directory, 0x02),
        0x12,
        "cell directory+hidden: the stored bit is kept and DIRECTORY is added"
    );
}

/// An ordinary file reports `NORMAL`.
#[test]
fn an_ordinary_file_reports_normal() {
    assert_eq!(
        derived_attributes(Kind::File, 0),
        0x80,
        "cell normal: an ordinary file is NORMAL, not 0"
    );
}

/// Stored and reported are two different quantities: the round trip is `set_attributes(x)` then `stat == derive(x)`.
///
/// The point of this row is that a caller cannot feed a reported value back in: `0x80` (reported) is not a legal stored
/// value, and the trait refuses it.
#[test]
fn stored_and_reported_are_different_quantities() {
    for stored in [0x00u32, 0x01, 0x02, 0x04, 0x20, 0x21] {
        let reported = derived_attributes(Kind::File, stored);
        assert_eq!(
            reported,
            if stored == 0 { 0x80 } else { stored },
            "cell stored {stored:#x}: reported is the stored bits plus NORMAL only when nothing is stored"
        );
        assert_eq!(
            reported & 0x10,
            0,
            "cell stored {stored:#x}: a **file** never reports DIRECTORY"
        );
    }
    // The derived bits are outside the stored value domain, which is what makes feeding a reported value back illegal.
    assert_ne!(
        derived_attributes(Kind::File, 0),
        0,
        "cell domain: the reported value is not the stored one"
    );
}
