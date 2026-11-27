//! Remaining pipeline cancellation boundaries in 726ae6a. No runtime hooks or live mounts.
//! cargo test -p fs-dedup --test review_cancellation_boundaries -- --ignored --test-threads=1 --nocapture

use fs_dedup::{Codec, DedupStore, StoreConfig};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct TempDir(PathBuf);
impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "dedup-cancel-review-{label}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn committer_failure_unblocks_a_full_walker_queue() {
    const CHILD_ENV: &str = "FS_DEDUP_FULL_WALKER_QUEUE_CHILD";
    if let Some(path) = std::env::var_os(CHILD_ENV) {
        let root = PathBuf::from(path);
        let store = DedupStore::open(root.join("store")).unwrap();
        let result = store.import_dir(root.join("source"), "ws", 1, None);
        assert!(
            result.is_err(),
            "injected file-publication failure must be returned"
        );
        return;
    }
    let temp = TempDir::new("full-queue");
    let source = temp.0.join("source");
    std::fs::create_dir(&source).unwrap();
    // More than both the 2048-file commit threshold and the 256-job bounded queue.
    // One worker ensures enumeration can outrun the file reader.
    for i in 0..4096 {
        std::fs::write(
            source.join(format!("{i:05}.bin")),
            b"unchanged small payload",
        )
        .unwrap();
    }
    let store = DedupStore::init(temp.0.join("store"), StoreConfig::default()).unwrap();
    let conn = store.open_conn().unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_file_publish BEFORE INSERT ON inodes
         WHEN NEW.kind = 1 BEGIN SELECT RAISE(ABORT, 'injected file publication failure'); END;",
    )
    .unwrap();
    drop(conn);
    drop(store);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "committer_failure_unblocks_a_full_walker_queue",
            "--include-ignored",
            "--nocapture",
        ])
        .env(CHILD_ENV, &temp.0)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "child failed: {status}");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("committer failed but walker did not leave its full job queue within 15s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn walker_failure_stops_all_writes_before_returning() {
    let temp = TempDir::new("walker-error");
    let source = temp.0.join("source");
    std::fs::create_dir(&source).unwrap();
    // Enumeration reaches a directory-batch failure while this file is still being chunked.
    let mut bytes = vec![0u8; 64 * 1024 * 1024];
    let mut state = 0x1234_5678_9abc_def0u64;
    for chunk in bytes.chunks_exact_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chunk.copy_from_slice(&state.to_le_bytes());
    }
    std::fs::write(source.join("000-file.bin"), &bytes).unwrap();
    drop(bytes);
    std::fs::create_dir(source.join("dir-to-publish")).unwrap();
    let store = DedupStore::init(
        temp.0.join("store"),
        StoreConfig {
            codec: Codec::None,
            ..StoreConfig::default()
        },
    )
    .unwrap();
    store.create_workspace("ws").unwrap();
    let conn = store.open_conn().unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_directory_publish BEFORE INSERT ON inodes
         WHEN NEW.kind = 2 AND NEW.inode_id <> 1
         BEGIN SELECT RAISE(ABORT, 'injected directory publication failure'); END;",
    )
    .unwrap();
    // Update mode flushes pending directories before descending into the next directory,
    // so the walker fails while the already-dispatched large file is still being read.
    let result = store.import_dir_update(&source, "ws", 1, None);
    assert!(result.is_err());
    let files_at_return: i64 = conn
        .query_row("SELECT COUNT(*) FROM inodes WHERE kind = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        files_at_return, 0,
        "fixture requires an unfinished file producer at failure"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut files_later = files_at_return;
    while Instant::now() < deadline {
        files_later = conn
            .query_row("SELECT COUNT(*) FROM inodes WHERE kind = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        if files_later != files_at_return {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(files_later, files_at_return, "a detached committer published inode rows after import returned Err and released pack.lock");
}
