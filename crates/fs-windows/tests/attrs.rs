//! `attrs` tests (P3-1 §1): what is stored, what is derived, and what is refused — coordinate-labelled.

use fs_windows::attrs::{
    accept, report, Kind, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM, STORABLE,
};
use fs_windows::error::Failure;

#[test]
fn exactly_four_bits_are_storable() {
    assert_eq!(STORABLE.len(), 4, "enum cardinality: four storable bits");
    for bit in STORABLE {
        assert_eq!(accept(bit), Ok(bit), "cell bit={bit:#x}: accepted as-is");
    }
}

/// `DIRECTORY` and `NORMAL` are derived; writing them is refused rather than silently dropped.
#[test]
fn derived_bits_are_refused_on_a_write() {
    assert_eq!(
        accept(FILE_ATTRIBUTE_DIRECTORY),
        Err(Failure::AccessDenied),
        "cell bit=DIRECTORY: derived from the entry kind, not storable"
    );
    assert_eq!(
        accept(FILE_ATTRIBUTE_NORMAL),
        Err(Failure::AccessDenied),
        "cell bit=NORMAL: defined as 'nothing else set', so it cannot be stored beside others"
    );
    assert_eq!(
        accept(FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_DIRECTORY),
        Err(Failure::AccessDenied),
        "cell bits=READONLY|DIRECTORY: one derived bit is enough to refuse"
    );
}

/// Bits this model knows nothing about are refused too: a caller must not believe encryption happened.
#[test]
fn unknown_bits_are_refused_not_ignored() {
    assert_eq!(
        accept(0x0000_4000),
        Err(Failure::InvalidParameter),
        "cell bit=ENCRYPTED: unsupported, refused"
    );
    // All bits set contains DERIVED bits too, and those are checked first: the answer is the more specific
    // refusal (AccessDenied), not the catch-all. The test states that order instead of leaving it implicit.
    assert_eq!(
        accept(0xFFFF_FFFF),
        Err(Failure::AccessDenied),
        "cell bits=all: the derived bits are refused first, and they are present"
    );
    assert_eq!(
        accept(!(FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_NORMAL)),
        Err(Failure::InvalidParameter),
        "cell bits=all-but-derived: only unknown bits remain, so the answer is InvalidParameter"
    );
}

/// Reporting: no stored bits means `NORMAL`, and a directory adds `DIRECTORY` **on top of** stored bits.
#[test]
fn report_derives_normal_and_directory() {
    assert_eq!(
        report(Kind::File, 0),
        FILE_ATTRIBUTE_NORMAL,
        "cell kind=file stored=none: NORMAL, because nothing else is set"
    );
    assert_eq!(
        report(Kind::File, FILE_ATTRIBUTE_READONLY),
        FILE_ATTRIBUTE_READONLY,
        "cell kind=file stored=READONLY: NORMAL must not be added beside another bit"
    );
    assert_eq!(
        report(Kind::Directory, 0),
        FILE_ATTRIBUTE_DIRECTORY,
        "cell kind=dir stored=none: DIRECTORY, and no NORMAL beside it"
    );
    assert_eq!(
        report(Kind::Directory, FILE_ATTRIBUTE_HIDDEN),
        FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_DIRECTORY,
        "cell kind=dir stored=HIDDEN: both bits"
    );
}

/// Every storable combination reports without `NORMAL` unless it is empty — the equivalence rule.
#[test]
fn normal_is_reported_only_when_no_other_bit_is_set() {
    for mask in 0u32..32 {
        let storable = mask & STORABLE.iter().copied().fold(0, |acc, bit| acc | bit);
        for kind in Kind::ALL {
            let word = report(kind, storable);
            assert_eq!(
                word & FILE_ATTRIBUTE_NORMAL != 0,
                word == FILE_ATTRIBUTE_NORMAL,
                "cell kind={kind:?} mask={mask}: NORMAL appears only on its own, got {word:#x}"
            );
            assert_eq!(
                word & FILE_ATTRIBUTE_DIRECTORY != 0,
                kind == Kind::Directory,
                "cell kind={kind:?} mask={mask}: DIRECTORY follows the kind"
            );
        }
    }
}

/// The bits keep their Windows values; a typo here would silently change what Explorer shows.
#[test]
fn the_bit_values_are_the_windows_ones() {
    assert_eq!(FILE_ATTRIBUTE_READONLY, 0x1, "cell READONLY");
    assert_eq!(FILE_ATTRIBUTE_HIDDEN, 0x2, "cell HIDDEN");
    assert_eq!(FILE_ATTRIBUTE_SYSTEM, 0x4, "cell SYSTEM");
    assert_eq!(FILE_ATTRIBUTE_DIRECTORY, 0x10, "cell DIRECTORY");
    assert_eq!(FILE_ATTRIBUTE_ARCHIVE, 0x20, "cell ARCHIVE");
    assert_eq!(FILE_ATTRIBUTE_NORMAL, 0x80, "cell NORMAL");
}
