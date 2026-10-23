//! Round-trip and decode-rejection tests for the reader (ADR-005 §6, §7, §12 items
//! 1 and 4).

use std::sync::atomic::{AtomicU32, Ordering};

use cas_core::{Object, ObjectId, Role};
use cas_store::Store;
use fs_snapshot::{
    encode_directory, encode_extents, encode_inode_table, encode_root, Content, DirEntry, Extent,
    InodeRecord, Kind, PageSink, Profile, RootValue, SnapshotError, SnapshotReader, SnapshotResult,
};

struct TempRoot {
    path: std::path::PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::var_os("WCAS_TEST_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "fs-snapshot-{label}-{}-{sequence}",
                std::process::id()
            ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A store-backed sink, so the reader can fetch what the encoder published.
struct StoreSink<'a> {
    store: &'a Store,
}

impl PageSink for StoreSink<'_> {
    fn put(&mut self, role: Role, value: &[u8]) -> SnapshotResult<ObjectId> {
        Ok(self.store.put(role, value)?.oid)
    }
}

fn store_at(root: &TempRoot) -> Store {
    Store::open(root.path(), cas_store::OpenMode::Create).expect("open store")
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

/// Builds a small tree: root dir with two files and a subdirectory.
fn build_tree(store: &Store, profile: &Profile) -> ObjectId {
    let mut sink = StoreSink { store };

    // /a.txt -> blob "hello"
    let a_bytes = store.put(Role::Bytes, b"hello").expect("put").oid;
    // /big -> three blocks
    let block = vec![7u8; profile.block_bytes as usize];
    let block_oid = store.put(Role::Bytes, &block).expect("put").oid;
    let extents = encode_extents(
        profile,
        std::iter::once(Extent {
            logical_start: 0,
            length: profile.block_bytes,
            object: block_oid,
            source_offset: 0,
        }),
        &mut sink,
    )
    .expect("encode extents")
    .expect("non-empty");

    // /sub -> empty-ish directory holding /sub/inner
    let inner = encode_directory(
        profile,
        std::iter::once(DirEntry {
            name: b"inner".to_vec(),
            child_serial: 3,
        }),
        &mut sink,
    )
    .expect("encode dir")
    .expect("non-empty");

    let sub_entries = vec![DirEntry {
        name: b"inner".to_vec(),
        child_serial: 3,
    }];
    let _ = sub_entries; // the directory above is the same data, kept explicit
    let _ = inner;

    let root_dir = encode_directory(
        profile,
        vec![
            DirEntry {
                name: b"a.txt".to_vec(),
                child_serial: 1,
            },
            DirEntry {
                name: b"big".to_vec(),
                child_serial: 2,
            },
            DirEntry {
                name: b"sub".to_vec(),
                child_serial: 3,
            },
        ]
        .into_iter(),
        &mut sink,
    )
    .expect("encode root dir")
    .expect("non-empty");

    let sub_dir = encode_directory(
        profile,
        std::iter::once(DirEntry {
            name: b"inner".to_vec(),
            child_serial: 4,
        }),
        &mut sink,
    )
    .expect("encode sub")
    .expect("non-empty");
    let _ = sub_dir;

    let records = vec![
        inode(0, Kind::Directory, 0, Content::Dir(root_dir)),
        inode(1, Kind::File, 5, Content::Blob(a_bytes)),
        inode(
            2,
            Kind::File,
            profile.block_bytes,
            Content::Extents(extents),
        ),
        inode(3, Kind::Directory, 0, Content::Empty),
        inode(
            4,
            Kind::File,
            2,
            Content::Blob(store.put(Role::Bytes, b"hi").expect("put").oid),
        ),
    ];
    let table = encode_inode_table(profile, records.into_iter(), &mut sink).expect("encode table");
    encode_root(
        profile,
        &RootValue {
            scope_id: [1u8; 32],
            inode_table: table,
        },
        &mut sink,
    )
    .expect("encode root")
}

// ------------------------------------------------------------------ round trips

#[test]
fn a_snapshot_round_trips_every_inode_field() {
    let root = TempRoot::new("roundtrip");
    let store = store_at(&root);
    let profile = Profile::V1;
    let root_oid = build_tree(&store, &profile);

    let reader = SnapshotReader::open(&store, root_oid, profile).expect("open");
    assert_eq!(reader.scope_id(), [1u8; 32]);

    for serial in 0..=4u64 {
        let record = reader
            .inode(serial)
            .expect("read")
            .unwrap_or_else(|| panic!("inode {serial} must be present"));
        assert_eq!(record.serial, serial);
        assert_eq!(record.created_ns, 1_000 + serial as i64);
        assert_eq!(record.modified_ns, 2_000 + serial as i64);
    }

    let a = reader.inode(1).expect("read").expect("present");
    assert_eq!(a.kind, Kind::File);
    assert_eq!(a.size, 5);
    let Content::Blob(oid) = a.content else {
        panic!("a.txt must be a blob, got {:?}", a.content);
    };
    let value = store.get(oid).expect("get");
    assert_eq!(Object::decode(&value).expect("decode").value(), b"hello");
}

#[test]
fn a_missing_serial_reads_as_none() {
    let root = TempRoot::new("missing");
    let store = store_at(&root);
    let profile = Profile::V1;
    let root_oid = build_tree(&store, &profile);
    let reader = SnapshotReader::open(&store, root_oid, profile).expect("open");
    assert!(reader.inode(99).expect("read").is_none());
}

#[test]
fn directory_lookup_and_listing_agree() {
    let root = TempRoot::new("dir");
    let store = store_at(&root);
    let profile = Profile::V1;
    let root_oid = build_tree(&store, &profile);
    let reader = SnapshotReader::open(&store, root_oid, profile).expect("open");

    let root_dir = reader.inode(0).expect("read").expect("present");
    assert_eq!(reader.lookup(&root_dir, b"a.txt").expect("lookup"), Some(1));
    assert_eq!(reader.lookup(&root_dir, b"big").expect("lookup"), Some(2));
    assert_eq!(reader.lookup(&root_dir, b"sub").expect("lookup"), Some(3));
    assert_eq!(reader.lookup(&root_dir, b"nope").expect("lookup"), None);

    let all = reader.read_dir(&root_dir, None, 100).expect("list");
    let names: Vec<&[u8]> = all.iter().map(|entry| entry.name.as_slice()).collect();
    assert_eq!(names, vec![&b"a.txt"[..], &b"big"[..], &b"sub"[..]]);
    assert_eq!(
        all.iter().map(|e| e.child_serial).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[test]
fn listing_honours_after_and_limit() {
    let root = TempRoot::new("paging");
    let store = store_at(&root);
    let profile = Profile::V1;
    let root_oid = build_tree(&store, &profile);
    let reader = SnapshotReader::open(&store, root_oid, profile).expect("open");
    let root_dir = reader.inode(0).expect("read").expect("present");

    let after = reader
        .read_dir(&root_dir, Some(b"a.txt"), 100)
        .expect("list");
    assert_eq!(after.len(), 2, "a.txt is excluded");
    assert_eq!(after[0].name, b"big");

    let limited = reader.read_dir(&root_dir, None, 1).expect("list");
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].name, b"a.txt");
}

#[test]
fn extents_in_range_returns_the_overlapping_runs() {
    let root = TempRoot::new("extents");
    let store = store_at(&root);
    let profile = Profile::V1;
    let root_oid = build_tree(&store, &profile);
    let reader = SnapshotReader::open(&store, root_oid, profile).expect("open");

    let big = reader.inode(2).expect("read").expect("present");
    let hits = reader.extents_in_range(&big, 0, 1).expect("query");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].logical_start, 0);
    assert_eq!(hits[0].length, profile.block_bytes);

    // A range past the end of the file finds nothing.
    let none = reader
        .extents_in_range(&big, profile.block_bytes, profile.block_bytes * 2)
        .expect("query");
    assert!(none.is_empty(), "the extent does not cover the range");
}

// ------------------------------------------------------------- decode rejections

fn expect_error(result: SnapshotResult<SnapshotReader<'_>>) -> SnapshotError {
    match result {
        Ok(_) => panic!("expected a decode failure"),
        Err(error) => error,
    }
}

#[test]
fn a_root_of_the_wrong_role_is_refused() {
    let root = TempRoot::new("wrong-role");
    let store = store_at(&root);
    let profile = Profile::V1;
    // Publish the root's bytes under the wrong role.
    let mut sink = StoreSink { store: &store };
    let table = {
        let records = vec![inode(0, Kind::Directory, 0, Content::Empty)];
        encode_inode_table(&profile, records.into_iter(), &mut sink).expect("table")
    };
    let mut value = vec![profile.fmt];
    value.extend_from_slice(&profile.id());
    value.extend_from_slice(&[0u8; 32]);
    value.extend_from_slice(&0u64.to_be_bytes());
    value.extend_from_slice(table.as_bytes());
    let wrong = store.put(Role::Bytes, &value).expect("put").oid;

    let error = expect_error(SnapshotReader::open(&store, wrong, profile));
    assert!(
        matches!(error, SnapshotError::WrongRole { .. }),
        "a root stored under another role must be refused: {error}"
    );
}

#[test]
fn an_unknown_profile_is_refused() {
    let root = TempRoot::new("profile");
    let store = store_at(&root);
    let profile = Profile::V1;
    let root_oid = build_tree(&store, &profile);

    let mut other = Profile::V1;
    other.mask_bits = 7;
    assert_ne!(
        other.id(),
        profile.id(),
        "a parameter change is a new profile"
    );

    let error = expect_error(SnapshotReader::open(&store, root_oid, other));
    assert!(
        matches!(error, SnapshotError::UnsupportedProfile { .. }),
        "a root from another profile must be refused, not converted: {error}"
    );
}

#[test]
fn a_wrong_format_byte_is_refused() {
    let root = TempRoot::new("fmt");
    let store = store_at(&root);
    let profile = Profile::V1;

    let mut value = vec![99u8]; // not fmt 1
    value.extend_from_slice(&profile.id());
    value.extend_from_slice(&[0u8; 32]);
    value.extend_from_slice(&0u64.to_be_bytes());
    value.extend_from_slice(&[0u8; 32]);
    let oid = store.put(Role::FilesystemRoot, &value).expect("put").oid;

    let error = expect_error(SnapshotReader::open(&store, oid, profile));
    assert!(
        matches!(error, SnapshotError::UnsupportedFormat { found: 99 }),
        "{error}"
    );
}

#[test]
fn a_truncated_root_is_malformed() {
    let root = TempRoot::new("truncated");
    let store = store_at(&root);
    let profile = Profile::V1;

    let mut value = vec![profile.fmt];
    value.extend_from_slice(&profile.id());
    // Stop early: no scope id, no table pointer.
    let oid = store.put(Role::FilesystemRoot, &value).expect("put").oid;

    let error = expect_error(SnapshotReader::open(&store, oid, profile));
    assert!(matches!(error, SnapshotError::Malformed { .. }), "{error}");
}

#[test]
fn trailing_bytes_in_a_root_are_malformed() {
    let root = TempRoot::new("trailing");
    let store = store_at(&root);
    let profile = Profile::V1;

    let mut value = vec![profile.fmt];
    value.extend_from_slice(&profile.id());
    value.extend_from_slice(&[0u8; 32]);
    value.extend_from_slice(&0u64.to_be_bytes());
    value.extend_from_slice(&[0u8; 32]);
    value.push(0xFF); // one byte too many
    let oid = store.put(Role::FilesystemRoot, &value).expect("put").oid;

    let error = expect_error(SnapshotReader::open(&store, oid, profile));
    assert!(matches!(error, SnapshotError::Malformed { .. }), "{error}");
}

// ============================================================================
// P2a-1-QA-1 — multi-page directories (regression tests for QA's report)
//
// `lookup` used to `fetch(pointer, Role::DirectoryLeaf, …)` before looking at the
// role, so a branch page was rejected by the fetch and the `DirectoryBranch` match
// arm was unreachable dead code. Every earlier test used a directory that fit in one
// page, which is why the whole suite was green.
//
// The tests below force a multi-page directory and check both navigation and the
// reader-side bounds QA asked for.
// ============================================================================

/// A directory with enough entries to need branch pages, and a lookup for an entry
/// that can only be found by descending.
fn build_large_directory(store: &Store, profile: &Profile) -> (ObjectId, Vec<DirEntry>) {
    let mut sink = StoreSink { store };
    let entries: Vec<DirEntry> = (0..2_000u64)
        .map(|index| DirEntry {
            name: format!("name-{index:06}").into_bytes(),
            child_serial: index + 1,
        })
        .collect();
    let dir_root = fs_snapshot::encode_directory(profile, entries.iter().cloned(), &mut sink)
        .expect("encode directory")
        .expect("non-empty");

    // The root inode points at that directory; children are only referenced by serial,
    // so the inode table does not need a row per entry for this test.
    let records = vec![InodeRecord {
        serial: 0,
        kind: Kind::Directory,
        size: 0,
        created_ns: 0,
        modified_ns: 0,
        content: Content::Dir(dir_root),
    }];
    let table = fs_snapshot::encode_inode_table(profile, records.into_iter(), &mut sink)
        .expect("encode table");
    let root = fs_snapshot::encode_root(
        profile,
        &RootValue {
            scope_id: [3u8; 32],
            inode_table: table,
        },
        &mut sink,
    )
    .expect("encode root");
    (root, entries)
}

#[test]
fn lookup_works_in_a_directory_larger_than_one_page() {
    let root = TempRoot::new("multipage");
    let store = store_at(&root);
    let profile = Profile::V1;
    let (root_oid, entries) = build_large_directory(&store, &profile);

    let reader = SnapshotReader::open(&store, root_oid, profile).expect("open");
    let dir = reader.inode(0).expect("read").expect("present");

    // First, middle and last: the middle one is only reachable through a branch page.
    for index in [0usize, 1_500, 1_999] {
        let expected = index as u64 + 1;
        let found = reader
            .lookup(&dir, &entries[index].name)
            .unwrap_or_else(|error| panic!("lookup of entry {index} failed: {error}"));
        assert_eq!(
            found,
            Some(expected),
            "entry {index} must resolve through the branch pages"
        );
    }

    // A name that is not present still reports absence, not an error.
    assert_eq!(reader.lookup(&dir, b"name-999999").expect("lookup"), None);
}

#[test]
fn read_dir_lists_every_entry_of_a_multi_page_directory() {
    let root = TempRoot::new("multipage-list");
    let store = store_at(&root);
    let profile = Profile::V1;
    let (root_oid, entries) = build_large_directory(&store, &profile);

    let reader = SnapshotReader::open(&store, root_oid, profile).expect("open");
    let dir = reader.inode(0).expect("read").expect("present");
    let listed = reader.read_dir(&dir, None, 10_000).expect("list");

    assert_eq!(listed.len(), entries.len(), "every entry must be listed");
    // Order is name order, and the walk crosses branch boundaries.
    for (got, want) in listed.iter().zip(entries.iter()) {
        assert_eq!(&got.name, &want.name);
        assert_eq!(got.child_serial, want.child_serial);
    }
}

// ------------------------------------------------------- QA's two observations

/// **QA observation ①** — the read path must reject an extent longer than the
/// profile allows, not only the encoder.
///
/// A forged leaf that claims a huge `length` would otherwise be accepted and handed
/// back to a caller as a real extent.
#[test]
fn reading_an_extent_longer_than_the_profile_allows_is_refused() {
    let root = TempRoot::new("extent-bound");
    let store = store_at(&root);
    let profile = Profile::V1;
    let mut sink = StoreSink { store: &store };

    // Build a leaf by hand: fmt | count=1 | start | length | oid | source_offset,
    // with a length beyond `max_extent_len`.
    let mut value = Vec::new();
    value.push(profile.fmt);
    value.extend_from_slice(&1u32.to_be_bytes());
    value.extend_from_slice(&0u64.to_be_bytes()); // logical_start
    value.extend_from_slice(&(profile.max_extent_len + 1).to_be_bytes()); // length
    value.extend_from_slice(ObjectId::from_bytes([1u8; 32]).as_bytes());
    value.extend_from_slice(&0u64.to_be_bytes()); // source_offset
    let leaf = sink.put(Role::ExtentLeaf, &value).expect("put leaf");

    let records = vec![InodeRecord {
        serial: 0,
        kind: Kind::File,
        size: profile.block_bytes * 2,
        created_ns: 0,
        modified_ns: 0,
        content: Content::Extents(leaf),
    }];
    let table = fs_snapshot::encode_inode_table(&profile, records.into_iter(), &mut sink)
        .expect("encode table");
    let root_oid = fs_snapshot::encode_root(
        &profile,
        &RootValue {
            scope_id: [4u8; 32],
            inode_table: table,
        },
        &mut sink,
    )
    .expect("encode root");

    let reader = SnapshotReader::open(&store, root_oid, profile).expect("open");
    let file = reader.inode(0).expect("read").expect("present");
    let error = reader
        .extents_in_range(&file, 0, 1)
        .expect_err("an over-long extent must be refused on the read path");
    assert!(matches!(error, SnapshotError::Malformed { .. }), "{error}");
}

/// **QA observation ②** — a page larger than the profile allows must be refused when
/// read, because the byte limit is part of the format, not only of the encoder.
#[test]
fn reading_an_oversized_page_is_refused() {
    let root = TempRoot::new("page-bound");
    let store = store_at(&root);
    let profile = Profile::V1;
    let mut sink = StoreSink { store: &store };

    // A directory leaf that declares more entries than the page limit allows.
    let entry_bytes = 1 + 4 + 8; // name length + short name + serial
    let count = (profile.max_page_bytes / entry_bytes + 1) as u32;
    let mut value = Vec::new();
    value.push(profile.fmt);
    value.extend_from_slice(&count.to_be_bytes());
    for _ in 0..count {
        value.push(4);
        value.extend_from_slice(b"aaaa");
        value.extend_from_slice(&1u64.to_be_bytes());
    }
    assert!(
        value.len() > profile.max_page_bytes,
        "the forged page must exceed the limit"
    );
    let dir_root = sink.put(Role::DirectoryLeaf, &value).expect("put leaf");

    let records = vec![InodeRecord {
        serial: 0,
        kind: Kind::Directory,
        size: 0,
        created_ns: 0,
        modified_ns: 0,
        content: Content::Dir(dir_root),
    }];
    let table = fs_snapshot::encode_inode_table(&profile, records.into_iter(), &mut sink)
        .expect("encode table");
    let root_oid = fs_snapshot::encode_root(
        &profile,
        &RootValue {
            scope_id: [5u8; 32],
            inode_table: table,
        },
        &mut sink,
    )
    .expect("encode root");

    let reader = SnapshotReader::open(&store, root_oid, profile).expect("open");
    let dir = reader.inode(0).expect("read").expect("present");
    let error = reader
        .read_dir(&dir, None, 10)
        .expect_err("an oversized page must be refused when read");
    assert!(matches!(error, SnapshotError::Malformed { .. }), "{error}");
}

// ------------------------------------------------- P2a-1-QA-2: two read gaps

/// **QA ①** — the inode-table read path must bound the page size too.
///
/// `lookup`, `read_dir` and `extents_in_range` already checked it; the inode path did
/// not, so a forged inode page could be arbitrarily large.
#[test]
fn reading_an_oversized_inode_page_is_refused() {
    let root = TempRoot::new("inode-bound");
    let store = store_at(&root);
    let profile = Profile::V1;
    let mut sink = StoreSink { store: &store };

    // A leaf that declares far more records than fit in a page. Each record is
    // serial + kind + size + two times + content tag = 34 bytes minimum.
    let count = (profile.max_page_bytes / 34 + 1) as u32;
    let mut value = Vec::new();
    value.push(profile.fmt);
    value.push(0); // node = leaf
    value.extend_from_slice(&count.to_be_bytes());
    for serial in 0..count {
        value.extend_from_slice(&(serial as u64).to_be_bytes());
        value.push(2); // Kind::File
        value.extend_from_slice(&0u64.to_be_bytes());
        value.extend_from_slice(&0i64.to_be_bytes());
        value.extend_from_slice(&0i64.to_be_bytes());
        value.push(0); // Content::Empty
    }
    assert!(
        value.len() > profile.max_page_bytes,
        "the forged page must exceed the limit"
    );
    let table = sink.put(Role::InodeTable, &value).expect("put leaf");
    let root_oid = fs_snapshot::encode_root(
        &profile,
        &RootValue {
            scope_id: [6u8; 32],
            inode_table: table,
        },
        &mut sink,
    )
    .expect("encode root");

    let reader = SnapshotReader::open(&store, root_oid, profile).expect("open");
    let error = reader
        .inode(1)
        .expect_err("an oversized inode page must be refused when read");
    assert!(matches!(error, SnapshotError::Malformed { .. }), "{error}");
}

/// **QA ②** — the read path must reject `source_offset + length` past the largest
/// legal object value.
///
/// The length bound alone does not cover this: a one-block extent can still start at
/// an offset beyond anything an object may hold.
#[test]
fn reading_an_extent_past_the_object_value_limit_is_refused() {
    let root = TempRoot::new("source-bound");
    let store = store_at(&root);
    let profile = Profile::V1;
    let mut sink = StoreSink { store: &store };

    let mut value = Vec::new();
    value.push(profile.fmt);
    value.extend_from_slice(&1u32.to_be_bytes());
    value.extend_from_slice(&0u64.to_be_bytes()); // logical_start
    value.extend_from_slice(&profile.block_bytes.to_be_bytes()); // legal length
    value.extend_from_slice(ObjectId::from_bytes([1u8; 32]).as_bytes());
    // A source offset past the largest legal object value.
    value.extend_from_slice(&(profile.max_object_value + 1).to_be_bytes());
    let leaf = sink.put(Role::ExtentLeaf, &value).expect("put leaf");

    let records = vec![InodeRecord {
        serial: 0,
        kind: Kind::File,
        size: profile.block_bytes,
        created_ns: 0,
        modified_ns: 0,
        content: Content::Extents(leaf),
    }];
    let table = fs_snapshot::encode_inode_table(&profile, records.into_iter(), &mut sink)
        .expect("encode table");
    let root_oid = fs_snapshot::encode_root(
        &profile,
        &RootValue {
            scope_id: [7u8; 32],
            inode_table: table,
        },
        &mut sink,
    )
    .expect("encode root");

    let reader = SnapshotReader::open(&store, root_oid, profile).expect("open");
    let file = reader.inode(0).expect("read").expect("present");
    let error = reader
        .extents_in_range(&file, 0, 1)
        .expect_err("an extent reading past the object limit must be refused");
    assert!(matches!(error, SnapshotError::Malformed { .. }), "{error}");
}
