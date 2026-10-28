//! **G3** — a write that began **before** the commit is **included** in it.
//!
//! G1 asked what happens to a write that arrives *during* the commit; this asks the other side of the same lock: a write already inside the gate must finish
//! before the commit's `materialise` reads the view, or the commit would publish a tree that never contained it — a lost write with the opposite sign.
//!
//! The write is parked **inside the gate** (`pause_point("inside-write")` runs after the read lock is taken), so the ordering is a fact the test arranges, not
//! a race it hopes for.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fs_workspace::Repo;

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("fs-workspace-g3-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&path).expect("temp");
    path
}

fn marker(name: &str, suffix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "layerfs-pause-{name}{}.{suffix}",
        std::env::var("LAYERFS_PAUSE_TAG").unwrap_or_default()
    ))
}

fn wait_for(path: &std::path::Path, what: &str) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(20) {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}: {}", path.display());
}

#[test]
fn a_write_that_started_before_the_commit_is_included() {
    let directory = temp_dir("g3");
    let repo = Arc::new(Repo::open(directory.join("store"), true).expect("repo"));
    repo.init().expect("init");
    let workspace = Arc::new(repo.workspace_create("w1", "main").expect("workspace"));
    workspace.create_file("/first").expect("create");
    workspace
        .write_at("/first", 0, b"old")
        .expect("seed content");
    workspace.flush().expect("flush");

    // SAFETY: `set_var` is safe in edition 2021; the writer thread below reads it.
    std::env::set_var("LAYERFS_PAUSE_AT", "inside-write");
    std::env::set_var("LAYERFS_PAUSE_TAG", "-g3");
    let release = marker("inside-write", "go");
    let here = marker("inside-write", "here");
    let _ = std::fs::remove_file(&release);
    let _ = std::fs::remove_file(&here);

    // The write starts first and stops **while holding the read gate**.
    let writer = {
        let workspace = Arc::clone(&workspace);
        std::thread::spawn(move || workspace.write_at("/first", 0, b"early"))
    };
    wait_for(&here, "the write to reach the pause point");

    // Now the commit: it must wait for the gate, which the write holds.
    let committed = {
        let workspace = Arc::clone(&workspace);
        std::thread::spawn(move || workspace.commit("after-the-write", false))
    };
    std::thread::sleep(Duration::from_millis(300));

    // Release the write; both then finish.
    std::fs::write(&release, b"go").expect("release the pause");
    writer.join().expect("writer thread").expect("write ok");
    let outcome = committed.join().expect("commit thread").expect("commit ok");
    assert!(outcome.commit.is_some(), "cell G3: the commit published");

    // **Included**: what the write wrote is in the commit — read it back **through history**, not through the live view, because the view would show it either
    // way (it is in the overlay) and that would make the criterion pass for the wrong reason.
    let commit = outcome.commit.expect("a commit id");
    // **Read it back from the commit itself**: `restore` drops the overlay's pending rows and puts the view on that commit's tree, so what is read afterwards
    // can only come from the published tree. Reading through the live view instead would show the overlay row either way — green for the wrong reason.
    let workspace = Arc::try_unwrap(workspace).unwrap_or_else(|_| panic!("the threads are joined"));
    workspace
        .restore(&commit, true)
        .expect("restore onto the commit");
    let bytes = workspace.read_at("/first", 0, 5).expect("read /first");
    assert_eq!(
        bytes, b"early",
        "cell G3: the commit must contain the write that began before it"
    );

    // SAFETY: as above; the test is finishing.
    std::env::remove_var("LAYERFS_PAUSE_AT");
    std::env::remove_var("LAYERFS_PAUSE_TAG");
    let _ = std::fs::remove_file(&release);
    let _ = std::fs::remove_dir_all(&directory);
}
