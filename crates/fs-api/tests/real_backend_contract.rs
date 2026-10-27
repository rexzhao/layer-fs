//! The P3-3a scenarios, instantiated against the **real** backend (P3-3b).
//!
//! The scenario functions live in `fs-winfsp/tests/contract.rs` and are generic over `WorkspaceOps`, so they are not
//! re-written here — that is the whole point of the P3-3a shape. This file only supplies the workspace and the second
//! instantiation.
//!
//! The real backend is exercised through the same calls the adapter makes, which is what makes "the mock honours the
//! contract" and "the backend honours the contract" two facts rather than one assumption.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_api::winfsp_backend::ApiBackend;
use fs_workspace::Repo;

struct Fixture {
    dir: PathBuf,
    _repo: Arc<Repo>,
    workspace: fs_workspace::Workspace,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Fixture {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fs-api-real-backend-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let repo = Arc::new(Repo::open(Path::new(&dir).join("store"), true).expect("repo"));
        repo.init().expect("init");
        let workspace = repo.workspace_create("w1", "main").expect("workspace");
        Self {
            dir,
            _repo: repo,
            workspace,
        }
    }

    fn backend(&self) -> ApiBackend<'_> {
        ApiBackend::new(&self.workspace)
    }
}

/// The real backend satisfies the identity-across-rename scenario (P3-3a's `scenario_identity_across_rename`).
#[test]
fn the_real_backend_satisfies_identity_across_rename() {
    let fixture = Fixture::new("identity");
    let backend = fixture.backend();
    fs_winfsp::scenarios::scenario_identity_across_rename::<ApiBackend<'_>>(&backend);
}

/// The real backend satisfies the "SUPERSEDE truncates in place" scenario.
#[test]
fn the_real_backend_satisfies_supersede_in_place() {
    let fixture = Fixture::new("supersede");
    let backend = fixture.backend();
    fs_winfsp::scenarios::scenario_supersede_truncates_in_place::<ApiBackend<'_>>(&backend);
}

/// The real backend satisfies the three removal cases (the core half of criterion 3 — see the scenario's docs).
#[test]
fn the_real_backend_satisfies_removal_cases() {
    let fixture = Fixture::new("removal");
    let backend = fixture.backend();
    fs_winfsp::scenarios::scenario_removal_cases::<ApiBackend<'_>>(&backend);
}

/// The real backend enumerates a directory by id after another directory took its old path.
#[test]
fn the_real_backend_satisfies_read_dir_by_id() {
    let fixture = Fixture::new("readdir");
    let backend = fixture.backend();
    fs_winfsp::scenarios::scenario_read_dir_by_id::<ApiBackend<'_>>(&backend);
}

/// **Criterion 3, the core half**, on the real backend: the three removal cases are about identity, not about handles.
///
/// The adapter half (`DeletePending` for a new open while old handles still work) is asserted in `fs-winfsp` against the
/// mock's handle table and will be re-asserted against the real one in P3-4, when the FFI layer brings that table. The
/// split is documented on `fs_winfsp::scenarios`, so its absence here is a scope statement rather than a gap.
#[test]
fn criterion_3_core_half_holds_on_the_real_backend() {
    use fs_semantics::disposition::Disposition;
    use fs_winfsp::{OpsError, WorkspaceOps, ROOT};

    let fixture = Fixture::new("criterion3");
    let backend = fixture.backend();
    let left = backend
        .create_or_open(b"/left", Disposition::Create)
        .expect("create left")
        .file_id;
    let right = backend
        .create_or_open(b"/right", Disposition::Create)
        .expect("create right")
        .file_id;

    // (1) the name belongs to another inode ⇒ a **stale identity**, not a missing entry.
    match backend.remove_at(ROOT, b"left", right) {
        Err(OpsError::InvalidRecord(_)) => {}
        other => panic!("cell 1: expected the stale-identity error, got {other:?}"),
    }
    // (2) removing it for real, then again: the second call is an idempotent success.
    backend
        .remove_at(ROOT, b"left", left)
        .expect("first removal");
    backend
        .remove_at(ROOT, b"left", left)
        .expect("cell 2: a repeated removal of a retired id is an idempotent success");
    // (3) the name is free but the id is still live (it was renamed away) ⇒ `NotFound`.
    backend
        .rename(b"/right", b"/moved", false)
        .expect("rename right");
    match backend.remove_at(ROOT, b"right", right) {
        Err(OpsError::NotFound) => {}
        other => panic!("cell 3: expected NotFound for a free name with a live id, got {other:?}"),
    }
}

/// A non-empty directory cannot be removed — and the refusal takes nothing with it (L2 class 06).
#[test]
fn the_real_backend_refuses_to_remove_a_non_empty_directory() {
    let fixture = Fixture::new("nonempty");
    let backend = fixture.backend();
    fs_winfsp::scenarios::scenario_remove_non_empty_directory::<ApiBackend<'_>>(&backend);
}
