//! `write_at_id` / `truncate_id`: the write side of "a handle names an object, not a path" (P3-3b, process 254).
//!
//! The read side got this in G2 and the write side did not; without it the real `WorkspaceOps` backend has no honest way
//! to implement `write(file_id, …)`.
//!
//! Both tests are **rename-shaped** on purpose: the failure mode being prevented is "the handle's name moved, so the
//! write landed on whatever is at the old path now". A test that only wrote by id without a rename in between would pass
//! for an implementation that resolved the path internally.

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
        "fs-workspace-g2write-{label}-{}-{}",
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

fn seeded(fixture: &Fixture, name: &str, content: &[u8]) -> FileId {
    let rules = Rules::new(fixture.workspace.store());
    let created = rules
        .create_or_open(&p(name), Disposition::Create)
        .expect("create");
    fixture.workspace.write_at(name, 0, content).expect("write");
    created.file_id
}

/// A write by id lands on **that object**, even after its name moved — and the old path is not written to.
#[test]
fn a_write_by_id_follows_the_object_across_a_rename() {
    common::strict_gate();
    let fixture = fixture("write");
    let id = seeded(&fixture, "/a", b"original");
    let rules = Rules::new(fixture.workspace.store());
    rules.rename(&p("/a"), &p("/b")).expect("rename");

    // A new object takes the old path: this is the §3.9 shape — a path-based write would hit *this* one.
    let intruder = rules
        .create_or_open(&p("/a"), Disposition::Create)
        .expect("create")
        .file_id;
    assert_ne!(
        intruder, id,
        "cell setup: the path now names a different object"
    );

    fixture
        .workspace
        .write_at_id(id, 0, b"rewritten")
        .expect("write by id");

    assert_eq!(
        fixture.workspace.read_at_id(id, 0, 9).expect("read by id"),
        b"rewritten",
        "cell target: the write landed on the object the id names"
    );
    assert_eq!(
        fixture.workspace.read_at("/b", 0, 9).expect("read /b"),
        b"rewritten",
        "cell renamed path: the object's new name sees the new bytes"
    );
    assert_eq!(
        fixture.workspace.read_at("/a", 0, 9).expect("read /a"),
        vec![0u8; 9],
        "cell old path: the object that reused the path was **not** written to"
    );
}

/// `truncate_id` follows the object too: the intruder at the old path keeps its size.
#[test]
fn a_truncate_by_id_follows_the_object_across_a_rename() {
    common::strict_gate();
    let fixture = fixture("truncate");
    let id = seeded(&fixture, "/a", b"original");
    let rules = Rules::new(fixture.workspace.store());
    rules.rename(&p("/a"), &p("/b")).expect("rename");
    let intruder = rules
        .create_or_open(&p("/a"), Disposition::Create)
        .expect("create")
        .file_id;
    fixture
        .workspace
        .write_at("/a", 0, b"keep me")
        .expect("give the intruder content");

    fixture
        .workspace
        .truncate_id(id, 2)
        .expect("truncate by id");

    assert_eq!(
        fixture.workspace.read_at_id(id, 0, 2).expect("read"),
        b"or",
        "cell target: the object the id names was truncated to its first two bytes"
    );
    assert_eq!(
        fixture.workspace.content_size("/a").expect("size"),
        7,
        "cell old path: the object that reused the path kept its size"
    );
    assert_eq!(
        fixture.workspace.read_at_id(intruder, 0, 7).expect("read"),
        b"keep me",
        "cell old path content: unchanged"
    );
}
