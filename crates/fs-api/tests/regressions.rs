//! Regressions found by the reference-model sequence (ADR-007 §10).
//!
//! Each test is the smallest case that fails before its fix. They live here rather than in
//! `fs-overlay/tests/contract.rs` because the trigger is the interaction between a
//! published snapshot and the overlay's whiteout bookkeeping, which a hand-built base does
//! not reproduce: the overlay fixture attaches to a snapshot it did not create, while the
//! bug needs a *workspace's own* snapshot as its base.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use fs_api::Repo;

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
                "fs-api-regress-{label}-{}-{sequence}",
                std::process::id()
            ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Removing a name that the **snapshot** knows, re-creating it and removing it again must
/// leave it gone.
///
/// Before the fix, the second removal returned `Ok` and the entry reappeared: the overlay
/// stored the first removal as a whiteout row, the re-creation reused that row with a
/// child, and the second removal deleted the row outright — revealing the base binding
/// again. Found by the reference-model sequence at `seed=1 step=126`.
#[test]
fn removing_a_snapshotted_name_twice_keeps_it_gone() {
    let root = TempRoot::new("whiteout");
    let repo = Repo::create(root.path.join("store")).expect("create");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");

    workspace.mkdir("/d").expect("mkdir");
    workspace.snapshot().expect("snapshot");

    workspace.remove("/d").expect("first removal");
    assert!(
        workspace.stat("/d").is_err(),
        "the first removal must hide the name"
    );

    workspace.mkdir("/d").expect("re-create");
    assert!(
        workspace.stat("/d").is_ok(),
        "the re-created name must be visible"
    );

    workspace.remove("/d").expect("second removal");
    assert!(
        workspace.stat("/d").is_err(),
        "the second removal must not reveal the snapshot binding again"
    );
    let names: Vec<String> = workspace
        .read_dir("/")
        .expect("read_dir")
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert!(
        !names.contains(&"d".to_owned()),
        "the directory listing must not show the removed name: {names:?}"
    );

    workspace.close().expect("close");
}
