//! The GC root seam: what the collector is told it must keep (P2b-6 §2.1).
//!
//! These tests are about the *enumeration*, not the collector. They pin the property the G1–G3
//! injections depend on: the root set is a value produced by a replaceable source, deduplicated
//! **by fact** (label), and comparable as a set — so "a row was dropped" and "a root was skipped"
//! are observable differences, not silent behaviour changes.
//!
//! They are deliberately runnable now, before mark/sweep exist: if the seam is wrong, later
//! injections cannot be built on it, and no amount of green GC code fixes that.

use cas_core::{Object, ObjectId, Role};
use fs_workspace::gc::{RepoRoots, RootKind, RootRef, RootSet, RootSource};
use fs_workspace::Repo;
use std::path::PathBuf;

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "fs-workspace-gcroots-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }

    fn store(&self) -> PathBuf {
        self.path.join("store")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn oid(label: &str) -> ObjectId {
    Object::new(Role::Bytes, label.as_bytes())
        .expect("framed")
        .id()
        .expect("hashed")
}

/// A stand-in source: the negative cases (C/D/F) are "this list is wrong on purpose".
struct Fixed(Vec<RootRef>);

impl RootSource for Fixed {
    fn roots(&self) -> fs_workspace::WorkspaceResult<RootSet> {
        Ok(RootSet::from_entries(self.0.clone()))
    }
}

#[test]
fn the_root_set_deduplicates_by_fact_not_by_object_id() {
    // Two rows can protect the same tree: a commit and a snapshot of it. Both are facts, and G2
    // compares facts, so both survive — while the *ids* the sweep walks are deduplicated.
    let tree = oid("tree");
    let set = RootSet::from_entries(vec![
        RootRef::new(RootKind::Commit, tree, "commit:c1"),
        RootRef::new(RootKind::Snapshot, tree, "snapshot:s1"),
    ]);
    assert_eq!(set.entries().len(), 2, "two rows, two facts");
    assert_eq!(set.ids(), vec![tree], "one object to walk");

    let duplicates = RootSet::from_entries(vec![
        RootRef::new(RootKind::Snapshot, tree, "snapshot:s1"),
        RootRef::new(RootKind::Snapshot, tree, "snapshot:s1"),
    ]);
    assert_eq!(
        duplicates.entries().len(),
        1,
        "the same fact twice is one fact"
    );
}

#[test]
fn dropping_a_row_is_visible_as_a_set_difference() {
    // The G1 injection in miniature: a snapshot row disappears from the enumeration while the
    // objects it protects are still reachable from it. Comparing facts, the difference names the
    // missing row instead of just "an id changed".
    let tree = oid("tree");
    let complete = RootSet::from_entries(vec![
        RootRef::new(RootKind::Commit, tree, "commit:c1"),
        RootRef::new(RootKind::Snapshot, tree, "snapshot:s1"),
    ]);
    let injected = RootSet::from_entries(vec![RootRef::new(RootKind::Commit, tree, "commit:c1")]);

    let missing: Vec<&RootRef> = complete
        .entries()
        .iter()
        .filter(|entry| !injected.entries().contains(entry))
        .collect();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].kind, RootKind::Snapshot);
    assert_eq!(
        missing[0].label, "snapshot:s1",
        "the diff names what vanished"
    );
    assert_eq!(
        injected.ids(),
        complete.ids(),
        "the id set alone cannot see this — which is why G2 compares facts"
    );
}

#[test]
fn a_replaceable_source_is_what_the_collector_consumes() {
    // The seam itself: production reads the repository, a test supplies a lie, and both are just
    // `RootSource`. Nothing in the collector may query the database for roots directly.
    let root = TempRoot::new("seam");
    let repo = Repo::open(root.store(), true).expect("open");
    repo.init().expect("init");

    let production = RepoRoots { repo: &repo };
    let from_repo = production.roots().expect("roots");
    assert!(
        !from_repo.is_empty(),
        "an initialised repository has at least one root"
    );

    let lie = oid("not-in-the-repo");
    let injected = Fixed(vec![RootRef::new(RootKind::Commit, lie, "commit:fake")]);
    let from_test = injected.roots().expect("roots");
    assert_eq!(from_test.ids(), vec![lie]);
    assert_ne!(
        from_test, from_repo,
        "the two sources disagree, which is exactly what an injection needs"
    );
}
