//! The canonical content shapes under profile v2 (ADR-007 §3.2, §11-2).
//!
//! The rule is a closed table — one legal shape per condition — and the decoder rejects
//! everything else. These tests pin the table from the outside: publish a file of each kind
//! through the workspace, then read it back with the snapshot reader and inspect the content
//! the root actually recorded.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use cas_store::{OpenMode, Store};
use fs_api::Repo;
use fs_snapshot::{Content, SnapshotReader};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::var_os("WCAS_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "fs-api-shapes-{label}-{}-{sequence}",
                std::process::id()
            ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }

    fn store(&self) -> PathBuf {
        self.path.join("store")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Publishes one file with `bytes` and returns the content the root records for it.
fn publish(label: &str, bytes: &[u8]) -> Content {
    let root = TempRoot::new(label);
    let repo = Repo::create(root.store()).expect("create");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");
    workspace.create("/f").expect("create file");
    workspace.write_at("/f", 0, bytes).expect("write");
    workspace
        .truncate("/f", u64::try_from(bytes.len()).expect("fits"))
        .expect("truncate");
    let commit = workspace.commit("shape", false).expect("commit");
    workspace.close().expect("close");

    let info = repo.commit_info(&commit).expect("info");
    let store = Store::open(repo.store_dir(), OpenMode::ReadOnly).expect("store");
    let reader =
        SnapshotReader::open_any(&store, info.root.parse().expect("root")).expect("reader");
    let root_inode = reader.inode(0).expect("root inode").expect("present");
    let serial = reader
        .lookup(&root_inode, b"f")
        .expect("lookup")
        .expect("the file");
    reader
        .inode(serial)
        .expect("inode")
        .expect("present")
        .content
}

/// `size == 0` → `Empty` (the first row of ADR-007 §3.2).
#[test]
fn an_empty_file_is_empty() {
    assert_eq!(publish("empty", b""), Content::Empty);
}

/// One chunk, not all zero → `Blob`, whatever the size.
///
/// This is the row that a fixed-block encoder could not express: a 100-byte file and a
/// 7 KiB file are both a single chunk, so both are `Blob`.
#[test]
fn a_single_chunk_is_a_blob() {
    for len in [1usize, 100, 4_096, 7_000] {
        let mut bytes = vec![0u8; len];
        bytes[0] = 0x5A;
        assert!(
            matches!(publish("single", &bytes), Content::Blob(_)),
            "a {len}-byte file is one chunk and must be a Blob"
        );
    }
}

/// Every chunk zero → `Empty`, even though the file is well past one chunk.
#[test]
fn an_all_zero_file_is_empty() {
    assert_eq!(
        publish("zeros", &vec![0u8; 100_000]),
        Content::Empty,
        "a zero file has no content to store, whatever its size"
    );
}

/// Two or more chunks, some non-zero → `Extents`.
#[test]
fn a_large_file_is_extents() {
    // Pseudo-random bytes so CDC produces many chunks.
    let mut state = 0x1234_5678u64;
    let mut bytes = Vec::with_capacity(200_000);
    while bytes.len() < 200_000 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        bytes.extend_from_slice(&(z ^ (z >> 31)).to_be_bytes());
    }
    bytes.truncate(200_000);
    assert!(
        matches!(publish("large", &bytes), Content::Extents(_)),
        "200 KiB of pseudo-random bytes must be Extents"
    );
}

/// A file whose bytes round-trip exactly, whatever shape it took.
#[test]
fn every_shape_reads_back_byte_for_byte() {
    let cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0u8; 1],
        vec![0xAB; 5_000],
        vec![0u8; 100_000],
        (0..70_000u32)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect(),
    ];
    for (index, bytes) in cases.iter().enumerate() {
        let root = TempRoot::new(&format!("roundtrip-{index}"));
        let repo = Repo::create(root.store()).expect("create");
        let mut workspace = repo.workspace_create("w1", "main").expect("create");
        workspace.create("/f").expect("create file");
        workspace.write_at("/f", 0, bytes).expect("write");
        workspace
            .truncate("/f", u64::try_from(bytes.len()).expect("fits"))
            .expect("truncate");
        let commit = workspace.commit("x", false).expect("commit");
        workspace.close().expect("close");

        let view = repo.read_view(&commit).expect("read_view");
        assert_eq!(
            &view.read_all("/f").expect("read"),
            bytes,
            "case {index} ({} bytes) must round-trip",
            bytes.len()
        );
    }
}
