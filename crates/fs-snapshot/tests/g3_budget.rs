//! G3's page/height budget, asserted against the **product** (P3-2 card §3.3 criteria 1–3).
//!
//! These measure whatever `Profile::CURRENT` is, so the moment v3 lands they judge it with no edit here. The thresholds
//! are the card's: `pages ≤ 1546` (baseline 1472 × 1.05), `max page ≤ max_page_bytes`, `root height byte ≤ 3`.
//!
//! Criterion 3 has **zero headroom**: the product's root height byte is 3 today and the threshold is `≤ 3`, because that
//! byte is what actually changes when records grow. Its failure message therefore names *which* of the two things went
//! wrong — a deeper tree (fan-out) or a bigger tree (page count) — instead of one assertion carrying both meanings.
//!
//! Criterion 4 (time) is not here: it lives in `g3_baseline.rs` as an `#[ignore]`d measurement. A timing assertion in CI
//! is a flaky criterion, and a flaky criterion gets muted, and a muted criterion is not a criterion.

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

/// The card's "带 oid 记录": content, size, both timestamps.
fn file_inode(serial: u64) -> InodeRecord {
    InodeRecord {
        serial,
        kind: Kind::File,
        size: 4096,
        created_ns: 1_700_000_000_000_000_000,
        modified_ns: 1_700_000_001_000_000_000,
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

/// Criterion 1 — `pages ≤ 1546` (baseline 1472 × 1.05).
#[test]
fn pages_stay_within_the_budget() {
    let measured = shape(Profile::CURRENT);
    assert!(
        measured.pages <= 1546,
        "cell pages: {} pages (baseline 1472, red line 1546 = ×1.05). A jump near +26% means records are being packed \
         by bytes rather than by the key-hash pages this format uses; a small rise can be legitimate (2.3% of pages were \
         already capacity-constrained in the baseline)",
        measured.pages
    );
}

/// Criterion 2 — no page exceeds `max_page_bytes`.
///
/// This is the **only** way bigger records may legitimately change the page count: a page that hits the byte guardrail
/// splits early. If a page were over the limit the encoder would be violating its own profile, which is worse than a
/// budget miss.
#[test]
fn no_page_exceeds_the_profile_limit() {
    let measured = shape(Profile::CURRENT);
    let limit = Profile::CURRENT.max_page_bytes;
    assert!(
        measured.largest <= limit,
        "cell page size: largest page is {} bytes, limit {limit}",
        measured.largest
    );
}

/// Criterion 3 — the **root page's height byte** is at most 3.
///
/// Zero headroom by design: the current product reports exactly 3. Two separate assertions, because "the tree got deeper"
/// and "the tree got wider" are different defects and must not be reported as one.
#[test]
fn the_root_height_byte_stays_at_most_three() {
    let measured = shape(Profile::CURRENT);
    assert!(
        measured.root_height <= 3,
        "cell height: root height byte is {} (limit 3). The tree gained a level — that is a fan-out problem, not a \
         capacity problem: check that branch pages are still filled by the key-hash split",
        measured.root_height
    );
    assert!(
        measured.pages <= 2000,
        "cell width: {} pages with height {} — the height is fine but the tree got wider, which is a capacity (record \
         size) problem, not a fan-out problem",
        measured.pages,
        measured.root_height
    );
}
