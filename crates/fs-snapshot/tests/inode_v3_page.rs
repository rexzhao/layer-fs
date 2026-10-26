//! The v3 **page** format: `fmt = 2` on inode pages only, and the record layout that goes with it (P3-2 card §3.1).
//!
//! These assertions read the encoded bytes directly instead of going through a reader: the claim is about what is on the
//! wire, and a round trip through this build's own reader would be satisfied by any two halves that agree with each
//! other — the half-v3 trap this batch exists to avoid.

use std::collections::BTreeMap;

use cas_core::{ObjectId, Role};
use fs_snapshot::record::Content;
use fs_snapshot::{
    encode_inode_table, inode_v3, InodeRecord, Kind, PageSink, Profile, SnapshotError,
    SnapshotResult,
};

#[derive(Default)]
struct Sink {
    pages: BTreeMap<ObjectId, (Role, Vec<u8>)>,
}

impl PageSink for Sink {
    fn put(&mut self, role: Role, value: &[u8]) -> SnapshotResult<ObjectId> {
        let oid = ObjectId::from_bytes(blake3::hash(value).into());
        self.pages.insert(oid, (role, value.to_vec()));
        Ok(oid)
    }
}

/// Values a reader can recognise at a glance: if the four v3 fields came back as zeros, these constants make it visible.
const ACCESSED: i64 = 0x1122_3344_5566_7788;
const CHANGED: i64 = 0x0011_2233_4455_6677;
const ATTRIBUTES: u32 = 0x21;
const SECURITY: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];

fn record(serial: u64) -> InodeRecord {
    InodeRecord {
        serial,
        kind: Kind::File,
        size: 0,
        created_ns: 1_000,
        modified_ns: 2_000,
        accessed_ns: ACCESSED,
        changed_ns: CHANGED,
        attributes: ATTRIBUTES,
        security: SECURITY.to_vec(),
        content: Content::Empty,
    }
}

fn encode(profile: Profile, count: u64) -> Vec<Vec<u8>> {
    let mut sink = Sink::default();
    let records: Vec<InodeRecord> = (0..count).map(record).collect();
    encode_inode_table(&profile, records.into_iter(), &mut sink).expect("encode");
    let mut pages: Vec<Vec<u8>> = sink
        .pages
        .values()
        .filter(|(role, _)| *role == Role::InodeTable)
        .map(|(_, value)| value.clone())
        .collect();
    pages.sort();
    pages
}

/// v3 inode pages start with `fmt = 2`; v2 pages still start with `1`.
#[test]
fn only_v3_inode_pages_carry_fmt_two() {
    let v2 = encode(Profile::V2, 1);
    let v3 = encode(Profile::V3, 1);
    assert_eq!(v2[0][0], 1, "cell v2: inode page fmt is 1");
    assert_eq!(v3[0][0], 2, "cell v3: inode page fmt is 2");
    // Everything after the fmt byte is the record; the v3 page must be longer by exactly 24 bytes + the SD.
    assert_eq!(
        v3[0].len(),
        v2[0].len() + 24 + SECURITY.len(),
        "cell growth: 8+8+4+4 bytes plus the descriptor — the ADR's '單條記錄最小增長 +24'"
    );
}

/// The four v3 fields survive the page builder with **their known values**, at every record in a multi-record page.
///
/// Checking more than one record is the point: a decoder that advanced by "the rest of the page" would pass a
/// single-record assertion and drop every record after the first.
#[test]
fn every_record_in_a_page_keeps_its_known_v3_values() {
    let pages = encode(Profile::V3, 3);
    let page = &pages[0];
    assert_eq!(page[0], 2, "cell fmt: v3 inode page");
    assert_eq!(page[1], 0, "cell node: leaf");
    let count = u32::from_be_bytes(page[2..6].try_into().expect("count"));
    assert_eq!(count, 3, "cell count: three records fit in one leaf");

    let mut at = 6usize;
    for expected_serial in 0..3u64 {
        let (decoded, used) = inode_v3::decode(&page[at..]).expect("decode record");
        assert_eq!(
            decoded.serial, expected_serial,
            "cell serial: record {expected_serial} decoded in order"
        );
        assert_eq!(
            decoded.accessed_ns, ACCESSED,
            "cell accessed: known constant"
        );
        assert_eq!(decoded.changed_ns, CHANGED, "cell changed: known constant");
        assert_eq!(
            decoded.attributes, ATTRIBUTES,
            "cell attributes: known constant"
        );
        assert_eq!(
            decoded.security,
            SECURITY.to_vec(),
            "cell security: verbatim"
        );
        at += used;
    }
    assert_eq!(at, page.len(), "cell consumed: exactly the whole page");
}

/// **Half-v3 negative**: a page that says `fmt = 2` but carries v2-layout records must fail to decode.
///
/// This is what makes the `fmt` byte load-bearing rather than decorative: without it, a build could switch the format
/// byte alone and every in-process round trip would still pass.
#[test]
fn a_v2_layout_record_under_fmt_two_is_refused() {
    let v2_pages = encode(Profile::V2, 1);
    let mut forged = v2_pages[0].clone();
    forged[0] = 2; // advertise v3, keep v2 records
    let error = inode_v3::decode(&forged[6..])
        .expect_err("cell half-v3: the record bytes do not parse as v3");
    assert!(
        matches!(error, SnapshotError::Malformed { .. }),
        "cell half-v3: Malformed (the bytes are corrupt for this format), not a silent field misread: {error:?}"
    );
}
