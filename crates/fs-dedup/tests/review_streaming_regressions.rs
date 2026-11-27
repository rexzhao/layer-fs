//! Failure-path regressions for the streaming changes in c3d406f.
//! Run explicitly: cargo test -p fs-dedup --test review_streaming_regressions -- --ignored --test-threads=1 --nocapture
//! Tests touch only isolated temporary stores and require no WinFsp mount.

use fs_dedup::{Codec, DedupStore, DedupWorkspaceBackend, StoreConfig};
use fs_semantics::disposition::Disposition;
use fs_winfsp::ops::{Path as OpsPath, WorkspaceOps};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct TempDir(PathBuf);
impl TempDir {
    fn new(label: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "dedup-review-stream-{label}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn random_bytes(size: usize, mut state: u64) -> Vec<u8> {
    let mut bytes = vec![0; size];
    for chunk in bytes.chunks_exact_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chunk.copy_from_slice(&state.to_le_bytes());
    }
    bytes
}
fn physical_pack_bytes(root: &Path) -> u64 {
    std::fs::read_dir(root.join("packs"))
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum()
}

#[test]
fn skipped_dedup_spills_are_reclaimable_by_gc() {
    let temp = TempDir::new("skipped");
    let store = DedupStore::init(
        temp.0.join("store"),
        StoreConfig {
            codec: Codec::None,
            pack_target_bytes: 1024 * 1024,
            ..StoreConfig::default()
        },
    )
    .unwrap();
    store.create_workspace("ws").unwrap();
    let backend = DedupWorkspaceBackend::open(store.clone(), "ws").unwrap();
    let file = backend
        .create_or_open(&OpsPath::new(b"/large.bin"), Disposition::Create)
        .unwrap();
    let data = random_bytes(8 * 1024 * 1024, 0x123456789abcdef);
    backend.write(file.file_id, 0, &data).unwrap();
    backend.flush().unwrap();
    let checks = AtomicUsize::new(0);
    // The file becomes busy after chunking, before the check-and-swap. This is a
    // supported is_busy transition; no wall-clock race or runtime test hook is needed.
    let summary = store
        .dedup_workspace(Some("ws"), 0, |_, _| {
            checks.fetch_add(1, Ordering::SeqCst) > 0
        })
        .unwrap();
    assert_eq!(summary.skipped_busy, 1);
    assert_eq!(summary.deduped_files, 0);
    assert_eq!(backend.read(file.file_id, 0, data.len()).unwrap(), data);
    let before = physical_pack_bytes(store.root());
    let gc = store.gc(0).unwrap();
    let after = physical_pack_bytes(store.root());
    eprintln!("skipped spill bytes={before}, after GC={after}, gc={gc:?}");
    assert_eq!(
        after, 0,
        "all chunks were speculative and unreferenced, but GC cannot reclaim their packs"
    );
}

#[test]
fn failed_deferred_release_does_not_leave_persisted_refcount_drift() {
    let temp = TempDir::new("deferred-release");
    let source = temp.0.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("a.bin"), b"old shared content").unwrap();
    std::fs::write(source.join("keep.bin"), b"old shared content").unwrap();
    let root = temp.0.join("store");
    let store = DedupStore::init(&root, StoreConfig::default()).unwrap();
    store.import_dir(&source, "ws", 1, None).unwrap();
    std::fs::write(source.join("a.bin"), b"new different content and length").unwrap();

    // A deterministic SQL failure at the final decrement models loss of the
    // in-memory deferred-release plan after manifest batches have already committed.
    let conn = store.open_conn().unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_ref_decrement BEFORE UPDATE OF refcount ON chunks
         WHEN NEW.refcount < OLD.refcount
         BEGIN SELECT RAISE(ABORT, 'injected refcount decrement failure'); END;",
    )
    .unwrap();
    let result = store.import_dir_update(&source, "ws", 1, None);
    assert!(result.is_err(), "the injected decrement error must surface");
    conn.execute_batch("DROP TRIGGER reject_ref_decrement;")
        .unwrap();
    drop(conn);
    drop(store);
    let reopened = DedupStore::open(&root).unwrap();
    let audit = reopened.fsck(false).unwrap();
    eprintln!("fsck after failed update and reopen: {audit:?}");
    assert_eq!(audit.missing_chunks, 0);
    assert_eq!(
        audit.mismatched_refcounts, 0,
        "a failed update lost its deferred release plan but kept its manifest changes"
    );
}

#[test]
fn committer_failure_unblocks_import_workers() {
    const CHILD_ENV: &str = "FS_DEDUP_REVIEW_ERROR_CHILD";
    if let Some(path) = std::env::var_os(CHILD_ENV) {
        let root = PathBuf::from(path);
        let store = DedupStore::open(root.join("store")).unwrap();
        let result = store.import_dir(root.join("source"), "ws", 16, None);
        assert!(result.is_err(), "fault-injected pack writes should fail");
        return;
    }
    let temp = TempDir::new("failed-committer");
    let source = temp.0.join("source");
    std::fs::create_dir(&source).unwrap();
    let data = random_bytes(2 * 1024 * 1024, 0x9876543210abcdef);
    for i in 0..32 {
        std::fs::write(source.join(format!("{i:03}.bin")), &data).unwrap();
    }
    let store = DedupStore::init(
        temp.0.join("store"),
        StoreConfig {
            codec: Codec::None,
            ..StoreConfig::default()
        },
    )
    .unwrap();
    // Force a pack append error without needing disk exhaustion or OS-specific permissions.
    // Opening the DB succeeds; creating a pack under a regular file cannot succeed.
    std::fs::remove_dir(store.root().join("packs")).unwrap();
    std::fs::write(store.root().join("packs"), b"injected non-directory").unwrap();
    drop(store);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "committer_failure_unblocks_import_workers",
            "--nocapture",
        ])
        .env(CHILD_ENV, &temp.0)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "child failed: {status}");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("import did not return its pack error within 10s; workers remained blocked after committer exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
