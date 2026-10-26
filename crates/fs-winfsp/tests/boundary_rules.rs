//! The FFI boundary's two rules (P3-4 card §2), pinned without WinFsp in the picture.

use fs_windows::error::NtStatus;
use fs_winfsp::boundary::{guarded, utf16_to_utf8, utf8_to_utf16};

/// UTF-16 crosses the boundary exactly once: a name survives a round trip, and the terminator is not part of it.
#[test]
fn names_round_trip_across_the_boundary() {
    for name in ["a.txt", "Ünïcode-目录", "☃-snowman"] {
        let units = utf8_to_utf16(name).expect("encode");
        assert_eq!(
            *units.last().expect("terminator"),
            0,
            "cell {name}: terminated for the OS"
        );
        assert_eq!(
            utf16_to_utf8(&units).expect("decode"),
            name,
            "cell {name}: the name survives the round trip — one conversion, at the edge"
        );
    }
    // The terminator ends the name even if bytes follow it: the OS hands us a buffer, not a length.
    assert_eq!(
        utf16_to_utf8(&[b'a' as u16, b'b' as u16, 0, b'z' as u16]).expect("decode"),
        "ab",
        "cell terminator: conversion stops at the first NUL"
    );
}

/// An unpaired surrogate and an embedded NUL are both refused, with the status the mapping table already has.
#[test]
fn unrepresentable_names_are_refused_not_truncated() {
    // A lone high surrogate: valid UTF-16 code units, invalid UTF-8 text.
    assert_eq!(
        utf16_to_utf8(&[0xD800, 0x0041]),
        Err(NtStatus::ObjectNameInvalid),
        "cell surrogate: refused rather than replaced — a replacement would store a name nobody can reproduce"
    );
    assert_eq!(
        utf8_to_utf16("a\0b"),
        Err(NtStatus::ObjectNameInvalid),
        "cell embedded nul: refused — the receiving side would truncate at it and silently lose the rest"
    );
}

/// A panic inside a callback becomes an error status instead of unwinding into the OS.
#[test]
fn a_panic_does_not_escape_the_callback() {
    let status = guarded(NtStatus::InternalError, || panic!("boom inside a callback"));
    assert_eq!(
        status,
        NtStatus::InternalError,
        "cell panic: caught at the boundary and reported as a status"
    );

    // Control: the same wrapper passes a normal status through untouched, so the assertion above is about the panic.
    let ok = guarded(NtStatus::InternalError, || NtStatus::Success);
    assert_eq!(
        ok,
        NtStatus::Success,
        "cell control: no panic, no rewriting"
    );
}
