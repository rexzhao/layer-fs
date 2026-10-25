//! `references` tests: the edge primitive garbage collection marks with (P2b-6 §11, §12b).

use std::sync::atomic::{AtomicU32, Ordering};

use cas_core::{ObjectId, Role};
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
        let path = std::env::temp_dir().join(format!(
            "fs-snapshot-references-{label}-{}-{sequence}",
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

struct StoreSink<'a> {
    store: &'a Store,
}

impl PageSink for StoreSink<'_> {
    fn put(&mut self, role: Role, value: &[u8]) -> SnapshotResult<ObjectId> {
        Ok(self.store.put(role, value)?.oid)
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

/// Every object id the fixture plants, recorded **as it is created**.
///
/// The point of collecting them here rather than by walking is the same as §8's hand-built ground
/// truth: expectations must not be produced by the code under test.
struct Expected {
    root: ObjectId,
    table: ObjectId,
    dir_leaf: ObjectId,
    blob: ObjectId,
    chunk: ObjectId,
    /// Two extent trees that point at the *same* chunk, for the shared-child case.
    extent_leaf_a: ObjectId,
    extent_leaf_b: ObjectId,
}

/// root dir (leaf) with two files: one blob, one extent tree; plus an empty directory inode.
///
/// `file_a` and `file_b` share one chunk object, which is what C11' is about.
fn fixture(store: &Store, profile: &Profile) -> Expected {
    let mut sink = StoreSink { store };
    let blob = store.put(Role::Bytes, b"hello").expect("put").oid;
    let chunk = store
        .put(Role::Bytes, &vec![7u8; profile.block_bytes as usize])
        .expect("put")
        .oid;

    let shared_extent = || Extent {
        logical_start: 0,
        length: profile.block_bytes,
        object: chunk,
        source_offset: 0,
    };
    let extent_leaf_a = encode_extents(profile, std::iter::once(shared_extent()), &mut sink)
        .expect("encode extents")
        .expect("non-empty");
    // Deliberately a *different* leaf over the **same** chunk: two extents, both reading the one
    // chunk object. Encoding a second identical leaf would collapse into one object id (content
    // addressing) and the shared-parent case would quietly become a shared-self case.
    let extent_leaf_b = encode_extents(
        profile,
        vec![
            shared_extent(),
            Extent {
                logical_start: profile.block_bytes,
                length: profile.block_bytes,
                object: chunk,
                source_offset: 0,
            },
        ]
        .into_iter(),
        &mut sink,
    )
    .expect("encode extents")
    .expect("non-empty");

    let dir_leaf = encode_directory(
        profile,
        vec![
            DirEntry {
                name: b"a".to_vec(),
                child_serial: 1,
            },
            DirEntry {
                name: b"b".to_vec(),
                child_serial: 2,
            },
            DirEntry {
                name: b"empty".to_vec(),
                child_serial: 3,
            },
        ]
        .into_iter(),
        &mut sink,
    )
    .expect("encode dir")
    .expect("non-empty");

    let records = vec![
        inode(0, Kind::Directory, 0, Content::Dir(dir_leaf)),
        inode(1, Kind::File, 5, Content::Blob(blob)),
        inode(
            2,
            Kind::File,
            profile.block_bytes,
            Content::Extents(extent_leaf_a),
        ),
        // A second file over the same chunk: the shared-subtree case.
        inode(
            3,
            Kind::File,
            profile.block_bytes,
            Content::Extents(extent_leaf_b),
        ),
        // An empty directory and a zero-length file both carry `Empty`.
        inode(4, Kind::Directory, 0, Content::Empty),
        inode(5, Kind::File, 0, Content::Empty),
    ];
    let table = encode_inode_table(profile, records.into_iter(), &mut sink).expect("encode table");
    let root = encode_root(
        profile,
        &RootValue {
            scope_id: [1u8; 32],
            inode_table: table,
        },
        &mut sink,
    )
    .expect("encode root");

    Expected {
        root,
        table,
        dir_leaf,
        blob,
        chunk,
        extent_leaf_a,
        extent_leaf_b,
    }
}

fn set(ids: Vec<ObjectId>) -> std::collections::BTreeSet<ObjectId> {
    ids.into_iter().collect()
}

// ------------------------------------------------------------------- postive cases

#[test]
fn a_root_references_exactly_its_inode_table() {
    let root = TempRoot::new("root");
    let store = Store::open(root.path(), cas_store::OpenMode::Create).expect("open");
    let profile = Profile::V1;
    let expected = fixture(&store, &profile);

    let reader = SnapshotReader::open(&store, expected.root, profile).expect("open reader");
    assert_eq!(
        reader.references(expected.root).expect("refs"),
        vec![expected.table]
    );
}

/// C10': a file's inode must yield its **content** objects. If only directory children came back,
/// every file's bytes would look unreachable and one sweep would delete them all.
#[test]
fn an_inode_leaf_references_every_content_root() {
    let root = TempRoot::new("inodes");
    let store = Store::open(root.path(), cas_store::OpenMode::Create).expect("open");
    let profile = Profile::V1;
    let expected = fixture(&store, &profile);

    let reader = SnapshotReader::open(&store, expected.root, profile).expect("open reader");
    let refs = set(reader.references(expected.table).expect("refs"));

    // Exactly the six content roots (two of which are `Empty` and contribute nothing).
    assert_eq!(
        refs,
        set(vec![
            expected.dir_leaf,
            expected.blob,
            expected.extent_leaf_a,
            expected.extent_leaf_b,
        ]),
        "an inode leaf must reference every content root and nothing else"
    );
}

#[test]
fn an_extent_leaf_references_the_chunks_it_points_at() {
    let root = TempRoot::new("extents");
    let store = Store::open(root.path(), cas_store::OpenMode::Create).expect("open");
    let profile = Profile::V1;
    let expected = fixture(&store, &profile);

    let reader = SnapshotReader::open(&store, expected.root, profile).expect("open reader");
    assert_eq!(
        set(reader.references(expected.extent_leaf_a).expect("refs")),
        set(vec![expected.chunk])
    );
}

/// A directory page holds inode **serials**, not object ids. Returning nothing here is correct and
/// the reason the inode table level is where the `serial -> content` edge is collected.
#[test]
fn a_directory_leaf_references_no_objects() {
    let root = TempRoot::new("dirdir");
    let store = Store::open(root.path(), cas_store::OpenMode::Create).expect("open");
    let profile = Profile::V1;
    let expected = fixture(&store, &profile);

    let reader = SnapshotReader::open(&store, expected.root, profile).expect("open reader");
    assert!(
        reader
            .references(expected.dir_leaf)
            .expect("refs")
            .is_empty(),
        "a directory leaf must reference no objects: its entries are serials"
    );
}

/// `Bytes`/`Chunk` are the graph's leaves: their bytes are the payload, and a non-empty result here
/// would mean the walker keeps descending into data.
#[test]
fn a_byte_object_references_nothing() {
    let root = TempRoot::new("bytes");
    let store = Store::open(root.path(), cas_store::OpenMode::Create).expect("open");
    let profile = Profile::V1;
    let expected = fixture(&store, &profile);

    let reader = SnapshotReader::open(&store, expected.root, profile).expect("open reader");
    assert!(reader.references(expected.blob).expect("refs").is_empty());
    assert!(reader.references(expected.chunk).expect("refs").is_empty());
}

/// C11': two parents over one object must each see it. Nothing is remembered between calls — if it
/// were, the second parent would report "no children" and the shared subtree would be swept.
#[test]
fn a_shared_object_is_reported_for_both_parents() {
    let root = TempRoot::new("shared");
    let store = Store::open(root.path(), cas_store::OpenMode::Create).expect("open");
    let profile = Profile::V1;
    let expected = fixture(&store, &profile);

    let reader = SnapshotReader::open(&store, expected.root, profile).expect("open reader");
    assert_eq!(
        set(reader.references(expected.extent_leaf_a).expect("refs")),
        set(vec![expected.chunk]),
        "first parent must see the shared chunk"
    );
    assert_eq!(
        set(reader.references(expected.extent_leaf_b).expect("refs")),
        set(vec![expected.chunk]),
        "second parent must see the same chunk; a cache here would hide it"
    );
}

/// C8': the call is a pure read — same oid, same list, and the store's bytes do not move.
#[test]
fn references_is_idempotent_and_writes_nothing() {
    let root = TempRoot::new("pure");
    let store = Store::open(root.path(), cas_store::OpenMode::Create).expect("open");
    let profile = Profile::V1;
    let expected = fixture(&store, &profile);

    let before = fingerprint(root.path());
    let reader = SnapshotReader::open(&store, expected.root, profile).expect("open reader");
    let first = reader.references(expected.table).expect("refs");
    let second = reader.references(expected.table).expect("refs");
    assert_eq!(first, second, "references must be idempotent");
    let after = fingerprint(root.path());
    assert_eq!(before, after, "references must not write to the store");
}

/// `{(relative path, length)}` over the store directory: an external measure, so "writes nothing"
/// cannot be satisfied by the code under test lying about its own bookkeeping.
fn fingerprint(dir: &std::path::Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).expect("read dir").flatten() {
            let meta = entry.metadata().expect("metadata");
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                let relative = entry
                    .path()
                    .strip_prefix(dir)
                    .expect("inside")
                    .to_string_lossy()
                    .replace('\\', "/");
                // Shared memory between connections has no durable content; ADR-008 §6.1.
                if !relative.ends_with("-shm") {
                    out.push((relative, meta.len()));
                }
            }
        }
    }
    out.sort();
    out
}

// ------------------------------------------------------------------- damage is not emptiness

/// §12: a damaged object must be an **error**, never "no children".
///
/// The positive control is in the same test: the same call on a sound table succeeds and returns a
/// non-empty list. Without it, a `references` that always failed would "pass" this test.
#[test]
fn a_damaged_object_is_an_error_not_an_empty_leaf() {
    let root = TempRoot::new("damaged");
    let store = Store::open(root.path(), cas_store::OpenMode::Create).expect("open");
    let profile = Profile::V1;
    let expected = fixture(&store, &profile);
    let reader = SnapshotReader::open(&store, expected.root, profile).expect("open reader");

    // Positive control: the sound object answers.
    assert!(!reader
        .references(expected.table)
        .expect("sound table")
        .is_empty());

    // A value published as an inode table but truncated: a well-formed header claiming one record,
    // then nothing. The format byte is right, so the failure is the truncated record itself and not
    // an earlier refusal.
    let damaged = store
        .put(Role::InodeTable, &[profile.fmt, 0u8, 1, 0, 0, 0])
        .expect("put")
        .oid;
    let error = reader
        .references(damaged)
        .expect_err("a truncated inode page must not read as an empty leaf");
    assert!(
        matches!(error, SnapshotError::Malformed { .. }),
        "expected a typed Malformed, got {error:?}"
    );
}

// ------------------------------------------------------------------- counts reconcile

/// Y22 in miniature: hand-computed edge counts, then a test-side walk that must land on exactly the
/// planted objects.
///
/// Two independent numbers are compared here and neither is derived from the reader: the fan-out of
/// each domain (1 / 4 / 0 / 1 / 1 / 0 / 0) and the total number of planted objects (7). A `references`
/// that missed an edge would fail the per-domain count; one that invented an edge would fail the
/// visited set; one that returned everything everywhere would fail both.
#[test]
fn reference_counts_reconcile_with_a_hand_counted_fixture() {
    let root = TempRoot::new("counts");
    let store = Store::open(root.path(), cas_store::OpenMode::Create).expect("open");
    let profile = Profile::V1;
    let expected = fixture(&store, &profile);
    let reader = SnapshotReader::open(&store, expected.root, profile).expect("open reader");

    let fan_out = [
        ("root", expected.root, 1usize),
        ("inode table", expected.table, 4),
        ("directory leaf", expected.dir_leaf, 0),
        ("blob", expected.blob, 0),
        ("chunk", expected.chunk, 0),
        ("extent leaf a", expected.extent_leaf_a, 1),
        // Two extent entries, one distinct child: this row also pins the within-page collapse.
        ("extent leaf b", expected.extent_leaf_b, 1),
    ];
    let mut edges = 0;
    for (label, oid, want) in fan_out {
        let got = reader.references(oid).expect("refs").len();
        assert_eq!(got, want, "{label} fan-out");
        edges += got;
    }
    assert_eq!(edges, 7, "total edges over the fixture");

    // A test-side breadth-first walk: the same shape the GC's mark will have, written here so the
    // count is checked against a second implementation rather than against itself.
    let mut visited = std::collections::BTreeSet::new();
    let mut queue = vec![expected.root];
    while let Some(oid) = queue.pop() {
        if !visited.insert(oid) {
            continue;
        }
        queue.extend(reader.references(oid).expect("refs"));
    }
    assert_eq!(
        visited,
        set(vec![
            expected.root,
            expected.table,
            expected.dir_leaf,
            expected.blob,
            expected.chunk,
            expected.extent_leaf_a,
            expected.extent_leaf_b,
        ]),
        "the walk must reach exactly the planted objects"
    );
    assert_eq!(visited.len(), 7, "hand-counted object total");
}

/// Card §21, kind side condition 3: every role in `Role::ALL` gets a call, and the answer is a
/// defined one — `Ok` with a defined edge set, or a **typed** error. Never a panic.
///
/// The compiler already forces `references` to mention every variant; this test is what forces the
/// *answers* to be defined. Bytes and chunks are the two roles with a defined empty edge set; the page
/// roles are given bytes that are not a page, and must say so instead of guessing.
#[test]
fn every_role_answers_its_own_way_and_never_panics() {
    let root = TempRoot::new("roles");
    let store = Store::open(root.path(), cas_store::OpenMode::Create).expect("open");
    let profile = Profile::V1;
    let expected = fixture(&store, &profile);
    let reader = SnapshotReader::open(&store, expected.root, profile).expect("open reader");

    let mut answers = Vec::new();
    for role in cas_core::Role::ALL {
        // A payload that is *not* a page: the point is the answer, not a successful decode.
        let oid = store.put(role, b"not a page").expect("put").oid;
        let answer = reader.references(oid);
        match (role, &answer) {
            (cas_core::Role::Bytes | cas_core::Role::Chunk, Ok(edges)) => {
                assert!(
                    edges.is_empty(),
                    "{role:?} is a leaf of the graph, so it has no edges"
                );
            }
            (_, Err(_)) => {}
            (other, Ok(edges)) => panic!("{other:?} accepted junk bytes and returned {edges:?}"),
        }
        answers.push(role);
    }
    assert_eq!(
        answers.len(),
        cas_core::Role::ALL.len(),
        "every role was asked"
    );
}

// ------------------------------------------------- the reader trusts the source, so test the source

/// A source that answers from a table of prepared frames.
///
/// It exists to make the two failure shapes of §24 reachable: the reader does not verify identities
/// itself (it trusts `ObjectSource` for that), so "a truncated frame" and "a frame of the wrong role"
/// have to be produced by a source rather than by damaging a store.
struct Fake {
    answers: Vec<(ObjectId, Vec<u8>)>,
}

impl Fake {
    fn from_store(store: &Store, oids: &[ObjectId]) -> Self {
        let answers = oids
            .iter()
            .map(|oid| {
                (
                    *oid,
                    store.get(*oid).expect("the fixture reads its own objects"),
                )
            })
            .collect();
        Self { answers }
    }

    fn replacing(mut self, oid: ObjectId, bytes: Vec<u8>) -> Self {
        for (known, answer) in self.answers.iter_mut() {
            if *known == oid {
                *answer = bytes;
                return self;
            }
        }
        self.answers.push((oid, bytes));
        self
    }
}

impl fs_snapshot::ObjectSource for Fake {
    fn get(&self, oid: ObjectId) -> SnapshotResult<Vec<u8>> {
        self.answers
            .iter()
            .find(|(known, _)| *known == oid)
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| SnapshotError::Store(cas_store::StoreError::NotFound { oid }))
    }
}

/// §24: `references` **trusts** the source, so a source that hands back a truncated frame must still
/// produce an error — never an empty edge list.
///
/// The paired control is the same call against the same source with a sound frame, so an
/// implementation that always failed would not pass.
#[test]
fn a_truncated_frame_from_the_source_is_an_error_not_an_empty_leaf() {
    let root = TempRoot::new("fake-truncated");
    let store = Store::open(root.path(), cas_store::OpenMode::Create).expect("open");
    let profile = Profile::V1;
    let expected = fixture(&store, &profile);

    let sound = Fake::from_store(&store, &[expected.root, expected.table]);
    let reader = SnapshotReader::open_any(&sound, expected.root).expect("open reader");
    assert!(
        !reader
            .references(expected.table)
            .expect("the sound frame")
            .is_empty(),
        "control: the same call succeeds with a sound frame"
    );

    // A frame that starts like an inode leaf and then stops.
    let truncated = sound.replacing(expected.table, vec![profile.fmt, 0u8, 1, 0, 0, 0]);
    let reader = SnapshotReader::open_any(&truncated, expected.root).expect("open reader");
    let error = reader
        .references(expected.table)
        .expect_err("a truncated frame must not read as an empty leaf");
    assert!(
        matches!(error, SnapshotError::Malformed { .. }),
        "expected a typed Malformed, got {error:?}"
    );
}

/// §24: the role comes from the envelope, so a root handed back as a byte object must be refused as
/// `WrongRole` rather than decoded as if it were a root.
#[test]
fn a_frame_of_the_wrong_role_is_refused_by_the_reader() {
    let root = TempRoot::new("fake-role");
    let store = Store::open(root.path(), cas_store::OpenMode::Create).expect("open");
    let profile = Profile::V1;
    let expected = fixture(&store, &profile);

    let wrong = Fake::from_store(&store, &[expected.root]).replacing(
        expected.root,
        cas_core::Object::new(cas_core::Role::Bytes, b"not a filesystem root")
            .expect("framed")
            .to_vec(),
    );
    let error = SnapshotReader::open_any(&wrong, expected.root)
        .expect_err("a byte object is not a filesystem root");
    assert!(
        matches!(error, SnapshotError::WrongRole { .. }),
        "expected a typed WrongRole, got {error:?}"
    );

    // Control: the same source, the same oid, the untouched frame opens.
    let sound = Fake::from_store(&store, &[expected.root]);
    SnapshotReader::open_any(&sound, expected.root).expect("the sound root opens");
}

// ------------------------------------------------- §32: count the source, do not trust the shape

/// A source that counts how often each object is fetched.
///
/// §32's point: a `references` implementation with a `HashMap` cache would pass every topological fixture,
/// because the topology is identical either way. Counting the fetches is what distinguishes "the caller
/// de-duplicates" from "the library de-duplicates".
struct Counting<'a> {
    inner: &'a Store,
    counts: std::cell::RefCell<std::collections::BTreeMap<ObjectId, usize>>,
}

impl Counting<'_> {
    fn count(&self, oid: ObjectId) -> usize {
        self.counts.borrow().get(&oid).copied().unwrap_or(0)
    }
}

impl fs_snapshot::ObjectSource for Counting<'_> {
    fn get(&self, oid: ObjectId) -> SnapshotResult<Vec<u8>> {
        *self.counts.borrow_mut().entry(oid).or_insert(0) += 1;
        fs_snapshot::ObjectSource::get(self.inner, oid)
    }
}

/// C11' (§32): two calls to `references` on the same oid must fetch it **twice**.
///
/// If the answer comes from a cache, the second call fetches nothing — and no fixture shape could tell
/// that apart from a correct one.
#[test]
fn references_fetches_the_same_object_on_every_call() {
    let root = TempRoot::new("counting");
    let store = Store::open(root.path(), cas_store::OpenMode::Create).expect("open");
    let profile = Profile::V1;
    let expected = fixture(&store, &profile);
    let counting = Counting {
        inner: &store,
        counts: std::cell::RefCell::new(std::collections::BTreeMap::new()),
    };

    let reader = SnapshotReader::open_any(&counting, expected.root).expect("open reader");
    let first = reader.references(expected.table).expect("first");
    assert_eq!(
        counting.count(expected.table),
        1,
        "the first call fetches the object once"
    );
    let second = reader.references(expected.table).expect("second");
    assert_eq!(
        counting.count(expected.table),
        2,
        "the second call must fetch it again: `references` remembers nothing between calls"
    );
    assert_eq!(first, second, "and it must answer the same thing");
}
