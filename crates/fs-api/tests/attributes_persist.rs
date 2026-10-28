//! ④ The attribute replacement policy **on disk** (P3-3b, closing the card).
//!
//! The mock already proves the two-way rule in memory; this proves it survives a **reopen** of the workspace. That is the
//! only evidence that the value reached the overlay column rather than living in a handle or a cache — the storage-side
//! twin of G3's "`sd_v2_shape` asserts the encoded root, not the in-memory view".
//!
//! Two directions, separately asserted, because "always replaces" and "never replaces" each satisfy one alone:
//!
//! | disposition | content | stored attributes after reopen |
//! | --- | --- | --- |
//! | SUPERSEDE | dropped | replaced (back to nothing set) |
//! | OVERWRITE / OVERWRITE_IF | dropped | kept |
//!
//! `close()` then `workspace_open` is deliberate: `set_attributes` followed by `get_info` in the same session would pass
//! for an implementation that kept the bits in memory.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use fs_api::winfsp_backend::ApiBackend;
use fs_semantics::disposition::Disposition;
use fs_winfsp::ops::Path;
use fs_winfsp::{Attributes, WorkspaceOps};
use fs_workspace::Repo;

struct Fixture {
    dir: PathBuf,
    repo: Arc<Repo>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Fixture {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fs-api-attr-persist-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let repo = Arc::new(Repo::open(&dir, true).expect("repo"));
        repo.init().expect("init");
        Self { dir, repo }
    }
}

/// Runs one disposition in its own workspace, reopens, and reports the stored attributes seen after the reopen.
fn attributes_after_reopen(label: &str, disposition: Disposition, set: u32) -> u32 {
    let fixture = Fixture::new(label);
    let id = {
        let workspace = fixture
            .repo
            .workspace_create("w1", "main")
            .expect("workspace");
        let backend = ApiBackend::new(&workspace);
        let id = backend
            .create_or_open(&Path::new(b"/f"), Disposition::Create)
            .expect("create")
            .file_id;
        backend
            .set_attributes(id, Attributes(set))
            .expect("set attributes");
        let before = backend.get_info(id).expect("info").attributes.0;
        assert_eq!(
            before, set,
            "cell setup: the stored bits are reported as-is (no NORMAL is added while another bit is set)"
        );

        backend
            .create_or_open(&Path::new(b"/f"), disposition)
            .expect("truncating disposition");
        workspace.close().expect("close");
        id
    };

    // **Reopen**: the answer must come from the overlay, not from anything still alive in the first session.
    let workspace = fixture.repo.workspace_open("w1").expect("reopen");
    let backend = ApiBackend::new(&workspace);
    let after = backend
        .get_info(id)
        .expect("info after reopen")
        .attributes
        .0;
    workspace.close().expect("close");
    after
}

/// SUPERSEDE replaces the stored attributes; the replacement survives a reopen.
#[test]
fn supersede_replaces_attributes_on_disk() {
    common::strict_gate();
    // `0x01 | 0x20` = READONLY | ARCHIVE, both storable bits.
    let after = attributes_after_reopen("supersede", Disposition::Supersede, 0x01 | 0x20);
    // `get_info` reports **derived** attributes (G3's ruling): a file with nothing stored is `NORMAL` (0x80), not 0.
    // So "replaced" here means "back to the ordinary-file value", not "zero" — the stored bits are what the replace
    // clears, and `NORMAL` is synthesised on the way out.
    assert_eq!(
        after, 0x80,
        "cell supersede: `ReplaceFileAttributes = TRUE` — the stored bits are gone (only the derived NORMAL remains), and          that is visible **after reopen**"
    );
}

/// OVERWRITE / OVERWRITE_IF keep the stored attributes; keeping them survives a reopen too.
#[test]
fn overwrite_keeps_attributes_on_disk() {
    common::strict_gate();
    for disposition in [Disposition::Overwrite, Disposition::OverwriteIf] {
        let after = attributes_after_reopen("overwrite", disposition, 0x01 | 0x20);
        assert_eq!(
            after,
            0x01 | 0x20,
            "cell {disposition:?}: `ReplaceFileAttributes = FALSE` — content dropped, metadata kept, still there after \
             reopen"
        );
    }
}
