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

/// Extending a file after a shrink must read **zeros**, never the bytes the shrink cut.
///
/// Found by the 10×1000 soak at `seed=1000 step=357`: `/n2` held `0xE9` over `[104,159)`, was
/// truncated to 66, then grew back to 181 — the workspace returned the old `0xE9` bytes for
/// `[104,154)`, the model zeros. ADR-006 §7.3 says truncate deletes pages past the new end
/// *and* trims the boundary page's tail ("把边界页的尾部截掉 … 之后再扩展读到的都是零"). The
/// overlay did the first half only: the page straddling the new end kept its bytes, and the
/// next extension made them visible again — KI-1's mirror image, truncate handing bytes back
/// instead of losing them.
#[test]
fn growing_a_shrunk_file_reads_zeros_not_the_cut_bytes() {
    let root = TempRoot::new("truncate-resurrect");
    let repo = Repo::create(root.path.join("store")).expect("create");
    let workspace = repo.workspace_create("w1", "main").expect("create");

    workspace.create("/n2").expect("create");
    // The soak's shape: a byte at 59 that survives, a fill at [104,159) that does not.
    workspace.write_at("/n2", 59, &[0xF2]).expect("write");
    workspace
        .write_at("/n2", 104, &[0xE9; 55])
        .expect("write fill");
    workspace.truncate("/n2", 66).expect("shrink");
    workspace.truncate("/n2", 181).expect("grow");

    let mut buffer = vec![0u8; 181];
    let read = workspace.read_at("/n2", 0, &mut buffer).expect("read");
    assert_eq!(read, 181, "the file must be its full new size");
    assert_eq!(buffer[59], 0xF2, "the surviving byte must survive");
    assert_eq!(
        &buffer[66..],
        &[0u8; 115][..],
        "the extension must be zeros, not the bytes truncate cut"
    );

    // The resurrected bytes must not come back through a write either: the page below the
    // write keeps its zeros.
    workspace.write_at("/n2", 170, &[0x11]).expect("write");
    let mut buffer = vec![0u8; 181];
    workspace.read_at("/n2", 0, &mut buffer).expect("read");
    assert_eq!(&buffer[66..170], &[0u8; 104][..], "zeros beside the write");
    assert_eq!(buffer[170], 0x11, "the write itself");

    workspace.close().expect("close");
}

/// The same defect across a page boundary: pages past the new end are dropped, so they can
/// only come back as zeros.
#[test]
fn growing_a_shrunk_file_across_pages_reads_zeros() {
    let root = TempRoot::new("truncate-resurrect-pages");
    let repo = Repo::create(root.path.join("store")).expect("create");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    let page = fs_overlay::schema::PAGE_BYTES;
    let body: Vec<u8> = (0..page * 3).map(|index| (index % 251) as u8).collect();

    workspace.create("/f").expect("create");
    workspace.write_at("/f", 0, &body).expect("write");
    workspace.truncate("/f", 66).expect("shrink");
    workspace.truncate("/f", page * 3).expect("grow");

    let mut buffer = vec![0u8; (page * 3) as usize];
    let read = workspace.read_at("/f", 0, &mut buffer).expect("read");
    assert_eq!(read as u64, page * 3);
    assert_eq!(&buffer[..66], &body[..66], "the surviving prefix");
    assert_eq!(
        &buffer[66..],
        &vec![0u8; (page * 3 - 66) as usize][..],
        "every byte past the shrink point must be zero after the extension"
    );

    workspace.close().expect("close");
}

/// Shrinking all the way to zero and growing again reads zeros: the `new_size == 0` path
/// drops every page, so nothing can be resurrected. Kept because the fix has an explicit
/// branch for it (there is no straddling page to zero).
#[test]
fn shrinking_to_zero_and_growing_reads_zeros() {
    let root = TempRoot::new("truncate-zero");
    let repo = Repo::create(root.path.join("store")).expect("create");
    let workspace = repo.workspace_create("w1", "main").expect("create");

    workspace.create("/f").expect("create");
    workspace.write_at("/f", 0, &[0xEE; 200]).expect("write");
    workspace.truncate("/f", 0).expect("shrink to zero");
    workspace.truncate("/f", 200).expect("grow");

    let mut buffer = vec![0x5Au8; 200];
    let read = workspace.read_at("/f", 0, &mut buffer).expect("read");
    assert_eq!(read, 200);
    assert_eq!(
        buffer,
        vec![0u8; 200],
        "a zero-length file has no bytes to give back"
    );

    workspace.close().expect("close");
}

/// The invariant has to survive a **close and reopen of the uncommitted overlay**.
///
/// This is the persistence path that matters for R3-1: the cut tail lives in `ov_page`, which
/// is on disk, so reopening the workspace and extending must still read zeros. It is also the
/// only persistence path this defect is visible through — `snapshot`/`commit` copy the content
/// into the base and mask the stale page behind `base_limit` (see the test below), so green
/// there says nothing about the defect.
#[test]
fn a_reopened_workspace_does_not_resurrect_cut_bytes() {
    let root = TempRoot::new("truncate-reopen");
    let repo = Repo::create(root.path.join("store")).expect("create");

    let workspace = repo.workspace_create("w1", "main").expect("create");
    workspace.create("/f").expect("create");
    workspace.write_at("/f", 0, &[0xEE; 200]).expect("write");
    workspace.truncate("/f", 66).expect("shrink");
    workspace.flush().expect("flush");
    workspace.close().expect("close without commit");

    let workspace = repo.workspace_open("w1").expect("reopen");
    workspace.truncate("/f", 181).expect("grow");
    let mut buffer = vec![0u8; 181];
    workspace.read_at("/f", 0, &mut buffer).expect("read");
    assert_eq!(&buffer[..66], &[0xEE; 66][..], "the surviving prefix");
    assert_eq!(&buffer[66..], &[0u8; 115][..], "the reopened tail");

    workspace.close().expect("close");
}

/// Coverage, not a regression: `commit` publishes the content into a new base, so the stale
/// page is masked by `base_limit` rather than fixed — this passes on the unfixed tree too
/// (QA measured it). It stays because the published side can be wrong in its own ways: the
/// commit must not carry cut bytes into the materialised snapshot, and the commit's history
/// must read back the truncated length and nothing else.
#[test]
fn committing_a_shrunk_file_stores_only_the_prefix() {
    let root = TempRoot::new("truncate-commit");
    let repo = Repo::create(root.path.join("store")).expect("create");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");

    workspace.create("/f").expect("create");
    workspace.write_at("/f", 0, &[0xEE; 200]).expect("write");
    workspace.truncate("/f", 66).expect("shrink");
    let commit = workspace.commit("shrunk", false).expect("commit");

    let view = repo.read_view(&commit).expect("read_view");
    let mut buffer = vec![0u8; 181];
    let read = view.read_at("/f", 0, &mut buffer).expect("read at commit");
    assert_eq!(read, 66, "the published file is the truncated one");
    assert_eq!(&buffer[..66], &[0xEE; 66][..]);

    workspace.truncate("/f", 181).expect("grow");
    let mut buffer = vec![0u8; 181];
    workspace.read_at("/f", 0, &mut buffer).expect("read");
    assert_eq!(&buffer[66..], &[0u8; 115][..], "and so does the extension");

    workspace.close().expect("close");
}

/// Coverage, not a regression: `snapshot` materialises the content just like `commit`, so the
/// stale page is masked rather than fixed (KI-9's table). It is here so the published path's
/// own invariants — snapshot carries the truncated length, extension still reads zeros — are
/// pinned; it must not be used as an A/B judge.
#[test]
fn snapshotting_a_shrunk_file_then_growing_reads_zeros() {
    let root = TempRoot::new("truncate-snapshot");
    let repo = Repo::create(root.path.join("store")).expect("create");
    let mut workspace = repo.workspace_create("w1", "main").expect("create");

    workspace.create("/f").expect("create");
    workspace.write_at("/f", 0, &[0xEE; 200]).expect("write");
    workspace.truncate("/f", 66).expect("shrink");
    workspace.snapshot().expect("snapshot");

    let mut buffer = vec![0u8; 181];
    workspace.read_at("/f", 0, &mut buffer).expect("read");
    assert_eq!(&buffer[..66], &[0xEE; 66][..]);

    workspace.truncate("/f", 181).expect("grow");
    let mut buffer = vec![0u8; 181];
    workspace.read_at("/f", 0, &mut buffer).expect("read");
    assert_eq!(&buffer[66..], &[0u8; 115][..]);

    workspace.close().expect("close");
}
