//! G2's read side: `read_at_id` and `read_dir(dir_id)` (P3-2 §2).
//!
//! The point of both is the same: a handle is identified by **FileId**, so a rename — which changes every path below it
//! — must not change what the caller sees. Each test states the before/after pair rather than only the after state,
//! because "still readable" says nothing without "was readable, with these bytes".

use std::sync::Arc;

use fs_namespace::{FileId, Path, Rules};
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
    let path = std::env::temp_dir().join(format!(
        "fs-workspace-g2-{label}-{}-{}",
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

fn p(text: &str) -> Path {
    Path::parse(text).expect("path")
}

/// A file read by FileId keeps its bytes across a rename of its parent **and** of itself.
#[test]
fn a_file_reads_by_file_id_after_both_kinds_of_rename() {
    let fixture = fixture("rename-file");
    let rules = Rules::new(fixture.workspace.store());
    rules.mkdir(&p("/d")).expect("mkdir");
    let created = rules
        .create_or_open(&p("/d/f"), fs_semantics::disposition::Disposition::Create)
        .expect("create");
    fixture
        .workspace
        .write_at("/d/f", 0, b"payload that must survive")
        .expect("write");

    let before = fixture
        .workspace
        .read_at_id(created.file_id, 0, 25)
        .expect("read by id");
    assert_eq!(before, b"payload that must survive", "cell before rename");

    rules
        .rename(&p("/d"), &p("/e"))
        .expect("rename the directory");
    rules
        .rename(&p("/e/f"), &p("/e/g"))
        .expect("rename the file");

    assert_eq!(
        fixture
            .workspace
            .read_at_id(created.file_id, 0, 25)
            .expect("read by id after rename"),
        before,
        "cell after rename: the same FileId must return the same bytes"
    );
    // And the old path is gone, so the read above cannot have been a path lookup in disguise.
    assert!(
        fixture.workspace.read_at("/d/f", 0, 24).is_err(),
        "cell control: the old path must no longer resolve"
    );
}

/// A directory listed by FileId keeps its entries, and the order is stable across calls.
#[test]
fn a_directory_lists_by_file_id_after_a_rename_with_a_stable_order() {
    let fixture = fixture("rename-dir");
    let rules = Rules::new(fixture.workspace.store());
    rules.mkdir(&p("/d")).expect("mkdir");
    for name in ["alpha", "beta", "gamma"] {
        rules
            .create_or_open(
                &p(&format!("/d/{name}")),
                fs_semantics::disposition::Disposition::Create,
            )
            .expect("create");
    }
    let dir_id = rules.resolve(&p("/d")).expect("dir id");

    let names = |entries: Vec<(Vec<u8>, FileId)>| -> Vec<String> {
        entries
            .into_iter()
            .map(|(name, _)| String::from_utf8(name).expect("utf8"))
            .collect()
    };
    let first = names(
        fixture
            .workspace
            .read_dir_id(dir_id, None, 16)
            .expect("list"),
    );
    assert_eq!(first, vec!["alpha", "beta", "gamma"], "cell before rename");
    let second = names(
        fixture
            .workspace
            .read_dir_id(dir_id, None, 16)
            .expect("list"),
    );
    assert_eq!(first, second, "cell stable order: two calls, same sequence");

    rules.rename(&p("/d"), &p("/moved")).expect("rename");
    assert_eq!(
        names(
            fixture
                .workspace
                .read_dir_id(dir_id, None, 16)
                .expect("list after rename")
        ),
        first,
        "cell after rename: the same dir_id must enumerate the same entries"
    );
}

/// Listing a removed directory is a typed error, not a panic and not an empty listing.
#[test]
fn listing_a_removed_directory_is_a_typed_error() {
    let fixture = fixture("removed-dir");
    let rules = Rules::new(fixture.workspace.store());
    rules.mkdir(&p("/gone")).expect("mkdir");
    let dir_id = rules.resolve(&p("/gone")).expect("dir id");
    rules.rmdir(&p("/gone")).expect("rmdir");

    match fixture.workspace.read_dir_id(dir_id, None, 16) {
        Ok(entries) => panic!("cell after remove: expected a typed error, got {entries:?}"),
        Err(error) => {
            let text = format!("{error:?}");
            assert!(
                text.contains("NotFound") || text.contains("NotADirectory"),
                "cell after remove: expected NotFound/NotADirectory, got {text}"
            );
        }
    }
}

/// Reading a directory's content by FileId is `IsADirectory`, not an empty byte string.
#[test]
fn reading_a_directory_by_file_id_is_refused() {
    let fixture = fixture("read-dir");
    let rules = Rules::new(fixture.workspace.store());
    rules.mkdir(&p("/d")).expect("mkdir");
    let dir_id = rules.resolve(&p("/d")).expect("dir id");
    assert!(
        fixture.workspace.read_at_id(dir_id, 0, 16).is_err(),
        "cell reading a directory: must be refused"
    );
    // Control: a file id does read, so the refusal above is about the kind, not about the id form.
    let file = rules
        .create_or_open(&p("/f"), fs_semantics::disposition::Disposition::Create)
        .expect("create");
    fixture.workspace.write_at("/f", 0, b"abc").expect("write");
    assert_eq!(
        fixture
            .workspace
            .read_at_id(file.file_id, 0, 3)
            .expect("read"),
        b"abc"
    );
}
