//! **G6 / G7** — the two **measurements** the publication gate owes: how long a reader can be starved, and how long a write can be held.
//!
//! Both are reported as numbers, not asserted as feelings: the card gives the lines (5 s for starvation, `IrpTimeout / 10` = 30 s for a blocked write) and these
//! criteria print what was measured next to the line they are compared with. A measurement that only ever says "ok" cannot tell anyone when the machine got
//! ten times slower.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fs_workspace::Repo;

fn fixture(label: &str) -> (std::path::PathBuf, Arc<Workspace>) {
    let directory =
        std::env::temp_dir().join(format!("fs-workspace-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("temp");
    let repo = Arc::new(Repo::open(directory.join("store"), true).expect("repo"));
    repo.init().expect("init");
    let workspace = Arc::new(repo.workspace_create("w1", "main").expect("workspace"));
    workspace.create_file("/f").expect("create");
    workspace.flush().expect("flush");
    (directory, workspace)
}

use fs_workspace::Workspace;

/// **G6**: a reader is not starved by a stream of commits — the longest wait stays under the 5 s line.
///
/// The reading is the **maximum** wait, because starvation is about the worst case: an average would let one 4.9 s stall hide behind a hundred fast reads.
#[test]
fn a_reader_is_not_starved_by_commits() {
    common::strict_gate();
    let (directory, workspace) = fixture("g6");
    let stop = Arc::new(AtomicBool::new(false));
    let worst = Arc::new(AtomicU64::new(0));

    let reader = {
        let workspace = Arc::clone(&workspace);
        let stop = Arc::clone(&stop);
        let worst = Arc::clone(&worst);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let started = Instant::now();
                let _ = workspace.read_at("/f", 0, 1);
                let waited = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                worst.fetch_max(waited, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };

    for round in 0..20 {
        let _ = workspace.write_at("/f", 0, format!("{round}").as_bytes());
        let _ = workspace.commit(&format!("round {round}"), false);
    }
    stop.store(true, Ordering::SeqCst);
    reader.join().expect("reader thread");

    let worst = worst.load(Ordering::SeqCst);
    println!("G6 READING: longest read wait during 20 commits = {worst} ms (line: 5000 ms)");
    assert!(
        worst < 5_000,
        "cell G6: a reader must not wait 5 s behind commits, measured {worst} ms"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

/// **G7**: a write held by a paused commit is released well inside the platform's own bound (`IrpTimeout / 10` = 30 s).
///
/// This is G1's number, measured instead of assumed — the card's line is the point where the FSD's timeout (not our lock) would become the failure mode.
#[test]
fn a_write_blocked_by_a_commit_is_released_within_the_bound() {
    common::strict_gate();
    let (directory, workspace) = fixture("g7");

    // SAFETY: `set_var` is safe in edition 2021; the commit thread reads it.
    std::env::set_var("LAYERFS_PAUSE_AT", "after-materialise");
    std::env::set_var("LAYERFS_PAUSE_TAG", "-g7");
    let release = std::env::temp_dir().join("layerfs-pause-after-materialise-g7.go");
    let here = std::env::temp_dir().join("layerfs-pause-after-materialise-g7.here");
    let _ = std::fs::remove_file(&release);
    let _ = std::fs::remove_file(&here);

    let committed = {
        let workspace = Arc::clone(&workspace);
        std::thread::spawn(move || workspace.commit("g7", false))
    };
    let started = Instant::now();
    while !here.exists() && started.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        here.exists(),
        "cell G7: the commit must reach the pause point"
    );

    let writer_started = Instant::now();
    let writer = {
        let workspace = Arc::clone(&workspace);
        std::thread::spawn(move || workspace.write_at("/f", 0, b"late"))
    };
    std::thread::sleep(Duration::from_millis(200));
    std::fs::write(&release, b"go").expect("release the pause");

    let _ = committed.join().expect("commit thread");
    writer.join().expect("writer thread").expect("write ok");
    let waited = u64::try_from(writer_started.elapsed().as_millis()).unwrap_or(u64::MAX);

    println!(
        "G7 READING: blocked write released after {waited} ms (bound: 30000 ms = IrpTimeout/10)"
    );
    assert!(
        waited < 30_000,
        "cell G7: a blocked write must be released inside IrpTimeout/10, measured {waited} ms"
    );

    // SAFETY: as above; the test is finishing.
    std::env::remove_var("LAYERFS_PAUSE_AT");
    std::env::remove_var("LAYERFS_PAUSE_TAG");
    let _ = std::fs::remove_file(&release);
    let _ = std::fs::remove_dir_all(&directory);
}
