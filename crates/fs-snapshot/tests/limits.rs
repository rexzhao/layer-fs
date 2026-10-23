//! Page-size limits and deep-tree behaviour (ADR-005 §4, §7 item 6).

use std::collections::BTreeMap;

use cas_core::{Object, ObjectId, Role};
use fs_snapshot::{
    encode_directory, encode_extents, encode_inode_table, Content, DirEntry, Extent, InodeRecord,
    Kind, PageSink, Profile, SnapshotResult,
};

#[derive(Debug, Default)]
struct MemorySink {
    values: BTreeMap<ObjectId, (Role, Vec<u8>)>,
}

impl PageSink for MemorySink {
    fn put(&mut self, role: Role, value: &[u8]) -> SnapshotResult<ObjectId> {
        let object = Object::new(role, value).expect("value within limits");
        let oid = object.id().expect("id");
        self.values.insert(oid, (role, value.to_vec()));
        Ok(oid)
    }
}

impl MemorySink {
    fn largest_page(&self) -> usize {
        self.values
            .values()
            .map(|(_, value)| value.len())
            .max()
            .unwrap_or(0)
    }
}

fn file_inode(serial: u64) -> InodeRecord {
    InodeRecord {
        serial,
        kind: Kind::File,
        size: 5,
        created_ns: 1_000,
        modified_ns: 2_000,
        content: Content::Blob(ObjectId::from_bytes([1u8; 32])),
    }
}

/// **The page byte limit is a hard bound** (ADR-005 §7 item 6).
///
/// The first implementation checked *after* appending, so a page could overshoot by
/// one entry; the scale run caught it at 16,440 bytes against a 16,384 limit. This
/// test pins the bound so the mistake cannot come back.
#[test]
fn no_page_exceeds_the_profile_limit() {
    let profile = Profile::V1;

    // Inode table with many pages.
    let mut sink = MemorySink::default();
    let mut records = vec![InodeRecord {
        serial: 0,
        kind: Kind::Directory,
        size: 0,
        created_ns: 0,
        modified_ns: 0,
        content: Content::Empty,
    }];
    for serial in 1..50_000u64 {
        records.push(file_inode(serial));
    }
    encode_inode_table(&profile, records.into_iter(), &mut sink).expect("encode inodes");
    assert!(
        sink.largest_page() <= profile.max_page_bytes,
        "an inode page was {} bytes against a {} limit",
        sink.largest_page(),
        profile.max_page_bytes
    );

    // Directory with long names, which is what stresses the entry size guard.
    let mut sink = MemorySink::default();
    let long = "x".repeat(profile.name_max - 8);
    let entries: Vec<DirEntry> = (0..20_000u64)
        .map(|index| DirEntry {
            name: format!("{long}{index:08}").into_bytes(),
            child_serial: index,
        })
        .collect();
    encode_directory(&profile, entries.into_iter(), &mut sink).expect("encode directory");
    assert!(
        sink.largest_page() <= profile.max_page_bytes,
        "a directory page was {} bytes against a {} limit",
        sink.largest_page(),
        profile.max_page_bytes
    );
}

/// A name at the profile limit still fits: the guard must not be so conservative
/// that it produces one entry per page.
#[test]
fn maximum_length_names_still_pack_several_per_page() {
    let profile = Profile::V1;
    let mut sink = MemorySink::default();
    let long = "y".repeat(profile.name_max - 4);
    let entries: Vec<DirEntry> = (0..1_000u64)
        .map(|index| DirEntry {
            name: format!("{long}{index:04}").into_bytes(),
            child_serial: index,
        })
        .collect();
    encode_directory(&profile, entries.into_iter(), &mut sink).expect("encode");
    // 1,000 entries of ~260 bytes is ~260 KiB of leaves; at a 16 KiB page limit that
    // needs at least 16 pages but nowhere near 1,000.
    let pages = sink.values.len();
    assert!(
        (16..200).contains(&pages),
        "expected packing between 16 and 200 pages, got {pages}"
    );
}

/// A deep inode table is still readable — the reader descends by first-key.
#[test]
fn a_multi_level_inode_table_encodes_to_a_branch_root() {
    let profile = Profile::V1;
    let mut sink = MemorySink::default();
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

    let (role, value) = sink.values.get(&root).expect("root page");
    assert_eq!(*role, Role::InodeTable);
    assert_eq!(value[0], profile.fmt);
    assert_eq!(
        value[1], 1,
        "the root of a large table is a branch (node = 1)"
    );
    assert!(value[2] >= 1, "branch height is at least 1");
}

/// Zero-length extents are refused; a page of them would mean an interval that
/// occupies no bytes while claiming an object.
#[test]
fn a_zero_length_extent_is_refused() {
    let profile = Profile::V1;
    let mut sink = MemorySink::default();
    let extents = vec![Extent {
        logical_start: 0,
        length: 0,
        object: ObjectId::from_bytes([1u8; 32]),
        source_offset: 0,
    }];
    let error = encode_extents(&profile, extents.into_iter(), &mut sink).expect_err("zero length");
    assert!(
        matches!(error, fs_snapshot::SnapshotError::Malformed { .. }),
        "{error}"
    );
}
