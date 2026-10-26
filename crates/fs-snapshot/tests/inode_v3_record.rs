//! The v3 record codec, row by row against ADR-005-v3 draft §3.1 (P3-2 card G3, acceptance matrix).
//!
//! Every row is about **bytes**, so none of them needs a tree, a store or a root. The negative rows assert the *variant*
//! (`Malformed`), never a message: `security_len` too large and an illegal attribute bit are both refusals, and a test
//! that only checked "it failed" would accept the wrong one.

use fs_snapshot::record::Content;
use fs_snapshot::{InodeRecordV3, Kind, MAX_SECURITY_LEN};

fn record() -> InodeRecordV3 {
    InodeRecordV3 {
        serial: 7,
        kind: Kind::File,
        size: 4096,
        created_ns: 1,
        modified_ns: 2,
        accessed_ns: 3,
        changed_ns: 4,
        attributes: 0x01 | 0x20,
        security: Vec::new(),
        content: Content::Empty,
    }
}

/// The field order of §2.2: a round trip, plus the **byte offsets**, because a round trip alone is satisfied by any
/// self-consistent layout — including a wrong one.
#[test]
fn the_layout_is_the_documented_field_order() {
    let encoded = fs_snapshot::inode_v3::encode(&record()).expect("encode");
    assert_eq!(
        &encoded[0..8],
        &7u64.to_be_bytes(),
        "cell serial: first, 8 bytes"
    );
    assert_eq!(encoded[8], 2, "cell kind: byte 8, 2 = File");
    assert_eq!(&encoded[9..17], &4096u64.to_be_bytes(), "cell size: byte 9");
    assert_eq!(&encoded[17..25], &1i64.to_be_bytes(), "cell created_ns");
    assert_eq!(&encoded[25..33], &2i64.to_be_bytes(), "cell modified_ns");
    assert_eq!(&encoded[33..41], &3i64.to_be_bytes(), "cell accessed_ns");
    assert_eq!(&encoded[41..49], &4i64.to_be_bytes(), "cell changed_ns");
    assert_eq!(
        &encoded[49..53],
        &(0x01u32 | 0x20).to_be_bytes(),
        "cell attributes"
    );
    assert_eq!(&encoded[53..57], &0u32.to_be_bytes(), "cell security_len");
    assert_eq!(
        encoded[57], 0,
        "cell content_tag: 0 = Empty, right after the security bytes"
    );
    assert_eq!(
        encoded.len(),
        58,
        "cell length: 57 fixed bytes + tag without an oid"
    );

    let (decoded, used) = fs_snapshot::inode_v3::decode(&encoded).expect("decode");
    assert_eq!(decoded, record(), "cell round trip");
    assert_eq!(used, encoded.len(), "cell consumed: the whole record");
}

/// `security_len` boundaries: 0, 1, 65535 and 65536 are accepted (65536 **inclusive**), 65537 is refused.
#[test]
fn security_len_accepts_up_to_65536_inclusive() {
    for len in [0usize, 1, 65_535, 65_536] {
        let mut candidate = record();
        candidate.security = vec![0xA5; len];
        let encoded = fs_snapshot::inode_v3::encode(&candidate).unwrap_or_else(|error| {
            panic!("cell len {len}: {len} bytes must be legal, got {error:?}")
        });
        let (decoded, _) = fs_snapshot::inode_v3::decode(&encoded).expect("decode");
        assert_eq!(
            decoded.security.len(),
            len,
            "cell len {len}: the security bytes round-trip at full length"
        );
        assert_eq!(
            decoded.security,
            vec![0xA5; len],
            "cell bytes {len}: the descriptor is copied verbatim, not normalised"
        );
    }

    let mut over = record();
    over.security = vec![0u8; 65_537];
    let error = fs_snapshot::inode_v3::encode(&over)
        .expect_err("cell 65537: one byte over the limit is refused");
    assert!(
        matches!(error, fs_snapshot::SnapshotError::Malformed { .. }),
        "cell 65537: Malformed (a corrupt record), not UnsupportedFormat (a version difference): {error:?}"
    );
    assert_eq!(
        MAX_SECURITY_LEN, 65_536,
        "cell constant: the limit is inclusive"
    );
}

/// Attributes: only the four writable bits may be stored; `DIRECTORY`, `NORMAL` and unknown bits are `Malformed`.
#[test]
fn only_writable_attribute_bits_may_be_stored() {
    for attributes in [0x01u32, 0x02, 0x04, 0x20, 0x01 | 0x02 | 0x04 | 0x20] {
        let mut candidate = record();
        candidate.attributes = attributes;
        assert!(
            fs_snapshot::inode_v3::encode(&candidate).is_ok(),
            "cell {attributes:#x}: a writable-bit combination is storable"
        );
    }
    for attributes in [0x10u32, 0x80, 0x10 | 0x01, 0x08, 0x40, 0x100] {
        let mut candidate = record();
        candidate.attributes = attributes;
        let error = fs_snapshot::inode_v3::encode(&candidate)
            .expect_err("cell illegal bit: must be refused");
        assert!(
            matches!(error, fs_snapshot::SnapshotError::Malformed { .. }),
            "cell {attributes:#x}: Malformed — derived (0x10/0x80) or unknown bits are not storable: {error:?}"
        );
    }
}

/// The four timestamps round-trip at **exactly `i64::MIN`, `i64::MAX` and `0`** — those three literals, nothing else —
/// and no ordering is enforced.
///
/// Spelled out because "the extremes" outlives this file: half a year from now the question will be *which* extremes,
/// and an answer that has to be re-derived from the code is an answer that gets re-derived wrongly. `i64::MIN`/`MAX` are
/// not plausible filesystem times; they are here to prove the **field width and signedness**, which a mid-range value
/// like `1_700_000_000_000_000_000` cannot.
#[test]
fn timestamps_round_trip_at_the_extremes() {
    for value in [i64::MIN, i64::MAX, 0] {
        let mut candidate = record();
        candidate.created_ns = value;
        candidate.modified_ns = value;
        candidate.accessed_ns = value;
        candidate.changed_ns = value;
        let encoded = fs_snapshot::inode_v3::encode(&candidate).expect("encode");
        let (decoded, _) = fs_snapshot::inode_v3::decode(&encoded).expect("decode");
        assert_eq!(decoded, candidate, "cell {value}: all four times survive");
    }
    // No ordering: `modified` before `created` is a legal record (clocks move backwards).
    let mut backwards = record();
    backwards.created_ns = 1_000;
    backwards.modified_ns = -1_000;
    assert!(
        fs_snapshot::inode_v3::encode(&backwards).is_ok(),
        "cell order: timestamps are not validated against each other"
    );
}

/// A record that a **decoder** is handed directly (not produced by `encode`) is validated too: the invariants belong to
/// the format, not to the writer.
#[test]
fn decoding_validates_the_same_invariants() {
    let mut encoded = fs_snapshot::inode_v3::encode(&record()).expect("encode");
    // Splice a derived attribute bit in without re-encoding.
    encoded[49..53].copy_from_slice(&0x10u32.to_be_bytes());
    let error = fs_snapshot::inode_v3::decode(&encoded)
        .expect_err("cell derived bit: refused on the way in");
    assert!(
        matches!(error, fs_snapshot::SnapshotError::Malformed { .. }),
        "cell derived bit: Malformed: {error:?}"
    );

    // A security_len above the limit, again spliced in: the length check must come from the record, not from the Vec it
    // happens to be stored in.
    let mut encoded = fs_snapshot::inode_v3::encode(&record()).expect("encode");
    encoded[53..57].copy_from_slice(&65_537u32.to_be_bytes());
    let error =
        fs_snapshot::inode_v3::decode(&encoded).expect_err("cell oversized security_len: refused");
    assert!(
        matches!(error, fs_snapshot::SnapshotError::Malformed { .. }),
        "cell oversized: Malformed: {error:?}"
    );
}

/// Truncation and unknown tags are `Malformed`, and the decoder never reads past the end.
///
/// Each cut is asserted to be an **error**, not merely "not a panic": a decoder that silently accepted a short slice
/// would report a record whose timestamps came from whatever bytes followed.
#[test]
fn truncation_and_unknown_tags_are_malformed() {
    let full = fs_snapshot::inode_v3::encode(&record()).expect("encode");
    for cut in [0usize, 8, 40, full.len() - 1] {
        let error = fs_snapshot::inode_v3::decode(&full[..cut])
            .expect_err("a truncated record must not decode");
        assert!(
            matches!(error, fs_snapshot::SnapshotError::Malformed { .. }),
            "cell cut {cut}: Malformed: {error:?}"
        );
    }

    // Unknown `kind` and unknown `content_tag` are refusals too, and they are *different* fields: a decoder that folded
    // them together would still pass every test above.
    let mut bad_kind = full.clone();
    bad_kind[8] = 9;
    assert!(
        matches!(
            fs_snapshot::inode_v3::decode(&bad_kind).expect_err("unknown kind"),
            fs_snapshot::SnapshotError::Malformed { .. }
        ),
        "cell kind: unknown kind is Malformed"
    );
    let mut bad_tag = full;
    let tag_at = bad_tag.len() - 1;
    bad_tag[tag_at] = 7;
    assert!(
        matches!(
            fs_snapshot::inode_v3::decode(&bad_tag).expect_err("unknown content tag"),
            fs_snapshot::SnapshotError::Malformed { .. }
        ),
        "cell tag: unknown content_tag is Malformed"
    );
}
