//! The **baseline reverse check**: the encoder that this build ships must still produce the measured v2 shape.
//!
//! This file exists to make an encoder change **explicit**. Nothing here is a new criterion about quality; it is a
//! tripwire saying "the pages you are looking at are not the ones we measured, and if that is intended, the numbers below
//! and the G3 card must move together".
//!
//! # Why the literals, and why they read `Profile::CURRENT`
//!
//! The expected numbers are written as **literals with a comment naming their origin** (`41694d0`, debug, 5-run median,
//! 99,999 records with oid) rather than computed from the encoder: a check that recomputed them from the code under test
//! would agree with any change, which is the "expected value shares code with the subject" trap.
//!
//! They are compared against `Profile::CURRENT`, **not** against a hard-coded `V2`. When `CURRENT` becomes `V3`, these
//! assertions go red until the numbers are re-measured — that is the intended failure, and it is why this file does not
//! hard-code v2: a hard-coded subject would keep passing while judging the wrong encoder.
//!
//! **Changing a number below means you changed the paging mechanism. Re-measure with `g3_baseline`, then update the G3
//! card in the same commit.**

use std::collections::BTreeMap;

use cas_core::{ObjectId, Role};
use fs_snapshot::{
    encode_inode_table, Content, InodeRecord, Kind, PageSink, Profile, SnapshotResult,
};

#[derive(Default)]
struct RecordingSink {
    pages: BTreeMap<ObjectId, (Role, Vec<u8>)>,
}

impl PageSink for RecordingSink {
    fn put(&mut self, role: Role, value: &[u8]) -> SnapshotResult<ObjectId> {
        let oid = ObjectId::from_bytes(blake3::hash(value).into());
        self.pages.insert(oid, (role, value.to_vec()));
        Ok(oid)
    }
}

fn file_inode(serial: u64) -> InodeRecord {
    InodeRecord {
        serial,
        kind: Kind::File,
        size: 4096,
        created_ns: 1_700_000_000_000_000_000,
        modified_ns: 1_700_000_001_000_000_000,
        accessed_ns: 0,
        changed_ns: 0,
        attributes: 0,
        security: Vec::new(),
        content: Content::Blob(ObjectId::from_bytes(
            [u8::try_from(serial % 251).expect("fits"); 32],
        )),
    }
}

struct Shape {
    pages: usize,
    largest: usize,
    root_height: u8,
}

fn shape(profile: Profile) -> Shape {
    let mut sink = RecordingSink::default();
    let mut records = vec![InodeRecord {
        serial: 0,
        kind: Kind::Directory,
        size: 0,
        created_ns: 0,
        modified_ns: 0,
        accessed_ns: 0,
        changed_ns: 0,
        attributes: 0,
        security: Vec::new(),
        content: Content::Empty,
    }];
    for serial in 1..100_000u64 {
        records.push(file_inode(serial));
    }
    let root = encode_inode_table(&profile, records.into_iter(), &mut sink).expect("encode");
    let table: Vec<usize> = sink
        .pages
        .iter()
        .filter(|(_, (role, _))| *role == Role::InodeTable)
        .map(|(_, (_, value))| value.len())
        .collect();
    Shape {
        pages: table.len(),
        largest: table.iter().copied().max().unwrap_or(0),
        root_height: sink.pages.get(&root).expect("root page").1[2],
    }
}

#[test]
fn the_current_encoder_still_matches_the_measured_baseline() {
    let measured = shape(Profile::CURRENT);
    // Baseline @ 41694d0: 99,999 inode records with oid, `max_page_bytes` 16384, debug build, 5-run median.
    assert_eq!(
        measured.pages, 1533,
        "pages_total changed — this is the paging mechanism, not a detail. Re-run `g3_baseline`, update this number and \
         the G3 card in the same commit"
    );
    // Baseline @ 41694d0: the fullest page, 274 bytes below the 16384 limit.
    assert_eq!(
        measured.largest, 16116,
        "largest page changed — records are being packed differently; re-measure before updating this number"
    );
    // Baseline @ 41694d0, **product** definition: the root page's height byte is 3 (the encoder accessor says 2; the byte
    // is what criterion 3 judges, so the byte is what is pinned here).
    assert_eq!(
        measured.root_height, 3,
        "root height byte changed — the tree gained or lost a level; that is a fan-out change, not a record-size change"
    );
}
