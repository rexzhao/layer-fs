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
