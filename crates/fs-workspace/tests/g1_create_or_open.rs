//! G1's eighteen cells, over a real workspace backend (P3-2 §1).
//!
//! The table (`fs_semantics::disposition`) says *what* happens; this file asserts the **third column**: what the call
//! leaves behind — the action, the node's kind, and that the returned `FileId` is the entry the name now carries.

use std::sync::Arc;

use fs_namespace::{CreateOutcome, FileId, Kind, Path, Rules};
use fs_semantics::disposition::{decide, Action, Disposition, Exists, Outcome};
use fs_workspace::Repo;

struct Fixture {
    dir: std::path::PathBuf,
    /// Kept alive for the fixture's lifetime: `WorkspaceStore` holds `Arc`s into it, and dropping it early would
    /// leave the store half-wired rather than failing loudly.
    _repo: Arc<Repo>,
    workspace: fs_workspace::Workspace,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(label: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "fs-workspace-g1-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let repo = Arc::new(Repo::open(dir.join("store"), true).expect("repo"));
    repo.init().expect("init");
    let workspace = repo.workspace_create("w1", "main").expect("workspace");
    Fixture {
        dir,
        _repo: repo,
        workspace,
    }
}

fn p(text: &str) -> Path {
    Path::parse(text).expect("path")
}

/// The cells that can be seeded without a second inode: absent, file, directory.
fn seed(
    workspace: &fs_workspace::Workspace,
    rules: &Rules<fs_workspace::WorkspaceStore>,
    exists: Exists,
) {
    match exists {
        Exists::Absent => {}
        Exists::File => {
            let seeded = rules
                .create_or_open(&p("/target"), Disposition::Create)
                .expect("seed file");
            // **Non-empty** on purpose: a zero-length seed cannot tell "truncated" from "never written".
            let _ = seeded;
            workspace
                .write_at("/target", 0, b"seeded bytes")
                .expect("seed content");
            workspace.truncate("/target", 12).expect("seed size");
        }
        Exists::Directory => rules.mkdir(&p("/target")).expect("seed dir"),
    }
}

#[test]
fn every_cell_has_an_action_a_kind_and_a_file_id() {
    let mut cells = 0usize;
    for disposition in Disposition::ALL {
        for exists in [Exists::Absent, Exists::File, Exists::Directory] {
            cells += 1;
            let fixture = fixture(&format!("{}-{}", disposition as u8, cells));
            let rules = Rules::new(fixture.workspace.store());
            seed(&fixture.workspace, &rules, exists);
            let before: Option<FileId> = rules.resolve(&p("/target")).ok();

            match (decide(disposition, exists), rules.create_or_open(&p("/target"), disposition)) {
                (Outcome::Failed(_), Err(_)) => {
                    assert_eq!(
                        before,
                        rules.resolve(&p("/target")).ok(),
                        "cell disposition={disposition:?} exists={exists:?}: a refusal changed the tree"
                    );
                }
                (Outcome::Failed(failure), Ok(outcome)) => panic!(
                    "cell disposition={disposition:?} exists={exists:?}: expected {failure:?}, got {outcome:?}"
                ),
                (Outcome::Opened(action), Err(error)) => panic!(
                    "cell disposition={disposition:?} exists={exists:?}: expected {action:?}, got {error:?}"
                ),
                (Outcome::Opened(action), Ok(CreateOutcome { file_id, kind, action: got })) => {
                    assert_eq!(
                        got, action,
                        "cell disposition={disposition:?} exists={exists:?}: action"
                    );
                    let expected_kind = if exists == Exists::Directory {
                        Kind::Directory
                    } else {
                        Kind::File
                    };
                    assert_eq!(
                        kind, expected_kind,
                        "cell disposition={disposition:?} exists={exists:?}: kind"
                    );
                    assert_eq!(
                        file_id,
                        rules.resolve(&p("/target")).expect("the name resolves"),
                        "cell disposition={disposition:?} exists={exists:?}: the outcome must name the entry the name carries"
                    );
                    // The truncating dispositions must leave an **empty** file, and must keep the same entry
                    // (that pair is the whole observable difference from SUPERSEDE).
                    if action == Action::TruncateExisting {
                        assert_eq!(
                            Some(file_id),
                            before,
                            "cell disposition={disposition:?} exists={exists:?}: overwriting reuses the entry"
                        );
                        assert_eq!(
                            fixture.workspace.content_size("/target").expect("size"),
                            0,
                            "cell disposition={disposition:?} exists={exists:?}: the file must be truncated to zero"
                        );
                    }
                    if action == Action::ReplaceExisting {
                        assert_ne!(
                            Some(file_id),
                            before,
                            "cell disposition=Supersede exists={exists:?}: replacing means a new entry"
                        );
                    }
                }
            }
            drop(rules);
            drop(fixture);
        }
    }
    assert_eq!(cells, 18, "cell count: 6 dispositions × 3 states");
}
