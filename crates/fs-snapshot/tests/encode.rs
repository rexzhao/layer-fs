//! Round-trip and structural tests for the encoder (ADR-005 §8, §12 items 1–3).

use std::collections::BTreeMap;

use cas_core::{Object, ObjectId, Role};
use fs_snapshot::{
    encode_directory, encode_extents, encode_inode_table, encode_root, Content, DirEntry, Extent,
    InodeRecord, Kind, PageSink, Profile, RootValue, SnapshotResult,
};

/// An in-memory sink that records every published value.
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
    fn get(&self, oid: ObjectId) -> (Role, &[u8]) {
        let (role, value) = self.values.get(&oid).expect("published object");
        (*role, value.as_slice())
    }
}

fn inode(serial: u64, kind: Kind, size: u64, content: Content) -> InodeRecord {
    InodeRecord {
        serial,
        kind,
        size,
        created_ns: 1_000 + serial as i64,
        modified_ns: 2_000 + serial as i64,
        content,
    }
}

/// The root inode must be present, so give every table one.
fn with_root(mut records: Vec<InodeRecord>) -> Vec<InodeRecord> {
    if !records.iter().any(|record| record.serial == 0) {
        records.insert(0, inode(0, Kind::Directory, 0, Content::Empty));
    }
    records
}

#[test]
fn a_small_inode_table_encodes_to_one_leaf_page() {
    let profile = Profile::V1;
    let mut sink = MemorySink::default();
    let records = with_root(vec![inode(
        1,
        Kind::File,
        10,
        Content::Blob(ObjectId::from_bytes([9u8; 32])),
    )]);
    let oid = encode_inode_table(&profile, records.into_iter(), &mut sink).expect("encode");

    let (role, value) = sink.get(oid);
    assert_eq!(role, Role::InodeTable, "one role carries both node kinds");
    assert_eq!(value[0], profile.fmt, "fmt byte first");
    assert_eq!(value[1], 0, "node byte 0 means leaf");
    assert_eq!(&value[2..6], &2u32.to_be_bytes(), "count = 2 records");
}

#[test]
fn an_empty_directory_is_represented_by_none_not_an_empty_page() {
    let profile = Profile::V1;
    let mut sink = MemorySink::default();
    let encoded = encode_directory(&profile, std::iter::empty(), &mut sink).expect("encode");
    assert!(encoded.is_none(), "emptiness is Content::Empty, not a page");
    assert!(sink.values.is_empty(), "nothing was published");
}

#[test]
fn a_directory_round_trips_its_names_and_serials() {
    let profile = Profile::V1;
    let mut sink = MemorySink::default();
    let entries = vec![
        DirEntry {
            name: b"alpha".to_vec(),
            child_serial: 1,
        },
        DirEntry {
            name: b"beta".to_vec(),
            child_serial: 2,
        },
    ];
    let oid = encode_directory(&profile, entries.into_iter(), &mut sink)
        .expect("encode")
        .expect("non-empty");

    let (role, value) = sink.get(oid);
    assert_eq!(role, Role::DirectoryLeaf);
    assert_eq!(value[0], profile.fmt);
    assert_eq!(&value[1..5], &2u32.to_be_bytes());
    // First entry: u8 len | name | u64 serial.
    assert_eq!(value[5], 5, "name length prefix");
    assert_eq!(&value[6..11], b"alpha");
    assert_eq!(&value[11..19], &1u64.to_be_bytes());
}

#[test]
fn extents_round_trip_and_gaps_are_legal() {
    let profile = Profile::V1;
    let mut sink = MemorySink::default();
    let extents = vec![
        Extent {
            logical_start: 0,
            length: profile.block_bytes,
            object: ObjectId::from_bytes([1u8; 32]),
            source_offset: 0,
        },
        // A gap from block_bytes to 3*block_bytes is the compact encoding of zeros.
        Extent {
            logical_start: profile.block_bytes * 3,
            length: profile.block_bytes,
            object: ObjectId::from_bytes([2u8; 32]),
            source_offset: 0,
        },
    ];
    let oid = encode_extents(&profile, extents.into_iter(), &mut sink)
        .expect("encode")
        .expect("non-empty");
    let (role, value) = sink.get(oid);
    assert_eq!(role, Role::ExtentLeaf);
    assert_eq!(&value[1..5], &2u32.to_be_bytes());
}

#[test]
fn overlapping_extents_are_refused() {
    let profile = Profile::V1;
    let mut sink = MemorySink::default();
    let extents = vec![
        Extent {
            logical_start: 0,
            length: 100,
            object: ObjectId::from_bytes([1u8; 32]),
            source_offset: 0,
        },
        Extent {
            logical_start: 50,
            length: 100,
            object: ObjectId::from_bytes([2u8; 32]),
            source_offset: 0,
        },
    ];
    let error = encode_extents(&profile, extents.into_iter(), &mut sink)
        .expect_err("overlap must be refused");
    assert!(
        matches!(error, fs_snapshot::SnapshotError::Unsorted { .. }),
        "{error}"
    );
}

#[test]
fn unsorted_names_are_refused_rather_than_silently_sorted() {
    let profile = Profile::V1;
    let mut sink = MemorySink::default();
    let entries = vec![
        DirEntry {
            name: b"b".to_vec(),
            child_serial: 1,
        },
        DirEntry {
            name: b"a".to_vec(),
            child_serial: 2,
        },
    ];
    let error = encode_directory(&profile, entries.into_iter(), &mut sink).expect_err("unsorted");
    assert!(
        matches!(error, fs_snapshot::SnapshotError::Unsorted { .. }),
        "the encoder must not sort on the caller's behalf: {error}"
    );
}

#[test]
fn names_compare_bytewise_so_case_is_significant() {
    let profile = Profile::V1;
    let mut sink = MemorySink::default();
    // "A" (0x41) < "B" (0x42) < "a" (0x61) bytewise.
    let entries = vec![
        DirEntry {
            name: b"A".to_vec(),
            child_serial: 1,
        },
        DirEntry {
            name: b"B".to_vec(),
            child_serial: 2,
        },
        DirEntry {
            name: b"a".to_vec(),
            child_serial: 3,
        },
    ];
    let oid = encode_directory(&profile, entries.into_iter(), &mut sink)
        .expect("encode")
        .expect("non-empty");
    // Decoding is covered by tests/read.rs; here the point is that byte order, not
    // insertion order, decided the layout — the encoder accepted the stream as given.
    let (role, value) = sink.get(oid);
    assert_eq!(role, Role::DirectoryLeaf);
    assert!(!value.is_empty());
}

#[test]
fn the_root_value_is_exactly_105_bytes() {
    let profile = Profile::V1;
    let mut sink = MemorySink::default();
    let root = RootValue {
        scope_id: [7u8; 32],
        inode_table: ObjectId::from_bytes([3u8; 32]),
    };
    let oid = encode_root(&profile, &root, &mut sink).expect("encode");
    let (role, value) = sink.get(oid);
    assert_eq!(role, Role::FilesystemRoot);
    assert_eq!(value.len(), 105);
    assert_eq!(value[0], profile.fmt);
    assert_eq!(&value[1..33], &profile.id());
    assert_eq!(&value[33..65], &[7u8; 32], "scope id");
    assert_eq!(
        &value[65..73],
        &0u64.to_be_bytes(),
        "root_serial is reserved 0"
    );
    assert_eq!(&value[73..105], ObjectId::from_bytes([3u8; 32]).as_bytes());
}

#[test]
fn an_illegal_kind_content_pair_is_refused() {
    // These two rules are v1-only: under v2 a file's shape follows its chunk count, so a
    // small file may be a Blob and a large one may be too. The v1 profile is still readable,
    // so its normalisation rules must still hold whenever it is the profile in use.
    let profile = Profile::V1;
    // A file smaller than one block must be a Blob.
    let mut sink = MemorySink::default();
    let records = with_root(vec![inode(1, Kind::File, 10, Content::Empty)]);
    let error = encode_inode_table(&profile, records.into_iter(), &mut sink)
        .expect_err("non-canonical shape");
    assert!(
        matches!(error, fs_snapshot::SnapshotError::Malformed { .. }),
        "{error}"
    );

    // A file at or above one block must not be a Blob.
    let mut sink = MemorySink::default();
    let records = with_root(vec![inode(
        1,
        Kind::File,
        profile.block_bytes,
        Content::Blob(ObjectId::from_bytes([1u8; 32])),
    )]);
    let error = encode_inode_table(&profile, records.into_iter(), &mut sink)
        .expect_err("non-canonical shape");
    assert!(
        matches!(error, fs_snapshot::SnapshotError::Malformed { .. }),
        "{error}"
    );
}

#[test]
fn an_empty_inode_table_is_refused() {
    let profile = Profile::V1;
    let mut sink = MemorySink::default();
    let error = encode_inode_table(&profile, std::iter::empty(), &mut sink)
        .expect_err("the root inode always exists");
    assert!(
        matches!(error, fs_snapshot::SnapshotError::Malformed { .. }),
        "{error}"
    );
}

// ---------------------------------------------------------------- history independence

/// The property the whole paging design exists for (ADR-005 §4, §12 item 2).
///
/// The same entry set, inserted in many different orders, must encode to the same
/// root object id. A pager that cut every N entries would fail this.
#[test]
fn the_root_id_is_independent_of_insertion_order() {
    let profile = Profile::V1;
    let mut reference: Option<ObjectId> = None;

    for attempt in 0..200u32 {
        // Build the same set, permuted differently each time.
        let mut serials: Vec<u64> = (1..=300).collect();
        // A cheap deterministic shuffle: stride by a number coprime with the length
        // and rotate by the attempt.
        let stride = 7u64;
        let start = (attempt as usize * 13) % serials.len();
        let mut shuffled = Vec::with_capacity(serials.len());
        for index in 0..serials.len() {
            shuffled.push(serials[(start + index * stride as usize) % serials.len()]);
        }
        serials = shuffled;

        // Insert in that order into a map, then emit sorted — the encoder requires
        // sorted input, and a real caller sorts after building.
        let mut map: BTreeMap<u64, InodeRecord> = BTreeMap::new();
        for serial in serials {
            map.insert(
                serial,
                inode(
                    serial,
                    Kind::File,
                    5,
                    Content::Blob(ObjectId::from_bytes([1u8; 32])),
                ),
            );
        }
        let records = with_root(map.into_values().collect());

        let mut sink = MemorySink::default();
        let oid = encode_inode_table(&profile, records.into_iter(), &mut sink).expect("encode");
        match reference {
            None => reference = Some(oid),
            Some(expected) => assert_eq!(
                oid, expected,
                "attempt {attempt}: the same set produced a different root id"
            ),
        }
    }
}

/// Locality (ADR-005 §4, §12 item 3): changing one value rewrites only the path from
/// that leaf to the root.
///
/// **This test does not distinguish the two paging rules.** Changing a value leaves
/// the key set, the entry count and every position untouched, so a fixed-count pager
/// also rewrites only the edited page and its ancestors. Architect pointed this out
/// after the first version of the doc comment wrongly claimed the opposite.
///
/// The insertion and deletion tests carry the mutation evidence; this one pins the
/// weaker property that a change of value is local at all, under whichever rule.
#[test]
fn changing_one_value_rewrites_only_its_path() {
    let profile = Profile::V1;
    let make = |changed: Option<u64>| {
        let records: Vec<InodeRecord> = (0..1000u64)
            .map(|serial| {
                let size = if serial == 0 { 0 } else { 5 };
                let content = if serial == 0 {
                    Content::Empty
                } else if Some(serial) == changed {
                    Content::Blob(ObjectId::from_bytes([0xFF; 32]))
                } else {
                    Content::Blob(ObjectId::from_bytes([1u8; 32]))
                };
                inode(
                    serial,
                    if serial == 0 {
                        Kind::Directory
                    } else {
                        Kind::File
                    },
                    size,
                    content,
                )
            })
            .collect();
        let mut sink = MemorySink::default();
        let root = encode_inode_table(&profile, records.into_iter(), &mut sink).expect("encode");
        (root, sink)
    };

    let (root_a, sink_a) = make(None);
    let (root_b, sink_b) = make(Some(500));

    assert_ne!(root_a, root_b, "a value change must change the root");

    // ADR-005 §12 item 3: changing one value must add at most "tree depth + 2" pages.
    // A pager that cuts every N entries relocates every page after the edit, so it
    // fails this by a wide margin — which is exactly why the assertion is tight.
    let new_pages = sink_b
        .values
        .keys()
        .filter(|oid| !sink_a.values.contains_key(*oid))
        .count();

    // Depth of the tree: 1000 entries at ~64 per page is 2 levels (leaf + root).
    let depth = 2usize;
    assert!(
        new_pages <= depth + 2,
        "changing one value rewrote {new_pages} pages; the locality bound is {}",
        depth + 2
    );
}

/// Re-encoding an unchanged tree publishes byte-identical pages (ADR-005 §4), which
/// is what lets a full re-encode avoid rewriting the store.
#[test]
fn re_encoding_the_same_tree_yields_the_same_objects() {
    let profile = Profile::V1;
    let build = || {
        let records: Vec<InodeRecord> = (0..500u64)
            .map(|serial| {
                inode(
                    serial,
                    if serial == 0 {
                        Kind::Directory
                    } else {
                        Kind::File
                    },
                    if serial == 0 { 0 } else { 5 },
                    if serial == 0 {
                        Content::Empty
                    } else {
                        Content::Blob(ObjectId::from_bytes([1u8; 32]))
                    },
                )
            })
            .collect();
        let mut sink = MemorySink::default();
        let root = encode_inode_table(&profile, records.into_iter(), &mut sink).expect("encode");
        (root, sink)
    };
    let (root_a, sink_a) = build();
    let (root_b, sink_b) = build();
    assert_eq!(root_a, root_b);
    assert_eq!(sink_a.values.len(), sink_b.values.len());
    for (oid, (role, value)) in &sink_a.values {
        let (other_role, other_value) = sink_b.values.get(oid).expect("same pages");
        assert_eq!(role, other_role);
        assert_eq!(value, other_value);
    }
}

/// Locality for **insertion** (ADR-005 §12 item 3, "插 1 条键").
///
/// The insertion must land in the **middle** of the key range. With a fixed-count
/// pager every page is a window over positions, so one insertion shifts every later
/// entry by one place and *every* page after the edit becomes a different set of
/// entries — all of them new objects. Appending at the end touches only the last
/// page and therefore cannot distinguish the two paging rules at all.
///
/// This was wrong in the first version of this test: it inserted serial 500,000 into
/// keys 0..999 and called that "the middle". Architect caught it; the assertions
/// below now check the insertion really is interior, so the mistake cannot silently
/// return. A change-of-value edit, by contrast, keeps positions and entry counts
/// fixed, so it perturbs one page under *both* rules and distinguishes nothing —
/// which is why only the insert and delete tests carry mutation evidence.
///
/// Measured rewrite counts for one interior edit:
///
/// | edit | fixed 8 entries per page | content-determined paging |
/// | --- | --- | --- |
/// | interior insert (2,000 inodes) | **146 new pages** | 2 new pages |
/// | interior delete (1,000 inodes) | **75 new pages** | 2 new pages |
/// | value change | 4 new pages | 2 new pages |
///
/// The two paging rules are equally history independent; they differ in *locality*.
#[test]
fn inserting_one_key_rewrites_only_its_neighbourhood() {
    let profile = Profile::V1;
    // An even key set with a hole at 1001, so inserting 1001 puts it in the middle.
    let build = |insert: bool| {
        let mut records: Vec<InodeRecord> = (0..2000u64)
            .filter(|serial| *serial != 1001)
            .map(|serial| {
                inode(
                    serial,
                    if serial == 0 {
                        Kind::Directory
                    } else {
                        Kind::File
                    },
                    if serial == 0 { 0 } else { 5 },
                    if serial == 0 {
                        Content::Empty
                    } else {
                        Content::Blob(ObjectId::from_bytes([1u8; 32]))
                    },
                )
            })
            .collect();
        if insert {
            records.push(inode(
                1001,
                Kind::File,
                5,
                Content::Blob(ObjectId::from_bytes([1u8; 32])),
            ));
            records.sort_by_key(|record| record.serial);
        }
        // Guard against the test silently degrading back into an append: the new
        // key must not be the first or the last one.
        if insert {
            let position = records
                .iter()
                .position(|record| record.serial == 1001)
                .expect("the inserted key must be present");
            assert!(
                position > 0 && position + 1 < records.len(),
                "the inserted key sits at position {position} of {}: an interior \
                 insertion is required, end insertion proves nothing",
                records.len()
            );
        }
        let mut sink = MemorySink::default();
        let root = encode_inode_table(&profile, records.into_iter(), &mut sink).expect("encode");
        (root, sink)
    };

    let (root_a, sink_a) = build(false);
    let (root_b, sink_b) = build(true);
    assert_ne!(root_a, root_b, "an insertion must change the root");

    let new_pages = sink_b
        .values
        .keys()
        .filter(|oid| !sink_a.values.contains_key(*oid))
        .count();

    // With content paging an insertion perturbs at most two leaves and their path to
    // the root. The bound is generous but far below "every page after the insertion".
    let depth = 2usize;
    assert!(
        new_pages <= depth + 2,
        "inserting one key rewrote {new_pages} pages; the locality bound is {} \
         (fixed-count paging fails this by construction)",
        depth + 2
    );
}

/// Deleting one key is equally local.
///
/// The removed key is interior to the range, for the same reason the insertion test
/// needs an interior insert: dropping the last key leaves every earlier page intact
/// under both rules.
#[test]
fn deleting_one_key_rewrites_only_its_neighbourhood() {
    let profile = Profile::V1;
    let removed = 500u64;
    let build = |remove: bool| {
        let mut records: Vec<InodeRecord> = (0..1000u64)
            .map(|serial| {
                inode(
                    serial,
                    if serial == 0 {
                        Kind::Directory
                    } else {
                        Kind::File
                    },
                    if serial == 0 { 0 } else { 5 },
                    if serial == 0 {
                        Content::Empty
                    } else {
                        Content::Blob(ObjectId::from_bytes([1u8; 32]))
                    },
                )
            })
            .collect();
        if remove {
            // An interior key, not the last one.
            assert!(
                records.iter().any(|record| record.serial == removed)
                    && removed < records.last().expect("non-empty").serial,
                "the removed key must be interior to the range"
            );
            records.retain(|record| record.serial != removed);
        }
        let mut sink = MemorySink::default();
        let root = encode_inode_table(&profile, records.into_iter(), &mut sink).expect("encode");
        (root, sink)
    };

    let (root_a, sink_a) = build(false);
    let (root_b, sink_b) = build(true);
    assert_ne!(root_a, root_b);

    let new_pages = sink_b
        .values
        .keys()
        .filter(|oid| !sink_a.values.contains_key(*oid))
        .count();
    let depth = 2usize;
    assert!(
        new_pages <= depth + 2,
        "deleting one key rewrote {new_pages} pages; the locality bound is {}",
        depth + 2
    );
}

/// Under **v2** the shapes follow the chunk count (ADR-007 §3.2), not the size.
///
/// The v1 rules above stay in force for v1 roots; this is what replaces them for the profile
/// this build writes: a one-chunk file is a Blob at any size, and a many-chunk file is
/// Extents at any size. The one absolute rule is that a zero-length file is `Empty`.
#[test]
fn the_current_profile_accepts_chunk_shaped_files() {
    let profile = fs_snapshot::Profile::CURRENT;

    // A small file as a Blob: legal under v2 (illegal under v1).
    let mut sink = MemorySink::default();
    let records = with_root(vec![inode(
        1,
        Kind::File,
        10,
        Content::Blob(ObjectId::from_bytes([1u8; 32])),
    )]);
    encode_inode_table(&profile, records.into_iter(), &mut sink)
        .expect("a one-chunk file is a Blob at any size");

    // A large file as Extents: legal under v2.
    let mut sink = MemorySink::default();
    let records = with_root(vec![inode(
        1,
        Kind::File,
        500_000,
        Content::Extents(ObjectId::from_bytes([2u8; 32])),
    )]);
    encode_inode_table(&profile, records.into_iter(), &mut sink)
        .expect("a many-chunk file is Extents");

    // A zero-length Blob stays illegal: Empty is the only canonical zero-length shape.
    let mut sink = MemorySink::default();
    let records = with_root(vec![inode(
        1,
        Kind::File,
        0,
        Content::Blob(ObjectId::from_bytes([3u8; 32])),
    )]);
    let error = encode_inode_table(&profile, records.into_iter(), &mut sink)
        .expect_err("a zero-length file must be Empty");
    assert!(
        matches!(error, fs_snapshot::SnapshotError::Malformed { .. }),
        "{error}"
    );
}
