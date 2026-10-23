//! **TreeStore contract tests (ADR-006 K1–K6).**
//!
//! These tests describe *behaviour that any namespace backend must provide*, not
//! how the current table backend implements it. They are written only against the
//! public `Fs` API so that the overlay backend planned for P2a must pass the same
//! file — a backend swap that breaks one of these has broken the contract, not a
//! test.
//!
//! `K1`/`K2` are the A1 defect, `K5` is the create_file symmetry case, and `K6` is
//! the KI-1 counterpart. `K3`/`K4` are the B1 concurrency contract and live in
//! `tree_store_contract_concurrency.rs`.
//!
//! **Why these duplicate `referential.rs` and `consistency.rs`:** those tests
//! confirm the table backend was *fixed*; these state the rule a future backend
//! must satisfy before it may be swapped in. The duplication is the point — one
//! file is a regression test for today's implementation, the other is the
//! acceptance gate for tomorrow's.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

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
                "fs-contract-{label}-{}-{}-{sequence}",
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

/// A block-sized body, so the file uses extents rather than a single object.
fn blocky(byte: u8) -> Vec<u8> {
    vec![byte; BLOCK_BYTES as usize]
}

fn read_all(fs: &Fs, path: &str) -> Vec<u8> {
    let size = fs.stat(path).expect("stat").size as usize;
    let mut buffer = vec![0xAAu8; size];
    let read = fs.read_at(path, 0, &mut buffer).expect("read");
    buffer.truncate(read);
    buffer
}

// --------------------------------------------------------------------- K1

/// **K1** — after `unlink`, the deleted inode's metadata and content are entirely
/// unreachable.
///
/// The observable statement: the path is gone, `verify` is clean (so no inode or
/// extent row survives to name the old content), and the store's reachable object
/// count does not include the deleted content any more.
#[test]
fn k1_unlink_leaves_nothing_reachable() {
    let (_root, fs) = fixture("k1");
    fs.create("/keep").expect("create keep");
    fs.write_at("/keep", 0, b"kept").expect("write keep");
    fs.create("/gone").expect("create gone");
    fs.write_at("/gone", 0, &blocky(9)).expect("write gone");

    let before = fs.verify().expect("verify");
    assert!(before.is_clean(), "{:?}", before.inconsistencies);

    fs.remove("/gone").expect("remove");

    // The name no longer resolves.
    assert!(fs.stat("/gone").is_err(), "/gone must be gone");
    // Nothing dangles and nothing is orphaned.
    let after = fs.verify().expect("verify");
    assert!(
        after.is_clean(),
        "K1: the deleted inode must leave no unreachable rows: {:?}",
        after.inconsistencies
    );
    // Exactly one reference remains — the block of `/keep` is too small to have
    // extents, so the surviving content is the single small-file object.
    assert_eq!(
        after.references, 1,
        "only /keep's object should remain referenced"
    );
    // The surviving file is untouched.
    assert_eq!(read_all(&fs, "/keep"), b"kept");
}

// --------------------------------------------------------------------- K2

/// **K2** — after a rename overwrites a file, the overwritten inode is as
/// unreachable as if it had been unlinked.
#[test]
fn k2_rename_over_a_file_leaves_the_old_content_unreachable() {
    let (_root, fs) = fixture("k2");
    fs.create("/src").expect("create src");
    fs.write_at("/src", 0, &blocky(1)).expect("write src");
    fs.create("/dst").expect("create dst");
    fs.write_at("/dst", 0, &blocky(2)).expect("write dst");

    fs.rename("/src", "/dst").expect("rename over");

    // The destination now holds the source's bytes.
    assert!(read_all(&fs, "/dst").iter().all(|byte| *byte == 1));
    // And the contract: nothing unreachable, nothing dangling.
    let report = fs.verify().expect("verify");
    assert!(
        report.is_clean(),
        "K2: the overwritten inode must leave no unreachable rows: {:?}",
        report.inconsistencies
    );
    assert_eq!(
        report.references, 1,
        "only the surviving file's content should be referenced"
    );
}

/// **K2** for the directory case: replacing an empty directory leaves nothing
/// behind either.
#[test]
fn k2_rename_over_an_empty_directory_leaves_nothing_unreachable() {
    let (_root, fs) = fixture("k2-dir");
    fs.mkdir("/src").expect("mkdir src");
    fs.mkdir("/dst").expect("mkdir dst");

    fs.rename("/src", "/dst").expect("rename over empty dir");

    let report = fs.verify().expect("verify");
    assert!(
        report.is_clean(),
        "K2: the replaced directory must leave no unreachable inode: {:?}",
        report.inconsistencies
    );
}

// --------------------------------------------------------------------- K5

/// **K5** — a same-path `create` race has exactly one winner.
///
/// This is the `create_file` twin of the `mkdir` case, added on QA's suggestion:
/// both go through the same "check then insert" shape, so both need the same
/// guarantee. The losers must receive a typed `AlreadyExists`, never a raw
/// constraint error — that was a real defect found in P2-0a.
#[test]
fn k5_same_path_create_has_exactly_one_winner() {
    const THREADS: usize = 4;
    let (root, _fs) = fixture("k5");

    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let root = root.path().to_path_buf();
                scope.spawn(move || {
                    let fs = Fs::open(&root, "default", OpenMode::ReadWrite).expect("open");
                    fs.create("/race")
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect()
    });

    let winners = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    assert_eq!(winners, 1, "K5: exactly one creator may succeed");

    for outcome in &outcomes {
        match outcome {
            Ok(()) => {}
            // The documented refusal.
            Err(fs_api::FsError::AlreadyExists { .. }) => {}
            // Losing the write lane is the other typed outcome (ADR-003 §7).
            Err(fs_api::FsError::Store(cas_store::StoreError::Busy)) => {}
            Err(other) => panic!("K5: unexpected failure {other:?}"),
        }
    }

    // And the store stays consistent afterwards.
    let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("reopen");
    assert!(fs.verify().expect("verify").is_clean());
}

// --------------------------------------------------------------------- K6

/// **K6** — truncating to an exact block boundary produces no surplus object.
///
/// The KI-1 counterpart: a boundary that falls exactly on a block edge has no
/// partial block, so nothing new may be published.
#[test]
fn k6_truncate_to_a_block_boundary_creates_no_surplus_object() {
    let (_root, fs) = fixture("k6");
    let block = BLOCK_BYTES as usize;
    let mut body = Vec::new();
    for index in 0..3u8 {
        body.extend(std::iter::repeat_n(index + 1, block));
    }
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &body).expect("write");

    let objects_before = fs.stats().expect("stats").objects;
    fs.truncate("/f", (block * 2) as u64)
        .expect("truncate to the edge");

    assert_eq!(fs.stat("/f").expect("stat").size, (block * 2) as u64);
    assert_eq!(
        fs.stats().expect("stats").objects,
        objects_before,
        "K6: an aligned truncate must not publish an object"
    );
    // The surviving bytes are the first two blocks.
    assert_eq!(read_all(&fs, "/f"), body[..block * 2].to_vec());
    // And the namespace stays consistent.
    assert!(fs.verify().expect("verify").is_clean());
}

/// **K6** for a shrink inside a block: the boundary block is rewritten, and the
/// result reads back as the surviving prefix.
#[test]
fn k6_truncate_inside_a_block_keeps_the_surviving_prefix() {
    let (_root, fs) = fixture("k6-inside");
    let block = BLOCK_BYTES as usize;
    let mut body = Vec::new();
    for index in 0..2u8 {
        body.extend(std::iter::repeat_n(index + 1, block));
    }
    fs.create("/f").expect("create");
    fs.write_at("/f", 0, &body).expect("write");

    let keep = block + 1234;
    fs.truncate("/f", keep as u64).expect("truncate inside");

    assert_eq!(read_all(&fs, "/f"), body[..keep].to_vec());
    assert!(fs.verify().expect("verify").is_clean());
}

// --------------------------------------------------------------------- K3/K4 support

/// **K3** (single-threaded half) — a failed operation leaves no partial state.
///
/// The concurrency half lives in `tree_store_contract_concurrency.rs`; this states
/// the invariant the lock provides: a rejected operation changes nothing.
#[test]
fn k3_a_rejected_operation_changes_nothing() {
    let (_root, fs) = fixture("k3");
    fs.mkdir("/d").expect("mkdir");
    fs.create("/d/f").expect("create");
    let before_records = fs.root_records().expect("records").len();
    let before_files = fs.read_dir("/d").expect("read_dir").len();

    // A non-empty directory cannot be removed.
    assert!(fs.remove("/d").is_err());
    // A rename into its own subtree is refused.
    assert!(fs.rename("/d", "/d/sub/x").is_err());

    assert_eq!(fs.read_dir("/d").expect("read_dir").len(), before_files);
    assert_eq!(
        fs.root_records().expect("records").len(),
        before_records,
        "K3: a rejected operation must not publish a root"
    );
    assert!(fs.verify().expect("verify").is_clean());
}

// --------------------------------------------------------------------- K1/K2 补强

/// **K1** — `rmdir` must remove the directory's inode, not just its name.
///
/// Architect reported the cascade gap in `unlink` and `rename` R3; `rmdir` has the
/// same shape and was found while enumerating every removal path for this contract.
/// An empty directory still owns a `file` row.
#[test]
fn k1_rmdir_removes_the_directory_inode() {
    let (_root, fs) = fixture("k1-rmdir");
    fs.mkdir("/d").expect("mkdir");
    fs.mkdir("/d/empty").expect("mkdir inner");

    fs.remove("/d/empty").expect("rmdir inner");
    fs.remove("/d").expect("rmdir");

    let report = fs.verify().expect("verify");
    assert!(
        report.is_clean(),
        "K1: rmdir must leave no unreachable inode: {:?}",
        report.inconsistencies
    );
    assert!(fs.stat("/d").is_err());
}

/// **K2** — replacing an empty directory with `rename` must remove the replaced
/// directory's inode as well as its name.
///
/// This is the R7 branch: architect's report covered R3 (file over file) but R7 has
/// the same "replace the destination, drop its inode" obligation.
#[test]
fn k2_rename_over_an_empty_directory_removes_the_replaced_inode() {
    let (_root, fs) = fixture("k2-r7");
    fs.mkdir("/src").expect("mkdir src");
    fs.mkdir("/dst").expect("mkdir dst");

    fs.rename("/src", "/dst").expect("rename over");

    let report = fs.verify().expect("verify");
    assert!(
        report.is_clean(),
        "K2/R7: the replaced directory must leave no unreachable inode: {:?}",
        report.inconsistencies
    );
    assert!(fs.stat("/dst").is_ok());
    assert!(fs.stat("/src").is_err());
}

/// **K1** across repeated create/remove cycles: no residue accumulates.
///
/// A leak that only appears after several rounds would be invisible to a
/// create-once-remove-once test.
#[test]
fn k1_repeated_create_and_remove_cycles_leave_no_residue() {
    let (_root, fs) = fixture("k1-cycles");
    for round in 0..10 {
        let path = format!("/cycle{round}");
        fs.create(&path).expect("create");
        fs.write_at(&path, 0, &blocky(round as u8)).expect("write");
        fs.remove(&path).expect("remove");

        let report = fs.verify().expect("verify");
        assert!(
            report.is_clean(),
            "K1: round {round} left residue: {:?}",
            report.inconsistencies
        );
    }
}
