//! G3's "reading does not write" criteria (P3-2 card §3, time/attribute semantics).
//!
//! Two claims, and the second is the one that catches a real class of bug:
//!
//! 1. `security(id)` is a **separate** accessor from `stat` — a descriptor can be 64 KiB and `GetFileInformation` is far
//!    more frequent than `GetSecurityInfo`, so folding it into `Stat` would pay that cost on every stat;
//! 2. **a read does not update `accessed_ns`**, tested in the strong form: read twice and require the **root id to be
//!    unchanged**. "the field did not change" would pass for an implementation that touches the inode and writes the same
//!    value back; "the root is the same object" can only hold if no write path ran at all.

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
    _repo: Arc<Repo>,
    workspace: fs_workspace::Workspace,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(label: &str) -> Fixture {
    let path = std::env::temp_dir().join(format!(
        "fs-workspace-g3-stat-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&path).expect("temp dir");
    let repo = Arc::new(Repo::open(path.join("store"), true).expect("repo"));
    repo.init().expect("init");
    let workspace = repo.workspace_create("w1", "main").expect("workspace");
    Fixture {
        dir: path,
        _repo: repo,
        workspace,
    }
}

fn committed_file(fixture: &mut Fixture, name: &str, content: &[u8]) -> FileId {
    let rules = Rules::new(fixture.workspace.store());
    let created = rules
        .create_or_open(&p(name), Disposition::Create)
        .expect("create");
    fixture.workspace.write_at(name, 0, content).expect("write");
    fixture.workspace.commit("seed", false).expect("commit");
    created.file_id
}

/// Reading twice must not change the **committed root**: if a read updated `accessed_ns`, the root object would be
/// rewritten and its id would differ.
#[test]
fn reading_twice_does_not_change_the_root_id() {
    common::strict_gate();
    let mut fixture = fixture("accessed");
    let id = committed_file(&mut fixture, "/f", b"payload");
    let before = fixture.workspace.base_root();

    let first = fixture.workspace.read_at_id(id, 0, 7).expect("read");
    let middle = fixture.workspace.base_root();
    let second = fixture.workspace.read_at_id(id, 0, 7).expect("read again");
    let after = fixture.workspace.base_root();

    assert_eq!(
        first, second,
        "cell content: both reads return the same bytes"
    );
    assert_eq!(
        (middle, after),
        (before, before),
        "cell root: a read must not rewrite the root (which is what updating `accessed_ns` would do)"
    );
}

/// The descriptor accessor is separate from `Stat` and reports "no explicit SD" for a v2 overlay.
///
/// `None` here is a statement about this store, not an unfinished feature: v2 has no column for it.
#[test]
fn security_is_a_separate_accessor_and_v2_has_none() {
    common::strict_gate();
    let mut fixture = fixture("security");
    let id = committed_file(&mut fixture, "/f", b"payload");
    assert_eq!(
        fixture.workspace.security(id).expect("security"),
        None,
        "cell v2: no explicit descriptor — inherit the default, which is not the same as an empty descriptor"
    );

    // Control: the call reaches the entry rather than failing for an unrelated reason — the inode is readable.
    assert_eq!(
        fixture.workspace.read_at_id(id, 0, 7).expect("read"),
        b"payload",
        "cell control: the same id is a live, readable entry"
    );
}
