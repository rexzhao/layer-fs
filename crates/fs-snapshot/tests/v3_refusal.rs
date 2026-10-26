//! G3's **refusal** side (P3-2 card §3.2): a v3 build must reject older snapshots as `UnsupportedProfile`, must not
//! modify the store while doing so, and must reject the same snapshot again the second time.
//!
//! The store here is the **real, committed v2 fixture**, copied into a temp directory so the rejection can be repeated
//! and so the byte-hash comparison has something to compare. Using a synthetic root would make the whole file a test of
//! the reader's own constants; the fixture is an external fact (its README pins every byte).
//!
//! What this file deliberately does **not** claim: that a *v2 binary* rejects a v3 fixture. That is a different
//! direction, it needs a build that no longer exists in this tree, and it is recorded as a manual pre-release item
//! rather than pretended here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fs_snapshot::{Profile, SnapshotError};

fn fixture_store() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("snapshot-v2-store")
        .join("store")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create dir");
    for entry in std::fs::read_dir(from).expect("read dir").flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

/// `(relative path, BLAKE3)` for every file: the card's "bytes did not change" needs a hash, not a length.
fn hashes(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).expect("read dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace(char::from(92), "/");
            // `-shm` is shared memory between connections, not durable content (ADR-008 §6.1).
            if relative.ends_with("-shm") {
                continue;
            }
            let bytes = std::fs::read(&path).expect("read file");
            out.insert(relative, blake3::hash(&bytes).to_hex().to_string());
        }
    }
    out
}

fn temp_copy(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fs-snapshot-v3-refusal-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    copy_dir(&fixture_store(), &dir);
    dir
}

/// The v2 root id from the fixture's README (verified against the fixture by `v2_fixture.rs`).
const V2_ROOT: [u8; 32] = [
    0xee, 0xd6, 0x36, 0xc6, 0x22, 0x5f, 0x5a, 0x68, 0x54, 0x57, 0xf8, 0xee, 0xc2, 0xd9, 0xa6, 0x42,
    0xd5, 0xd6, 0x0f, 0xbd, 0x9f, 0x8b, 0xde, 0x86, 0x79, 0x02, 0xc7, 0xd6, 0x98, 0x90, 0x19, 0x2f,
];

/// A v3 build refuses the v2 fixture with `UnsupportedProfile`, **twice**, and does not touch a byte in between.
#[test]
fn the_v2_fixture_is_refused_twice_without_changing_a_byte() {
    let dir = temp_copy("fixture");
    let before = hashes(&dir);
    let store = cas_store::Store::open(&dir, cas_store::OpenMode::ReadOnly).expect("open store");
    let root = cas_core::ObjectId::from_bytes(V2_ROOT);

    for attempt in 1..=2 {
        let error = fs_snapshot::SnapshotReader::open(&store, root, Profile::V3)
            .err()
            .unwrap_or_else(|| panic!("attempt {attempt}: a v3 build must refuse the v2 fixture"));
        assert!(
            matches!(error, SnapshotError::UnsupportedProfile { .. }),
            "attempt {attempt}: the refusal names the profile (a version gap), not a parse failure: {error:?}"
        );
        assert_eq!(
            hashes(&dir),
            before,
            "attempt {attempt}: a refusal must not modify the store"
        );
    }
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **Row 0 of this file**: the same copy, the same code path, opened with the profile the fixture was written under, must
/// read. Without it the two refusals above would pass on a copy that was broken by the copy.
#[test]
fn the_same_copy_reads_under_its_own_profile() {
    let dir = temp_copy("control");
    let store = cas_store::Store::open(&dir, cas_store::OpenMode::ReadOnly).expect("open store");
    let root = cas_core::ObjectId::from_bytes(V2_ROOT);
    let reader = fs_snapshot::SnapshotReader::open(&store, root, Profile::V2)
        .expect("cell control: the v2 fixture reads under v2");
    // The control's job is "the copy still reads": a record from the fixture proves the reader got past the root and the
    // inode table, which a root-only check would not.
    let record = reader
        .inode(1)
        .expect("cell control: the inode table decodes")
        .is_some();
    assert!(
        record,
        "cell control: the fixture's first inode is really there"
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A **synthetic v1** root is refused as a profile gap too.
///
/// The card is explicit about what this can and cannot say: this build cannot produce a real v1 snapshot (P3-0c proved
/// `Profile::CURRENT` is v2 and there is no v1 writer), so the synthetic root tests **field recognition only** — that the
/// profile id is what decides — and says nothing about whether a historical v1 store would read.
#[test]
fn a_synthetic_v1_root_is_refused_by_profile() {
    use cas_core::{ObjectId, Role};
    use fs_snapshot::{encode_root, PageSink, RootValue, SnapshotResult};

    #[derive(Default)]
    struct Memory(BTreeMap<ObjectId, (Role, Vec<u8>)>);
    impl PageSink for Memory {
        fn put(&mut self, role: Role, value: &[u8]) -> SnapshotResult<ObjectId> {
            let oid = ObjectId::from_bytes(blake3::hash(value).into());
            self.0.insert(oid, (role, value.to_vec()));
            Ok(oid)
        }
    }
    impl fs_snapshot::ObjectSource for Memory {
        fn get(&self, oid: ObjectId) -> SnapshotResult<Vec<u8>> {
            self.0
                .get(&oid)
                .map(|(role, value)| {
                    cas_core::Object::new(*role, value)
                        .expect("framed")
                        .to_vec()
                })
                .ok_or_else(|| SnapshotError::Store(cas_store::StoreError::NotFound { oid }))
        }
    }

    let mut memory = Memory::default();
    let root = encode_root(
        &Profile::V1,
        &RootValue {
            scope_id: [1u8; 32],
            inode_table: ObjectId::from_bytes([2u8; 32]),
        },
        &mut memory,
    )
    .expect("encode a v1-shaped root");
    // `SnapshotReader` is not `Debug`, so `.expect_err` is unavailable; the match keeps the panic message explicit.
    let error = match fs_snapshot::SnapshotReader::open(&memory, root, Profile::V3) {
        Ok(_) => panic!("cell v1: a v1 root must be refused"),
        Err(error) => error,
    };
    assert!(
        matches!(error, SnapshotError::UnsupportedProfile { .. }),
        "cell v1: UnsupportedProfile — the profile id, not the bytes, is what makes it old: {error:?}"
    );
}
