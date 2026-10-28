//! Offline maintenance must take the whole store or change nothing (ADR-008 §6.1, P2b-6 §2.4).
//!
//! "Change nothing" is proven from **outside** the implementation: a fingerprint of every file
//! under the store (relative path + length) is taken before the refused attempt and compared after.
//! An implementation that opened the database, set `journal_mode`, or half-cleaned `tmp/` before
//! discovering it was not alone would fail here — which is exactly the failure mode a "we promise
//! not to write" comment cannot catch.

mod common;

use std::path::{Path, PathBuf};

use fs_workspace::Repo;

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "fs-workspace-exclusive-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
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

/// Every file under `root` as `(relative path, length)`, sorted — a fingerprint taken outside the
/// process, so it cannot agree with a lie inside it.
fn fingerprint(root: &Path) -> Vec<(String, u64)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, u64)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, out);
            } else if let Ok(meta) = entry.metadata() {
                let relative = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                // `-shm` is shared memory between connections: no durable content, and any
                // opener may create or drop it. ADR-008 §6.1 fixes this comparison accordingly.
                if !relative.ends_with("-shm") {
                    out.push((relative, meta.len()));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

#[test]
fn a_refused_exclusive_open_leaves_every_byte_alone() {
    common::strict_gate();
    let root = TempRoot::new("refused");
    let dir = root.store();
    let repo = std::sync::Arc::new(Repo::open(&dir, true).expect("open"));
    repo.init().expect("init");
    let workspace = repo.workspace_create("w1", "main").expect("workspace");
    workspace.create_file("/f").expect("create");
    workspace.write_at("/f", 0, b"payload").expect("write");
    workspace.close().expect("close");
    drop(repo);

    // Another process (here: another handle) holds the store.
    let held = std::sync::Arc::new(Repo::open(&dir, false).expect("open"));
    let before = fingerprint(&dir);

    match fs_workspace::MaintenanceRepo::open(&dir) {
        Err(fs_workspace::WorkspaceError::StoreInUse { what }) => {
            assert!(
                what.contains("main database") || what.contains("overlay"),
                "the refusal names what was busy: {what}"
            );
        }
        other => panic!("a store in use must be refused, got {other:?}"),
    }

    let after = fingerprint(&dir);
    assert_eq!(
        before, after,
        "a refused maintenance open must not touch the store"
    );
    // The store is still usable by its legitimate user.
    assert!(held.workspace_open("w1").is_ok());
}

#[test]
fn an_unused_store_can_be_taken_exclusively() {
    common::strict_gate();
    let root = TempRoot::new("taken");
    let dir = root.store();
    let repo = std::sync::Arc::new(Repo::open(&dir, true).expect("open"));
    repo.init().expect("init");
    drop(repo);

    let maintenance = fs_workspace::MaintenanceRepo::open(&dir).expect("exclusive open");
    let head: String = maintenance
        .connection()
        .query_row(
            "SELECT head_commit FROM branch WHERE name = 'main'",
            [],
            |row| row.get(0),
        )
        .expect("head");
    assert_eq!(head.len(), 64);
    drop(maintenance);

    // Released: an ordinary open works again.
    Repo::open(&dir, false).expect("ordinary open after maintenance");
}
