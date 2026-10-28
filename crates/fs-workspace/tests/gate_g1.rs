//! **G1** — a write that arrives while a `commit` is between `materialise` and `adopt` **waits**, and then survives.
//!
//! This is the window the publication gate exists for: `commit` reads the view, then adopts a new base. A write landing between the two would be in neither
//! tree — the tree was read before it, and the overlay row is replaced when the base moves. The test does not hope to hit that window; it **parks** the commit
//! in it with `pause_point`, so the write is guaranteed to arrive there.
//!
//! One test file, one process: `LAYERFS_PAUSE_AT` is a process-level variable, and a second pause in the same process would confuse which commit is parked.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fs_workspace::Repo;

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("fs-workspace-g1-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&path).expect("temp");
    path
}

fn marker(name: &str, suffix: &str) -> PathBuf {
    std::env::temp_dir().join(format!("layerfs-pause-{name}.{suffix}"))
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
fn a_write_during_a_commit_waits_and_survives() {
    let directory = temp_dir("g1");
    let repo = Arc::new(Repo::open(directory.join("store"), true).expect("repo"));
    repo.init().expect("init");
    let workspace = Arc::new(repo.workspace_create("w1", "main").expect("workspace"));
    workspace.create_file("/before").expect("create");
    workspace.write_at("/before", 0, b"x").expect("write");
    workspace.flush().expect("flush");

    // The pause is armed **for this process**, and released by a file rather than a sleep: "the commit is standing still" becomes a fact instead of a guess.
    // SAFETY: `set_var` is safe in edition 2021; the variable is read by the commit thread below.
    std::env::set_var("LAYERFS_PAUSE_AT", "after-materialise");
    let release = marker("after-materialise", "go");
    let here = marker("after-materialise", "here");
    let _ = std::fs::remove_file(&release);
    let _ = std::fs::remove_file(&here);

    let commit_done = Arc::new(AtomicBool::new(false));
    let committed = {
        let workspace = Arc::clone(&workspace);
        let commit_done = Arc::clone(&commit_done);
        std::thread::spawn(move || {
            let outcome = workspace.commit("during-pause", false);
            commit_done.store(true, Ordering::SeqCst);
            outcome
        })
    };

    // Wait until the commit is **inside** the window, then write from another thread.
    wait_for(&here, "the commit to reach the pause point");
    let wrote = Arc::new(AtomicBool::new(false));
    let writer = {
        let workspace = Arc::clone(&workspace);
        let wrote = Arc::clone(&wrote);
        let commit_done = Arc::clone(&commit_done);
        std::thread::spawn(move || {
            workspace.create_file("/late").expect("create /late");
            workspace
                .write_at("/late", 0, b"late")
                .expect("write /late");
            // Record what the writer saw at the moment it returned: if the commit had already finished, the gate did not order anything.
            wrote.store(true, Ordering::SeqCst);
            commit_done.load(Ordering::SeqCst)
        })
    };

    // The writer must be **blocked**: give it long enough that an unblocked write would certainly have finished, then require that it has not.
    std::thread::sleep(Duration::from_millis(300));
    let finished_while_paused = wrote.load(Ordering::SeqCst);
    assert!(
        !finished_while_paused,
        "cell G1: a write must not complete while the commit is between materialise and adopt"
    );

    // Release the commit and let both threads finish.
    std::fs::write(&release, b"go").expect("release the pause");
    let outcome = committed.join().expect("commit thread").expect("commit ok");
    assert!(outcome.commit.is_some(), "cell G1: the commit published");
    let commit_was_done = writer.join().expect("writer thread");
    assert!(
        commit_was_done,
        "cell G1: the write completed only after the commit returned"
    );

    // **And it survived**: the entry is in the view, which means it landed in the adopted base rather than being dropped with the old overlay rows.
    // **And it survived**: it can be read back, which means it landed in the adopted base rather than being dropped with the old overlay rows.
    let late = workspace
        .read_at("/late", 0, 4)
        .expect("cell G1: the late write must survive the commit");
    assert_eq!(late, b"late");

    // SAFETY: as above; the test is finishing.
    std::env::remove_var("LAYERFS_PAUSE_AT");
    let _ = std::fs::remove_file(&release);
    let _ = std::fs::remove_dir_all(&directory);
}
