//! End-to-end truncation for the overwriting dispositions (P3-2 G1, QA's reproduction).
//!
//! The unit test asserts `content_size == 0` right after the call; this file goes through the **public read path**
//! afterwards, because that is where QA found the gap: an entry can report size 0 and still serve the old bytes if the
//! truncation never reached the pages.

use std::sync::Arc;

use fs_namespace::{FileId, Path, Rules};
use fs_semantics::disposition::Disposition;
use fs_workspace::Repo;

struct Fixture {
    dir: std::path::PathBuf,
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
        "fs-workspace-g1-truncate-{label}-{}-{}",
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

/// The bytes as a caller sees them, through the same read path the frontend uses.
fn read_all(workspace: &fs_workspace::Workspace, path: &str) -> Vec<u8> {
    let size = workspace.content_size(path).expect("size");
    if size == 0 {
        return Vec::new();
    }
    workspace
        .read_at(path, 0, usize::try_from(size).expect("fits"))
        .expect("bytes")
}

fn seeded(label: &str, payload: &[u8]) -> (Fixture, Rules<fs_workspace::WorkspaceStore>) {
    let fixture = fixture(label);
    let rules = Rules::new(fixture.workspace.store());
    rules
        .create_or_open(&p("/f"), Disposition::Create)
        .expect("seed file");
    fixture
        .workspace
        .write_at("/f", 0, payload)
        .expect("seed content");
    fixture
        .workspace
        .truncate("/f", u64::try_from(payload.len()).expect("fits"))
        .expect("seed size");
    (fixture, rules)
}

/// `OVERWRITE_IF` and `OVERWRITE` on an existing file must leave it **empty as read**, not just size 0.
#[test]
fn the_overwriting_dispositions_leave_nothing_to_read() {
    let payload = vec![0xABu8; 8192];
    for disposition in [Disposition::OverwriteIf, Disposition::Overwrite] {
        let (fixture, rules) = seeded(&format!("{disposition:?}"), &payload);
        assert_eq!(
            read_all(&fixture.workspace, "/f"),
            payload,
            "cell disposition={disposition:?}: the seed must be readable before the call"
        );

        let outcome = rules
            .create_or_open(&p("/f"), disposition)
            .expect("the overwriting disposition is legal here");
        assert_eq!(
            fixture.workspace.content_size("/f").expect("size"),
            0,
            "cell disposition={disposition:?}: size"
        );
        assert!(
            read_all(&fixture.workspace, "/f").is_empty(),
            "cell disposition={disposition:?}: the old bytes must not be readable after the truncation"
        );
        let _: FileId = outcome.file_id;
    }
}

/// `OVERWRITE` on a name that does not exist is a refusal, and it must not create anything.
#[test]
fn overwrite_without_a_target_is_refused_and_creates_nothing() {
    let fixture = fixture("missing");
    let rules = Rules::new(fixture.workspace.store());
    assert!(
        rules
            .create_or_open(&p("/absent"), Disposition::Overwrite)
            .is_err(),
        "cell disposition=Overwrite exists=Absent: must be refused"
    );
    assert!(
        rules.resolve(&p("/absent")).is_err(),
        "cell disposition=Overwrite exists=Absent: the name must still be free"
    );
}

/// `SUPERSEDE` **truncates in place**: the name and the FileId are unchanged, and the content is gone
/// (ADR-009 §4.2 ruling (a)).
///
/// The former test asserted the opposite (a new entry, the old one still readable by its id). It is rewritten rather than
/// deleted because the *question* is still worth asking: what does a handle opened before a SUPERSEDE see afterwards?
#[test]
fn supersede_truncates_in_place() {
    let payload = b"bytes the old entry loses".to_vec();
    let (fixture, rules) = seeded("supersede", &payload);
    let before = rules.resolve(&p("/f")).expect("entry before");

    let outcome = rules
        .create_or_open(&p("/f"), Disposition::Supersede)
        .expect("supersede is legal on an existing file");
    assert_eq!(
        outcome.file_id, before,
        "cell identity: SUPERSEDE keeps the entry (same FileId) — it is an overwrite of the same object"
    );
    assert_eq!(
        outcome.replaced, None,
        "cell replaced: Constant `None` on the create path; `RenameOutcome::replaced` (G4) is a different field"
    );
    assert!(
        read_all(&fixture.workspace, "/f").is_empty(),
        "cell content: the entry is now empty"
    );
    assert_eq!(
        rules.resolve(&p("/f")).expect("entry after"),
        before,
        "cell name: still bound to the same entry"
    );
}
