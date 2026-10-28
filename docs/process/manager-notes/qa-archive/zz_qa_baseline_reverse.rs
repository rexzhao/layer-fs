//! Baseline reverse-check (approval 181): the four numbers must match `41694d0`'s baseline exactly.
use std::time::Instant;

use cas_core::{ObjectId, Role};
use fs_snapshot::{encode_inode_table, Content, InodeRecord, Kind, PageSink, Profile};
use fs_snapshot::SnapshotError;

struct Sink { values: Vec<Vec<u8>> }
impl PageSink for Sink {
    fn put(&mut self, _role: Role, value: &[u8]) -> Result<ObjectId, SnapshotError> {
        self.values.push(value.to_vec());
        Ok(ObjectId::from_bytes(*blake3::hash(value).as_bytes()))
    }
}

#[test]
fn qa_baseline_reverse_check() {
    const N: u64 = 99_999;
    const GUARD: usize = 16_384 - 311;
    let mut sink = Sink { values: Vec::new() };
    let records = (1..=N).map(|serial| InodeRecord {
        serial, kind: Kind::File, size: 4096, created_ns: 0, modified_ns: 0,
        content: Content::Blob(ObjectId::from_bytes([7; 32])),
    });
    encode_inode_table(&Profile::V2, records, &mut sink).expect("encode");

    let leaves: Vec<&Vec<u8>> = sink.values.iter().filter(|v| v[1] == 0).collect();
    let pages = sink.values.len();
    let max_page = sink.values.iter().map(Vec::len).max().unwrap_or(0);
    let max_entries = leaves.iter().map(|v| u32::from_be_bytes([v[2], v[3], v[4], v[5]]) as usize).max().unwrap_or(0);
    let guard_hits = leaves.iter().filter(|v| v.len() >= GUARD).count();
    let avg = N as f64 / leaves.len() as f64;

    println!("pages={pages} leaves={leaves} branches={branches} max_page={max_page} max_entries={max_entries} avg={avg:.2} guard_hits={guard_hits}",
        leaves = leaves.len(), branches = pages - leaves.len());
    assert_eq!(pages, 1472, "baseline pages");
    assert_eq!(leaves.len(), 1448, "baseline leaves");
    assert_eq!(pages - leaves.len(), 24, "baseline branches");
    assert_eq!(max_page, 16110, "baseline max page bytes");
    assert_eq!(max_entries, 244, "baseline max entries per leaf");
    assert_eq!(guard_hits, 34, "baseline guard hits");
    assert!((avg - 69.06).abs() < 0.01, "baseline avg entries per page");
}
