//! Read-amplification criteria for `read_at_id` (P3-2 §1 criterion 1) — **two live assertions, no `#[ignore]`**.
//!
//! The criterion went through three readings before landing; this file is the final one, with the reason each shape was
//! rejected kept so nobody re-derives them:
//!
//! 1. "a 512-byte read must cost ≤ 24 KiB" — **unachievable by construction**: files ≤ 32 KiB are stored as a single
//!    `Content::Blob` and `base.rs` reads the whole object, while larger files are read in 32 KiB extents. The floor is
//!    one 32 KiB block, so the old bound was below the format's own minimum.
//! 2. "k = min(8, chunk count)" — a numerical coincidence (33,976 ≈ 8 × 4,110), not a mechanism.
//! 3. **Final**: two assertions that are true of *this* format and would still catch a regression:
//!    - **A** (single file): 64 KiB file, 512-byte read ⇒ **≤ 36 KiB**. It asserts "exactly one block plus index", which
//!      is the best this format allows;
//!    - **B** (across file sizes): `cost(256 KiB) ≤ cost(32 KiB) × 1.2`. It assumes nothing about the block size — only
//!      that reading 512 bytes does not get more expensive as the file grows. An implementation that read every extent's
//!      data scored **ratio 8.838** in the card's red run, so this is a live criterion rather than a formality.

mod common;

use std::sync::Arc;

use fs_namespace::{FileId, Path, Rules};
use fs_semantics::disposition::Disposition;
use fs_workspace::Repo;

fn p(text: &str) -> Path {
    Path::parse(text).expect("path")
}

struct Fixture {
    dir: std::path::PathBuf,
    repo: Arc<Repo>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(label: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "fs-workspace-amp-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let repo = Arc::new(Repo::open(dir.join("store"), true).expect("repo"));
    repo.init().expect("init");
    Fixture { dir, repo }
}

/// A committed file of `size` bytes, handle closed.
///
/// Committed on purpose: uncommitted content lives in the overlay's own pages and never reaches a pack, so the reading
/// would measure the overlay rather than the format's block granularity.
fn committed_file(fixture: &Fixture, name: &str, size: usize) -> FileId {
    let payload: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
    let workspace = fixture
        .repo
        .workspace_open("w1")
        .or_else(|_| fixture.repo.workspace_create("w1", "main"))
        .expect("workspace");
    let rules = Rules::new(workspace.store());
    let created = rules
        .create_or_open(&p(name), Disposition::Create)
        .expect("create");
    workspace.write_at(name, 0, &payload).expect("write");
    workspace
        .truncate(name, u64::try_from(size).expect("fits"))
        .expect("truncate");
    workspace.commit("seed", false).expect("commit");
    workspace.close().expect("close");
    created.file_id
}

/// Pack bytes read while serving a `len`-byte request at offset 0.
fn read_cost(fixture: &Fixture, file_id: FileId, len: usize) -> u64 {
    let workspace = fixture.repo.workspace_open("w1").expect("workspace");
    cas_store::testing::reset_pack_read_bytes();
    let bytes = workspace.read_at_id(file_id, 0, len).expect("read");
    assert_eq!(
        bytes.len(),
        len,
        "cell read: the request was answered in full"
    );
    let cost = cas_store::testing::pack_read_bytes();
    workspace.close().expect("close");
    cost
}

/// One 64 KiB file lives in 32 KiB extents, so a 512-byte read pays one block plus the index. A build that read the
/// whole file (or several blocks) would exceed the bound immediately.
#[test]
fn a_small_read_of_a_64_kib_file_costs_at_most_thirty_six_kib() {
    common::strict_gate();
    let _serialised = cas_store::testing::serialised();
    let fixture = fixture("single");
    let file_id = committed_file(&fixture, "/big", 64 * 1024);
    let cost = read_cost(&fixture, file_id, 512);
    assert!(
        cost <= 36 * 1024,
        "cell A: a 512-byte read of a 64 KiB file cost {cost} bytes; the format's floor is one 32 KiB block plus the \
         extent index (measured 33,976 in the card), so anything above 36 KiB means more than one block was read"
    );
}

/// The part that does not depend on the block size: **reading 512 bytes must not get more expensive as the file grows.**
///
/// This is the assertion that survives a future change of block size, and the one that catches "read every extent":
/// the card's red run scored 8.838 by doing exactly that.
#[test]
fn a_small_read_does_not_scale_with_the_file() {
    common::strict_gate();
    let _serialised = cas_store::testing::serialised();
    let fixture = fixture("ratio");
    let small = committed_file(&fixture, "/32k", 32 * 1024);
    let large = committed_file(&fixture, "/256k", 256 * 1024);

    let small_cost = read_cost(&fixture, small, 512);
    let large_cost = read_cost(&fixture, large, 512);
    assert!(
        small_cost > 0 && large_cost > 0,
        "cell B: both reads must read something"
    );
    let ratio = large_cost as f64 / small_cost as f64;
    assert!(
        ratio <= 1.2,
        "cell B: cost must not grow with file size — 32 KiB file cost {small_cost} bytes, 256 KiB file cost \
         {large_cost} bytes (ratio {ratio:.3}, limit 1.2). A ratio far above this means the read walked more extents \
         than the range covers."
    );
}
