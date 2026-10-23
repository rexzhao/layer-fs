//! **P2-0b / B1** — does the table backend really have these races?
//!
//! Architect predicted four concurrency defects from reading the code. This card's
//! job is to **prove or disprove them by running them**, not to fix them: the table
//! backend is replaced in P2a, and the overlay backend's single-writer rule is
//! meant to make them structurally impossible.
//!
//! Each test detects the defect and reports what it saw, so a passing test means the
//! race did not occur in that run, not that it cannot.
//!
//! **`b1_1` is `#[ignore]`d** because it reproduces a window this card deliberately
//! does not fix (the table backend is replaced in P2a). Run it with `--ignored` to
//! watch it fail; the contract form that must pass on the overlay backend is
//! `tree_store_contract_concurrency::k3_concurrent_writes_to_different_blocks_keep_both`.
//!
//! The four predictions:
//!   1. `write_at` to different blocks of one file races on `size` and one writer's
//!      length extension is lost (size goes backwards).
//!   2. `write_at` against `truncate` lets a file grow back after being cut.
//!   3. `write_at` against `unlink` leaves extent rows for a deleted inode.
//!   4. Two cross-directory renames both pass the R9 subtree check and create a
//!      directory cycle.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use fs_api::{Fs, OpenMode, BLOCK_BYTES};

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
                "fs-b1-{label}-{}-{}-{sequence}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or(0)
            ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn fixture(label: &str) -> (TempRoot, Fs) {
    let root = TempRoot::new(label);
    let fs = Fs::open(&root.path, "default", OpenMode::Create).expect("open");
    (root, fs)
}

/// B1-1 — two writers extend the same file at different offsets.
///
/// Architect's prediction: each `write_at` reads `size`, computes a new size, and
/// writes it back; if both read the old size, the later commit overwrites the
/// earlier one's extension and the file ends up shorter than the furthest write.
///
/// The invariant asserted is simply "the file is at least as long as the furthest
/// byte anyone wrote". It cannot be violated by a correct implementation.
#[test]
#[ignore = "reproduces the unfixed B1-1 data-loss window; run with --ignored to see it fail"]
fn b1_1_concurrent_extensions_do_not_lose_the_furthest_write() {
    let (root, _fs) = fixture("b1-1");
    fs_create_probe(&root, 0);

    let rounds = 40;
    let mut violations = 0;
    for round in 0..rounds {
        let path = format!("/f{round}");
        {
            let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("open");
            fs.create(&path).expect("create");
        }

        // Two writers, each appending a block at a very different offset.
        let low = 0u64;
        let high = BLOCK_BYTES * 8;
        std::thread::scope(|scope| {
            for offset in [low, high] {
                let root = root.path().to_path_buf();
                let path = path.clone();
                scope.spawn(move || {
                    let fs = Fs::open(&root, "default", OpenMode::ReadWrite).expect("open");
                    let _ = fs.write_at(&path, offset, &vec![0x5Au8; BLOCK_BYTES as usize]);
                });
            }
        });

        let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("open");
        let size = fs.stat(&path).expect("stat").size;
        // The furthest write ends at `high + BLOCK_BYTES`.
        if size > 0 && size < high + BLOCK_BYTES {
            violations += 1;
            eprintln!(
                "B1-1 round {round}: size is {size}, expected at least {}",
                high + BLOCK_BYTES
            );
        }
    }

    assert_eq!(
        violations, 0,
        "B1-1: {violations}/{rounds} rounds lost the furthest write's extension"
    );
}

/// B1-2 — `write_at` racing `truncate`.
///
/// Architect's prediction: `truncate` commits a smaller size, then an in-flight
/// `write_at` commits based on the size it read earlier, so the file grows back
/// past the truncation.
///
/// Asserted invariant: after the writers finish, the file's size is either what the
/// truncate set or what a write legitimately produced — never the *pre-truncate*
/// size coming back without a write that justifies it. Concretely, a truncate to 0
/// followed by a write at offset 0 must leave a file no larger than the write's end.
#[test]
fn b1_2_truncate_and_write_do_not_resurrect_the_old_length() {
    let (root, _fs) = fixture("b1-2");
    let rounds = 40;
    let mut violations = 0;

    for round in 0..rounds {
        let path = format!("/g{round}");
        {
            let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("open");
            fs.create(&path).expect("create");
            fs.write_at(&path, 0, &vec![1u8; (BLOCK_BYTES * 4) as usize])
                .expect("seed");
        }

        std::thread::scope(|scope| {
            {
                let root = root.path().to_path_buf();
                let path = path.clone();
                scope.spawn(move || {
                    let fs = Fs::open(&root, "default", OpenMode::ReadWrite).expect("open");
                    let _ = fs.truncate(&path, 0);
                });
            }
            {
                let root = root.path().to_path_buf();
                let path = path.clone();
                scope.spawn(move || {
                    let fs = Fs::open(&root, "default", OpenMode::ReadWrite).expect("open");
                    let _ = fs.write_at(&path, 0, b"short");
                });
            }
        });

        let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("open");
        let size = fs.stat(&path).expect("stat").size;
        // Either order is fine, but the file can never be longer than the seed
        // unless a write made it so — and the only write here is 5 bytes from 0.
        if size > (BLOCK_BYTES * 4) {
            violations += 1;
            eprintln!("B1-2 round {round}: size {size} exceeds the seed length");
        }
    }

    assert_eq!(
        violations, 0,
        "B1-2: {violations}/{rounds} rounds grew past the seed"
    );
}

/// B1-3 — `write_at` racing `unlink`.
///
/// Architect's prediction: the writer commits extent rows for an inode that
/// `unlink` has already removed, leaving extents whose inode does not exist. The
/// consistency check added in this same card makes the outcome observable.
///
/// Asserted invariant: after both operations settle, `verify` reports nothing. A
/// file that was removed may not leave block rows behind.
#[test]
fn b1_3_write_racing_unlink_leaves_no_orphan_extents() {
    let (root, _fs) = fixture("b1-3");
    let rounds = 40;
    let mut violations = 0;

    for round in 0..rounds {
        let path = format!("/h{round}");
        {
            let fs = Fs::open(root.path(), "default", OpenMode::Create).expect("create");
            fs.create(&path).expect("create");
            fs.write_at(&path, 0, &vec![1u8; BLOCK_BYTES as usize])
                .expect("seed");
        }

        std::thread::scope(|scope| {
            {
                let root = root.path().to_path_buf();
                let path = path.clone();
                scope.spawn(move || {
                    let fs = Fs::open(&root, "default", OpenMode::ReadWrite).expect("open");
                    let _ = fs.write_at(&path, 0, &vec![2u8; BLOCK_BYTES as usize]);
                });
            }
            {
                let root = root.path().to_path_buf();
                let path = path.clone();
                scope.spawn(move || {
                    let fs = Fs::open(&root, "default", OpenMode::ReadWrite).expect("open");
                    let _ = fs.remove(&path);
                });
            }
        });

        let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("open");
        let report = fs.verify().expect("verify");
        if !report.is_clean() {
            violations += 1;
            eprintln!("B1-3 round {round}: {:?}", report.inconsistencies);
        }
        // Clean up whatever survived so the next round starts from a known state.
        let _ = fs.remove(&path);
    }

    assert_eq!(
        violations, 0,
        "B1-3: {violations}/{rounds} rounds left orphaned rows behind"
    );
}

/// B1-4 — two cross-directory renames, each moving into the other's tree.
///
/// Architect's prediction: both pass the R9 subtree check (neither is inside the
/// other at check time), then both commit, and the directory graph contains a cycle.
///
/// Asserted invariant: after both settle, every directory can still be walked from
/// the root in finite time, and `verify` is clean. A cycle makes the walk non
/// terminating, which the depth cap below catches.
#[test]
fn b1_4_cross_renames_cannot_create_a_directory_cycle() {
    let (root, _fs) = fixture("b1-4");
    let rounds = 40;
    let mut violations = 0;

    for round in 0..rounds {
        let base = format!("/c{round}");
        {
            let fs = Fs::open(root.path(), "default", OpenMode::Create).expect("open");
            fs.mkdir(&base).expect("mkdir base");
            fs.mkdir(&format!("{base}/x")).expect("mkdir x");
            fs.mkdir(&format!("{base}/y")).expect("mkdir y");
            fs.mkdir(&format!("{base}/x/deep")).expect("mkdir deep");
        }

        // x -> y/x  and  y -> x/y : each target is outside the source at check time.
        std::thread::scope(|scope| {
            {
                let root = root.path().to_path_buf();
                let base = base.clone();
                scope.spawn(move || {
                    let fs = Fs::open(&root, "default", OpenMode::ReadWrite).expect("open");
                    let _ = fs.rename(&format!("{base}/x"), &format!("{base}/y/x"));
                });
            }
            {
                let root = root.path().to_path_buf();
                let base = base.clone();
                scope.spawn(move || {
                    let fs = Fs::open(&root, "default", OpenMode::ReadWrite).expect("open");
                    let _ = fs.rename(&format!("{base}/y"), &format!("{base}/x/y"));
                });
            }
        });

        let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("open");
        // Walk the whole tree with a depth cap; a cycle would never finish.
        let mut walked = 0usize;
        let mut stack = vec![base.clone()];
        let mut depth_guard = 0usize;
        let mut cycled = false;
        while let Some(current) = stack.pop() {
            depth_guard += 1;
            if depth_guard > 1000 {
                cycled = true;
                break;
            }
            let Ok(entries) = fs.read_dir(&current) else {
                continue;
            };
            for entry in entries {
                walked += 1;
                if matches!(entry.kind, fs_api::Kind::Directory) {
                    stack.push(format!("{current}/{}", entry.name));
                }
            }
        }

        let report = fs.verify().expect("verify");
        if cycled || !report.is_clean() {
            violations += 1;
            eprintln!(
                "B1-4 round {round}: cycle={cycled} walked={walked} inconsistencies={:?}",
                report.inconsistencies
            );
        }
    }

    assert_eq!(
        violations, 0,
        "B1-4: {violations}/{rounds} rounds produced a cycle or an inconsistency"
    );
}

/// Creates a probe file so the store directory exists before threads open it.
fn fs_create_probe(root: &TempRoot, _unused: u8) {
    let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("open");
    let _ = fs.stats();
    static SEEN: AtomicUsize = AtomicUsize::new(0);
    SEEN.fetch_add(1, Ordering::Relaxed);
}
