//! Scale benchmark for the snapshot encoder (ADR-005 §12 item 8).
//!
//! Reports page counts, tree depth and encode time for the three shapes the ADR
//! names. This is **data, not a gate**: nothing here asserts a timing, because the
//! ADR explicitly refuses to promise numbers before measuring them.
//!
//! Run with:
//!
//! ```sh
//! cargo run -p fs-snapshot --release --example scale
//! ```

use std::time::Instant;

use cas_core::{Object, ObjectId, Role};
use fs_snapshot::{
    encode_directory, encode_extents, encode_inode_table, Content, DirEntry, Extent, InodeRecord,
    Kind, PageSink, Profile, SnapshotResult,
};

/// What the sink saw, enough to compute fanout and depth.
#[derive(Default, Debug)]
struct CountingSink {
    total: usize,
    bytes: usize,
    max_page_bytes: usize,
    min_page_bytes: usize,
}

impl CountingSink {
    fn record(&mut self, value: &[u8]) {
        self.total += 1;
        self.bytes += value.len();
        if self.total == 1 || value.len() < self.min_page_bytes {
            self.min_page_bytes = value.len();
        }
        if value.len() > self.max_page_bytes {
            self.max_page_bytes = value.len();
        }
    }
}

impl PageSink for CountingSink {
    fn put(&mut self, role: Role, value: &[u8]) -> SnapshotResult<ObjectId> {
        // Hash the value so the encoder does the same work it would against a store.
        let object = Object::new(role, value).expect("value within limits");
        let oid = object.id().expect("id");
        self.record(value);
        Ok(oid)
    }
}

fn file_inode(serial: u64) -> InodeRecord {
    InodeRecord {
        serial,
        kind: Kind::File,
        size: 5,
        created_ns: 1_000,
        modified_ns: 2_000,

        accessed_ns: 0,
        changed_ns: 0,
        attributes: 0,
        security: Vec::new(),
        content: Content::Blob(ObjectId::from_bytes([1u8; 32])),
    }
}

fn measure<F>(label: &str, entries: usize, body: F)
where
    F: FnOnce(&mut CountingSink),
{
    let mut sink = CountingSink::default();
    let started = Instant::now();
    body(&mut sink);
    let elapsed = started.elapsed();
    let fanout = entries as f64 / sink.total as f64;
    println!(
        "{label:<22} entries={entries:<7} pages={:<7} fanout={fanout:>6.1} \
         bytes={:<9} encode={:>9.1}ms page=[{},{})",
        sink.total,
        sink.bytes,
        elapsed.as_secs_f64() * 1_000.0,
        sink.min_page_bytes,
        sink.max_page_bytes
    );
}

fn main() {
    let profile = Profile::V1;
    println!(
        "profile mask_bits={} min_entries={} max_page_bytes={}",
        profile.mask_bits, profile.min_entries, profile.max_page_bytes
    );

    measure("inode table", 200_000, |sink| {
        let mut records = Vec::with_capacity(200_000);
        records.push(InodeRecord {
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
        });
        for serial in 1..200_000u64 {
            records.push(file_inode(serial));
        }
        encode_inode_table(&profile, records.into_iter(), sink).expect("encode");
    });

    measure("directory (one dir)", 100_000, |sink| {
        let entries: Vec<DirEntry> = (0..100_000u64)
            .map(|index| DirEntry {
                name: format!("entry-{index:06}").into_bytes(),
                child_serial: index + 1,
            })
            .collect();
        encode_directory(&profile, entries.into_iter(), sink).expect("encode");
    });

    // 1 GiB at 64 KiB blocks = 16 384 extents, the ADR's example.
    measure("1 GiB extent tree", 16_384, |sink| {
        let extents: Vec<Extent> = (0..16_384u64)
            .map(|index| Extent {
                logical_start: index * profile.block_bytes,
                length: profile.block_bytes,
                object: ObjectId::from_bytes([(index % 251) as u8; 32]),
                source_offset: 0,
            })
            .collect();
        encode_extents(&profile, extents.into_iter(), sink).expect("encode");
    });
}
