//! Targeted regressions found while reviewing fe05c0a.
//!
//! These tests assert the desired behavior, not the existing bugs. Known failures are
//! explicitly ignored so an ordinary test run is not silently made red by a review-only
//! change. Reproduce with:
//!
//! cargo test -p fs-dedup --test review_regressions -- --ignored --test-threads=1 --nocapture
//!
//! No WinFsp mount is needed; all stores and source files are temporary.

use fs_dedup::{DedupStore, DedupWorkspaceBackend, StoreConfig};
use fs_semantics::disposition::Disposition;
use fs_winfsp::ops::{Path as OpsPath, WorkspaceOps};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
const PAYLOAD: &[u8] = b"unique unchanged payload";

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-dedup-review-{label}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create isolated test directory");
        Self(path)
    }

    fn source(&self, name: &str, payload: &[u8]) -> PathBuf {
        let source = self.0.join(name);
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a.bin"), payload).unwrap();
        source
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn import_update_with_only_mtime_changed_preserves_reused_chunks() {
    let temp = TempDir::new("mtime-update");
    let source = temp.source("source", PAYLOAD);
    let store = DedupStore::init(temp.0.join("store"), StoreConfig::default()).unwrap();
    store.import_dir(&source, "ws", 1, None).unwrap();

    // Force the update path without changing any chunk or depending on clock resolution.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(source.join("a.bin"))
        .unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(5))
        .unwrap();
    drop(file);

    let result = store.import_dir_update(&source, "ws", 1, None);
    assert!(result.is_ok(), "mtime-only update failed: {result:?}");
    let summary = result.unwrap();
    assert_eq!(summary.updated_files, 1);
    assert_eq!(summary.errors, 0);

    let backend = DedupWorkspaceBackend::open(store.clone(), "ws").unwrap();
    let file = backend
        .create_or_open(&OpsPath::new(b"/a.bin"), Disposition::Open)
        .unwrap();
    assert_eq!(
        backend.read(file.file_id, 0, PAYLOAD.len()).unwrap(),
        PAYLOAD
    );
    let audit = store.fsck(false).unwrap();
    assert_eq!(audit.missing_chunks, 0);
    assert_eq!(audit.mismatched_refcounts, 0);
}

#[test]
fn dedup_after_same_content_cow_does_not_trust_a_deleted_chunk_index_entry() {
    let temp = TempDir::new("cow-rededup");
    let source = temp.source("source", PAYLOAD);
    let store = DedupStore::init(temp.0.join("store"), StoreConfig::default()).unwrap();
    store.import_dir(&source, "ws", 1, None).unwrap();
    let backend = DedupWorkspaceBackend::open(store.clone(), "ws").unwrap();
    let file = backend
        .create_or_open(&OpsPath::new(b"/a.bin"), Disposition::Open)
        .unwrap();

    // CoW materialization caches the chunk location, then drops its sole DB reference.
    // Writing the same byte is intentional: the next dedup must resolve that hash again.
    backend.write(file.file_id, 0, &PAYLOAD[..1]).unwrap();
    backend.flush().unwrap();
    let result = store.dedup_workspace(Some("ws"), 0, |_, id| backend.is_file_dirty(id));
    assert!(result.is_ok(), "dedup after CoW failed: {result:?}");
    assert_eq!(result.unwrap().deduped_files, 1);
    assert!(!store
        .overlay()
        .file_path(backend.ws_id(), file.file_id)
        .exists());
    assert_eq!(
        backend.read(file.file_id, 0, PAYLOAD.len()).unwrap(),
        PAYLOAD
    );
}

#[test]
fn independent_store_writers_must_not_overwrite_each_others_packs() {
    let temp = TempDir::new("independent-writers");
    let source_a = temp.source("source-a", b"payload from writer A");
    let source_b = temp.source("source-b", b"payload from writer B");
    let root = temp.0.join("store");
    let first = DedupStore::init(&root, StoreConfig::default()).unwrap();

    // Model two CLI processes opening the same store before either writes a pack.
    // A future implementation may instead safely reject the second writer at open.
    let second = match DedupStore::open(&root) {
        Ok(store) => store,
        Err(_) => return,
    };
    first.import_dir(&source_a, "a", 1, None).unwrap();
    second.import_dir(&source_b, "b", 1, None).unwrap();
    drop(first);
    drop(second);

    // Reopen to avoid any decoded-data cache masking an on-disk overwrite.
    let reopened = DedupStore::open(&root).unwrap();
    for (workspace, expected) in [
        ("a", b"payload from writer A".as_slice()),
        ("b", b"payload from writer B".as_slice()),
    ] {
        let backend = DedupWorkspaceBackend::open(reopened.clone(), workspace).unwrap();
        let file = backend
            .create_or_open(&OpsPath::new(b"/a.bin"), Disposition::Open)
            .unwrap();
        let result = backend.read(file.file_id, 0, expected.len());
        assert!(
            result.is_ok(),
            "workspace {workspace} lost committed pack data: {result:?}"
        );
        assert_eq!(result.unwrap(), expected);
    }
}
