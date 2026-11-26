//! Instantiates every `fs-winfsp` backend contract scenario against `WinFspBackend`
//! (backed by upstream LayerFS).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use fs_layerfs::{PersistenceProfile, Repo, WinFspBackend, WorkspaceHandle, DEFAULT_BRANCH, DEFAULT_VOLUME_SIZE_BYTES};

static COUNTER: AtomicU64 = AtomicU64::new(1);

struct Harness {
    dir: PathBuf,
    ws: Option<WorkspaceHandle>,
    _repo: Repo,
}

impl Harness {
    fn new(label: &str) -> Self {
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "fs-layerfs-contract-{label}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let repo = Repo::init(
            dir.join("repo"),
            PersistenceProfile::Disposable,
            DEFAULT_VOLUME_SIZE_BYTES,
        )
        .expect("init repo");
        let ws = repo.open_workspace(DEFAULT_BRANCH).expect("open work");
        Self {
            dir,
            ws: Some(ws),
            _repo: repo,
        }
    }

    fn backend(&self) -> WinFspBackend {
        self.ws.as_ref().expect("ws").backend()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(ws) = self.ws.take() {
            let _ = ws.close();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn winfsp_backend_satisfies_every_contract_scenario() {
    let h = Harness::new("rename");
    fs_winfsp::scenarios::scenario_identity_across_rename::<WinFspBackend>(&h.backend());

    let h = Harness::new("supersede");
    fs_winfsp::scenarios::scenario_supersede_truncates_in_place::<WinFspBackend>(&h.backend());

    let h = Harness::new("removal");
    fs_winfsp::scenarios::scenario_removal_cases::<WinFspBackend>(&h.backend());

    let h = Harness::new("readdir");
    fs_winfsp::scenarios::scenario_read_dir_by_id::<WinFspBackend>(&h.backend());

    let h = Harness::new("nonempty");
    fs_winfsp::scenarios::scenario_remove_non_empty_directory::<WinFspBackend>(&h.backend());

    let h = Harness::new("case");
    fs_winfsp::scenarios::scenario_case_insensitive_names::<WinFspBackend>(&h.backend());
}
