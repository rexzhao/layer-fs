//! **TreeStore contract: K3 (no check-then-use window) and K4 (no cycles).**
//!
//! These are the concurrency half of the ADR-006 K1–K6 contract. They are written
//! against the public API only, so a future backend must satisfy the same file.
//!
//! ## What the table backend does today
//!
//! P2-0b ran architect's four predictions against the table backend
//! (`b1_races.rs`, kept as the reproducer). Results:
//!
//! | prediction | reproduced? |
//! | --- | --- |
//! | B1-1 concurrent writes to different blocks lose the furthest extension | **yes** |
//! | B1-2 `truncate` racing `write_at` resurrects the old length | no |
//! | B1-3 `write_at` racing `unlink` leaves orphan extents | no (the A1 cascade hides it) |
//! | B1-4 cross-directory renames form a cycle | no |
//!
//! B1-1 is a real data-loss window: `write_at` reads the file's size *outside* the
//! transaction that commits the new one, so two writers can both compute their new
//! extent list from the same starting size and the later commit drops the earlier
//! writer's blocks. It is **not fixed here** — the table backend is replaced in P2a,
//! and the overlay backend's single-writer rule is meant to remove the window
//! structurally rather than patch it.
//!
//! `k3_concurrent_writes_to_different_blocks_keep_both` therefore **fails on the
//! table backend by design**. It is written as a contract test so that the overlay
//! backend must pass it: a green run there is the architectural proof that the
//! window is gone. It is `#[ignore]`d by default so the table backend's known
//! failure does not mask a real regression in the ordinary suite; run it with
//! `cargo test -p fs-api --test tree_store_contract_concurrency -- --ignored`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use fs_api::{Fs, Kind, OpenMode, BLOCK_BYTES};

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
                "fs-contract-conc-{label}-{}-{}-{sequence}",
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

/// **K3** — concurrent writes to disjoint blocks of one file must both survive.
///
/// Under linearizable semantics this assertion is exactly right, not too strong:
/// the overlay runs each operation as one complete transaction under a handle mutex
/// held until commit (ADR-006 §2.5, §7.4 inference 2), so two writes to disjoint
/// ranges have only two possible serial orders — write-low-then-high and
/// write-high-then-low — and **both orders produce the same final bytes**, because
/// neither range overlaps the other. Asserting "both survive" therefore does not
/// pick an order; it states the one result every serial order agrees on.
///
/// On the table backend this fails: it is the B1-1 reproduction (the size and extent
/// layout are read outside the committing transaction). `#[ignore]`d so that known
/// failure does not hide a real regression, and so the overlay backend can be held
/// to it.
#[test]
#[ignore = "fails on the table backend by design (this is B1-1); the overlay backend must pass it"]
fn k3_concurrent_writes_to_different_blocks_keep_both() {
    let (root, _fs) = fixture("k3-blocks");
    let rounds = 40;
    let mut violations = 0;

    for round in 0..rounds {
        let path = format!("/f{round}");
        {
            let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("open");
            fs.create(&path).expect("create");
        }

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
        if size < high + BLOCK_BYTES {
            violations += 1;
        }
    }

    assert_eq!(
        violations, 0,
        "K3: {violations}/{rounds} rounds lost a concurrent writer's block"
    );
}

/// **K3** — a `write_at` that races a `truncate` must not restore the old length.
///
/// Passes on the table backend today; kept as a contract test because the overlay
/// backend must also pass it.
#[test]
fn k3_truncate_and_write_settle_to_a_valid_size() {
    let (root, _fs) = fixture("k3-truncate");
    let seed = BLOCK_BYTES * 4;
    let mut violations = 0;

    for round in 0..40 {
        let path = format!("/g{round}");
        {
            let fs = Fs::open(root.path(), "default", OpenMode::Create).expect("open");
            fs.create(&path).expect("create");
            fs.write_at(&path, 0, &vec![1u8; seed as usize])
                .expect("seed");
        }

        std::thread::scope(|scope| {
            for action in 0..2 {
                let root = root.path().to_path_buf();
                let path = path.clone();
                scope.spawn(move || {
                    let fs = Fs::open(&root, "default", OpenMode::ReadWrite).expect("open");
                    if action == 0 {
                        let _ = fs.truncate(&path, 0);
                    } else {
                        let _ = fs.write_at(&path, 0, b"short");
                    }
                });
            }
        });

        let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("open");
        if fs.stat(&path).expect("stat").size > seed {
            violations += 1;
        }
    }

    assert_eq!(
        violations, 0,
        "K3: {violations}/40 rounds grew the file past any write that justified it"
    );
}

/// **K3** — `write_at` racing `unlink` must leave the namespace consistent.
#[test]
fn k3_write_racing_unlink_leaves_nothing_orphaned() {
    let (root, _fs) = fixture("k3-unlink");
    let mut violations = 0;

    for round in 0..40 {
        let path = format!("/h{round}");
        {
            let fs = Fs::open(root.path(), "default", OpenMode::Create).expect("open");
            fs.create(&path).expect("create");
            fs.write_at(&path, 0, &vec![1u8; BLOCK_BYTES as usize])
                .expect("seed");
        }

        std::thread::scope(|scope| {
            for action in 0..2 {
                let root = root.path().to_path_buf();
                let path = path.clone();
                scope.spawn(move || {
                    let fs = Fs::open(&root, "default", OpenMode::ReadWrite).expect("open");
                    if action == 0 {
                        let _ = fs.remove(&path);
                    } else {
                        let _ = fs.write_at(&path, 0, &vec![2u8; BLOCK_BYTES as usize]);
                    }
                });
            }
        });

        let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("open");
        let report = fs.verify().expect("verify");
        if !report.is_clean() {
            violations += 1;
        }
        let _ = fs.remove(&path);
    }

    assert_eq!(
        violations, 0,
        "K3: {violations}/40 rounds left rows for a removed inode"
    );
}

/// **K4** — two cross-directory renames must never create a directory cycle.
#[test]
fn k4_cross_renames_never_form_a_cycle() {
    let (root, _fs) = fixture("k4");
    let mut violations = 0;

    for round in 0..40 {
        let base = format!("/c{round}");
        {
            let fs = Fs::open(root.path(), "default", OpenMode::Create).expect("open");
            fs.mkdir(&base).expect("mkdir base");
            fs.mkdir(&format!("{base}/x")).expect("mkdir x");
            fs.mkdir(&format!("{base}/y")).expect("mkdir y");
            fs.mkdir(&format!("{base}/x/deep")).expect("mkdir deep");
        }

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
        // Walk with a step cap: a cycle never finishes.
        let mut stack = vec![base.clone()];
        let mut steps = 0usize;
        let mut cycled = false;
        while let Some(current) = stack.pop() {
            steps += 1;
            if steps > 1000 {
                cycled = true;
                break;
            }
            let Ok(entries) = fs.read_dir(&current) else {
                continue;
            };
            for entry in entries {
                if entry.kind == Kind::Directory {
                    stack.push(format!("{current}/{}", entry.name));
                }
            }
        }

        if cycled || !fs.verify().expect("verify").is_clean() {
            violations += 1;
        }
    }

    assert_eq!(violations, 0, "K4: {violations}/40 rounds formed a cycle");
}
// ============================================================================
// K3a–K3d — the four inferences from ADR-006 §7.4 (architect, 7a6cf49).
//
// Every assertion has the same shape:
//
//     result ∈ { outcome of serial order A, outcome of serial order B }
//
// Under linearizable semantics a concurrent pair must settle on exactly one of the
// two orders, so a result matching neither is a violation. This form is deliberately
// weaker than "matches order A" — it does not demand a particular interleaving, only
// that one of them happened.
//
// All four are `#[ignore]`d: the table backend is not linearizable for concurrent
// writes to one file (B1-1), and these are the overlay backend's admission gates.
// ============================================================================

use std::sync::Mutex;

/// **K3a** — concurrent writes to *overlapping* ranges settle on one serial order,
/// and the overlap is never byte-interleaved.
#[test]
#[ignore = "table backend is not linearizable here (B1-1); the overlay backend must pass it"]
fn k3a_overlapping_concurrent_writes_match_one_serial_order() {
    let (root, _fs) = fixture("k3a");
    let mut violations = 0;

    for round in 0..40 {
        let path = format!("/o{round}");
        {
            let fs = Fs::open(root.path(), "default", OpenMode::Create).expect("open");
            fs.create(&path).expect("create");
            fs.write_at(&path, 0, &[0u8; 64]).expect("seed");
        }

        // Two writes whose ranges overlap on bytes 20..44.
        let first_offset = 10u64;
        let first_bytes = vec![1u8; 34]; // 10..44
        let second_offset = 20u64;
        let second_bytes = vec![2u8; 34]; // 20..54
        std::thread::scope(|scope| {
            for (offset, bytes) in [
                (first_offset, first_bytes.clone()),
                (second_offset, second_bytes.clone()),
            ] {
                let root = root.path().to_path_buf();
                let path = path.clone();
                scope.spawn(move || {
                    let fs = Fs::open(&root, "default", OpenMode::ReadWrite).expect("open");
                    let _ = fs.write_at(&path, offset, &bytes);
                });
            }
        });

        // The two serial orders, computed rather than assumed.
        let mut a_then_b = vec![0u8; 64];
        a_then_b[10..44].copy_from_slice(&first_bytes);
        a_then_b[20..54].copy_from_slice(&second_bytes);
        let mut b_then_a = vec![0u8; 64];
        b_then_a[20..54].copy_from_slice(&second_bytes);
        b_then_a[10..44].copy_from_slice(&first_bytes);

        let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("open");
        let mut got = vec![0xAAu8; 64];
        let read = fs.read_at(&path, 0, &mut got).expect("read");
        got.truncate(read);

        if got != a_then_b && got != b_then_a {
            violations += 1;
        }
    }

    assert_eq!(
        violations, 0,
        "K3a: {violations}/40 rounds produced bytes matching neither serial order \
         (byte-level interleaving is not allowed)"
    );
}

/// **K3b** — `write_at` and `truncate` racing settle on one of the two orders.
#[test]
#[ignore = "table backend is not linearizable here (B1-1); the overlay backend must pass it"]
fn k3b_write_and_truncate_match_one_serial_order() {
    let (root, _fs) = fixture("k3b");
    let seed: usize = 1024;
    let mut violations = 0;

    for round in 0..40 {
        let path = format!("/t{round}");
        {
            let fs = Fs::open(root.path(), "default", OpenMode::Create).expect("open");
            fs.create(&path).expect("create");
            fs.write_at(&path, 0, &vec![7u8; seed]).expect("seed");
        }

        std::thread::scope(|scope| {
            for action in 0..2 {
                let root = root.path().to_path_buf();
                let path = path.clone();
                scope.spawn(move || {
                    let fs = Fs::open(&root, "default", OpenMode::ReadWrite).expect("open");
                    if action == 0 {
                        let _ = fs.write_at(&path, 100, &[9u8; 50]);
                    } else {
                        let _ = fs.truncate(&path, 10);
                    }
                });
            }
        });

        // Order A: write 100..150, then truncate to 10 -> ten 7s.
        let order_a = vec![7u8; 10];
        // Order B: truncate to 10, then write 100..150 -> ten 7s, zeros, fifty 9s.
        let mut order_b = vec![7u8; 150];
        for byte in order_b.iter_mut().take(150).skip(10) {
            *byte = 0;
        }
        order_b[100..150].copy_from_slice(&[9u8; 50]);

        let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("open");
        let size = fs.stat(&path).expect("stat").size as usize;
        let mut got = vec![0xAAu8; size];
        let read = fs.read_at(&path, 0, &mut got).expect("read");
        got.truncate(read);

        if got != order_a && got != order_b {
            violations += 1;
        }
    }

    assert_eq!(
        violations, 0,
        "K3b: {violations}/40 rounds produced bytes matching neither serial order"
    );
}

/// **K3c** — `write_at` racing `unlink` settles on one order, and when the unlink
/// wins the write fails and leaves nothing behind (K1).
#[test]
#[ignore = "table backend is not linearizable here (B1-1); the overlay backend must pass it"]
fn k3c_write_and_unlink_match_one_serial_order() {
    let (root, _fs) = fixture("k3c");
    let mut violations = 0;

    for round in 0..40 {
        let path = format!("/u{round}");
        {
            let fs = Fs::open(root.path(), "default", OpenMode::Create).expect("open");
            fs.create(&path).expect("create");
            fs.write_at(&path, 0, &vec![1u8; BLOCK_BYTES as usize])
                .expect("seed");
        }

        let wrote = Mutex::new(false);
        std::thread::scope(|scope| {
            for action in 0..2 {
                let root = root.path().to_path_buf();
                let path = path.clone();
                let wrote = &wrote;
                scope.spawn(move || {
                    let fs = Fs::open(&root, "default", OpenMode::ReadWrite).expect("open");
                    if action == 0 {
                        let _ = fs.remove(&path);
                    } else if fs
                        .write_at(&path, 0, &vec![2u8; BLOCK_BYTES as usize])
                        .is_ok()
                    {
                        *wrote.lock().expect("lock") = true;
                    }
                });
            }
        });

        let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("open");
        // Under either order the namespace must stay consistent (K1).
        if !fs.verify().expect("verify").is_clean() {
            violations += 1;
        }
        // Order A (write then unlink): gone. Order B (unlink then write): gone, the
        // write failed. Neither order may leave a file that the writer did not write.
        let exists = fs.stat(&path).is_ok();
        let did_write = *wrote.lock().expect("lock");
        if exists && !did_write {
            violations += 1;
        }
        let _ = fs.remove(&path);
    }

    assert_eq!(
        violations, 0,
        "K3c: {violations}/40 rounds left an outcome that matches neither serial order"
    );
}

/// **K3d** — two cross-directory renames racing: at most one commits (K4).
#[test]
#[ignore = "table backend is not linearizable here (B1-1); the overlay backend must pass it"]
fn k3d_cross_renames_admit_at_most_one_winner() {
    let (root, _fs) = fixture("k3d");
    let mut violations = 0;

    for round in 0..40 {
        let base = format!("/r{round}");
        {
            let fs = Fs::open(root.path(), "default", OpenMode::Create).expect("open");
            fs.mkdir(&base).expect("mkdir base");
            fs.mkdir(&format!("{base}/x")).expect("mkdir x");
            fs.mkdir(&format!("{base}/y")).expect("mkdir y");
            fs.mkdir(&format!("{base}/x/deep")).expect("mkdir deep");
        }

        let successes = Mutex::new(0usize);
        std::thread::scope(|scope| {
            for (from, to) in [
                (format!("{base}/x"), format!("{base}/y/x")),
                (format!("{base}/y"), format!("{base}/x/y")),
            ] {
                let root = root.path().to_path_buf();
                let successes = &successes;
                scope.spawn(move || {
                    let fs = Fs::open(&root, "default", OpenMode::ReadWrite).expect("open");
                    if fs.rename(&from, &to).is_ok() {
                        *successes.lock().expect("lock") += 1;
                    }
                });
            }
        });

        // Both succeeding would mean each moved into the other's tree: a cycle.
        let both = *successes.lock().expect("lock") > 1;
        let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("open");
        if both || !fs.verify().expect("verify").is_clean() {
            violations += 1;
        }
    }

    assert_eq!(
        violations, 0,
        "K3d: {violations}/40 rounds let both cross-renames commit"
    );
}
