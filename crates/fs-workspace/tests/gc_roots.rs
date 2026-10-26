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
use std::path::{Path, PathBuf};

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
    assert_eq!(set.by_root(), vec![tree], "one object to walk");

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
        injected.by_root(),
        complete.by_root(),
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
    assert_eq!(from_test.by_root(), vec![lie]);
    assert_ne!(
        from_test, from_repo,
        "the two sources disagree, which is exactly what an injection needs"
    );
}

// --------------------------------------------------------------- criteria seam (G1–G5, C/D/E/F/G)
//
// The four counter-examples all need to *lie at one seam* and then assert that exactly one
// criterion goes red. Each test here therefore runs a clean mark and an injected one on the same
// fixture — the clean run is the positive control QA requires (a red assertion in a seam that
// never fires proves nothing).

use fs_workspace::gc::criteria;

/// Declares two roots, walks both: the positive control for every injection below.
fn clean_pair() -> (RootSet, RootSet) {
    let commit = oid("commit-tree");
    let snap = oid("snapshot-tree");
    let declared = RootSet::from_entries(vec![
        RootRef::new(RootKind::Commit, commit, "commit:c1"),
        RootRef::new(RootKind::Snapshot, snap, "snapshot:s1"),
    ]);
    (declared.clone(), declared)
}

#[test]
fn a_clean_mark_passes_g1_g2_g3_and_fails_only_g5_for_lack_of_collection() {
    let (declared, walked) = clean_pair();
    let mut report = fs_workspace::gc::GcReport::skeleton(&declared);
    for entry in walked.entries() {
        report.record_visit(entry);
        report.reachable.push(entry.root);
    }
    report.reachable_count = report.reachable.len();

    assert!(criteria::g1_no_live_object_freed(&report).is_ok());
    assert!(criteria::g2_mark_covers_declared(&report).is_ok());
    assert!(criteria::g3_every_root_visited(&report).is_ok());
    // G5 is the non-vacuous criterion: nothing was freed, so the run proves nothing yet.
    assert!(criteria::g5_not_vacuous(&report).is_err());
}

#[test]
fn skipping_one_root_is_caught_by_g2_and_g3() {
    // Counter-example D: the source omits a root, so mark never visits it.
    let (declared, walked) = clean_pair();
    let mut report = fs_workspace::gc::GcReport::skeleton(&declared);
    for entry in &walked.entries()[..1] {
        report.record_visit(entry);
    }
    let (missed, extra) = criteria::g2_mark_covers_declared(&report).expect_err("G2 must go red");
    assert_eq!(missed, vec!["snapshot:s1".to_owned()]);
    assert!(extra.is_empty());
    assert!(
        criteria::g3_every_root_visited(&report).is_err(),
        "G3 must go red"
    );
}

#[test]
fn visiting_a_root_without_counting_it_is_caught_by_g3_per_kind() {
    // Counter-example F: the label is in the visited list but the per-kind counter never moved —
    // the shape a UNION query invites when the per-kind dispatch forgets a source.
    let (declared, walked) = clean_pair();
    let report = fs_workspace::gc::GcReport {
        roots_declared: declared.entries().iter().map(|e| e.label.clone()).collect(),
        roots_visited: declared.entries().iter().map(|e| e.label.clone()).collect(),
        root_oids: declared.by_root(),
        visited_oids: declared.by_root(),
        roots_by_kind: [(RootKind::Snapshot, 1), (RootKind::Commit, 1)].into(),
        visited_by_kind: [(RootKind::Commit, 1)].into(),
        ..Default::default()
    };
    let error = criteria::g3_every_root_visited(&report).expect_err("G3 must go red");
    assert!(
        error.contains("Snapshot"),
        "the reason names the kind: {error}"
    );
    assert!(
        criteria::g2_mark_covers_declared(&report).is_ok(),
        "G2 alone cannot see this"
    );
    let _ = walked;
}

#[test]
fn forcing_a_reachable_object_into_the_free_set_is_caught_by_g1_and_g4() {
    // Counter-example G: the sweep's seam adds an id that mark reached.
    let (declared, walked) = clean_pair();
    let mut report = fs_workspace::gc::GcReport::skeleton(&declared);
    for entry in walked.entries() {
        report.record_visit(entry);
        report.reachable.push(entry.root);
    }
    let victim = walked.entries()[0].root;
    report.freed.push(victim);

    let offending = criteria::g1_no_live_object_freed(&report).expect_err("G1 must go red");
    assert_eq!(offending, vec![victim]);
    assert!(
        criteria::g4_freed_is_unreachable(&report).is_err(),
        "G4 must go red"
    );
    assert!(
        criteria::g5_not_vacuous(&report).is_ok(),
        "G5 is satisfied — something was freed"
    );
}

#[test]
fn freeing_only_unreachable_objects_satisfies_all_five() {
    // Counter-example E's positive side: a genuinely unreachable object is freed, live ones are not.
    let (declared, walked) = clean_pair();
    let mut report = fs_workspace::gc::GcReport::skeleton(&declared);
    for entry in walked.entries() {
        report.record_visit(entry);
        report.reachable.push(entry.root);
    }
    report.freed.push(oid("garbage"));
    report.reachable_count = report.reachable.len();

    assert!(criteria::g1_no_live_object_freed(&report).is_ok());
    assert!(criteria::g2_mark_covers_declared(&report).is_ok());
    assert!(criteria::g3_every_root_visited(&report).is_ok());
    assert!(criteria::g4_freed_is_unreachable(&report).is_ok());
    assert!(criteria::g5_not_vacuous(&report).is_ok());
}

// -------------------------------------------- §8: independent ground truth + the three-step template
//
// The ground truth is built here, by hand, from what the test itself created ("I declared these
// two roots"), never read back from the implementation. An injected run must then diff against
// `R_true`, not against a shrunken copy of itself — otherwise counter-example C stays green
// (proving the subject with the subject).

use std::cell::Cell;

/// A source whose omission of one root can be switched on and off, so the same fixture can run
/// clean, injected, and clean again.
struct Toggle {
    roots: Vec<RootRef>,
    omit: Cell<bool>,
}

impl RootSource for Toggle {
    fn roots(&self) -> fs_workspace::WorkspaceResult<RootSet> {
        let entries = if self.omit.get() {
            self.roots.iter().skip(1).cloned().collect()
        } else {
            self.roots.clone()
        };
        Ok(RootSet::from_entries(entries))
    }
}

/// The three-step template (card §8.3): clean → injected (only the named criterion red) → clean.
#[test]
fn the_injection_seam_is_consumed_and_leaves_no_residue() {
    // Ground truth, held by the test: two roots, plus one object that must be collected.
    let commit = oid("ground-commit");
    let snap = oid("ground-snapshot");
    let garbage = oid("ground-garbage");
    let truth = RootSet::from_entries(vec![
        RootRef::new(RootKind::Commit, commit, "commit:c1"),
        RootRef::new(RootKind::Snapshot, snap, "snapshot:s1"),
    ]);
    let source = Toggle {
        roots: truth.entries().to_vec(),
        omit: Cell::new(false),
    };

    let run = |omit: bool| {
        source.omit.set(omit);
        let mut visited = std::collections::BTreeSet::new();
        let mut report;
        {
            let mut sink = |kind: RootKind, root: ObjectId| {
                visited.insert((kind, root));
            };
            // With `gc-test-hooks` off, `on_root_visited` is the only field; with it on, the
            // defaults cover the seams (no skip, nothing forced), so this stays valid both ways.
            #[allow(clippy::needless_update)]
            let mut hooks = fs_workspace::gc::GcHooks {
                on_root_visited: Some(&mut sink),
                ..Default::default()
            };
            let input = fs_workspace::gc::GcInput {
                declared: &truth,
                mark_source: &source as &dyn RootSource,
            };
            report = input.mark(&mut hooks).expect("mark");
        }
        // The test owns the sink's storage (§8.4): the interface stays streaming.
        assert_eq!(
            visited.len(),
            report.visited_oids.len(),
            "sink saw every visited root"
        );
        report.freed.push(garbage);
        report.roots_declared = truth.entries().iter().map(|e| e.label.clone()).collect();
        report.root_oids = truth.by_root();
        report.reachable_count = report.reachable.len();
        report
    };

    // 1. clean → everything green.
    let clean = run(false);
    assert!(criteria::g1_no_live_object_freed(&clean).is_ok());
    assert!(criteria::g2_mark_covers_declared(&clean).is_ok());
    assert!(criteria::g3_every_root_visited(&clean).is_ok());
    assert!(criteria::g4_freed_is_unreachable(&clean).is_ok());
    assert!(criteria::g5_not_vacuous(&clean).is_ok());

    // 2. injected → only G2/G3 red, the rest still green (no "everything turns red" theatre).
    let injected = run(true);
    assert!(
        criteria::g1_no_live_object_freed(&injected).is_ok(),
        "G1 unaffected by omission here"
    );
    let (missed, _) = criteria::g2_mark_covers_declared(&injected).expect_err("G2 must be red");
    // `skip(1)` drops the first entry, which is `commit:c1` (entries are sorted by kind: Commit <
    // Snapshot). Asserting the actual name keeps this honest — a wrong label here would mean the
    // injection drops a different fact than the test thinks.
    assert_eq!(
        missed,
        vec!["commit:c1".to_owned()],
        "compared against R_true, not against itself"
    );
    assert!(
        criteria::g3_every_root_visited(&injected).is_err(),
        "G3 must be red"
    );
    assert!(criteria::g4_freed_is_unreachable(&injected).is_ok());
    assert!(
        criteria::g5_not_vacuous(&injected).is_ok(),
        "G5 is about the sweep, not the roots"
    );

    // 3. injection off again → all green, so the flag was really consumed and nothing leaked.
    let restored = run(false);
    assert!(
        criteria::g2_mark_covers_declared(&restored).is_ok(),
        "no residue after the injection"
    );
    assert!(criteria::g3_every_root_visited(&restored).is_ok());
}

// ------------------------------------------------ §7 injections that need live seams (F, label conflict)

#[test]
fn skip_root_makes_counter_example_f_constructible() {
    // F: the root stays in the declared set, but mark refuses to visit it. Only G3's per-kind /
    // oid comparison can see this — G2 compares the declared set against what mark *reports*, and
    // mark reports nothing here, so the label is simply absent.
    use fs_workspace::gc::{GcHooks, GcInput};

    let truth = RootSet::from_entries(vec![
        RootRef::new(RootKind::Commit, oid("f-commit"), "commit:c1"),
        RootRef::new(RootKind::Snapshot, oid("f-snap"), "snapshot:s1"),
    ]);
    let source = Fixed(truth.entries().to_vec());

    let mut skip = |entry: &RootRef| entry.kind == RootKind::Snapshot;
    let mut hooks = GcHooks {
        skip_root: Some(&mut skip),
        ..Default::default()
    };
    let input = GcInput {
        declared: &truth,
        mark_source: &source as &dyn RootSource,
    };
    let report = input.mark(&mut hooks).expect("mark");

    assert!(
        criteria::g3_every_root_visited(&report).is_err(),
        "G3 must be red: a declared root was never visited"
    );
    assert!(
        criteria::g2_mark_covers_declared(&report).is_err(),
        "G2 is red too, because mark never reported the label"
    );
    assert_eq!(report.visited_by_kind.get(&RootKind::Snapshot), None);
}

#[test]
fn a_label_naming_two_trees_is_an_error_not_two_facts() {
    // Without this, a mistyped label silently becomes two "facts" and G2's set comparison stops
    // meaning what it says.
    let error = RootSet::try_from_entries(vec![
        RootRef::new(RootKind::Snapshot, oid("one"), "snapshot:s1"),
        RootRef::new(RootKind::Snapshot, oid("two"), "snapshot:s1"),
    ])
    .expect_err("a label must name one tree");
    let message = error.to_string();
    assert!(
        message.contains("snapshot:s1"),
        "the message names the label: {message}"
    );
}

// ---------------------------------------------------------------- tripwire T1–T3 (card §7, QA)
//
// One table, three checks. QA's first review found all three weak in the same way: they looked
// thorough but could not fail for the reason they claimed. So:
//
//   T1 enumerates **both** databases (the metadata database *and* every workspace overlay) and
//      fails on a root-shaped column nobody classified — the overlay half is the point, since
//      `ov_meta`/`ov_inode` are where a GC root would hide;
//   T2 derives its two sides from *different* sources (this table vs `RootKind`'s variants) and
//      compares them; a table that agrees with itself would prove nothing;
//   T3 checks the value domain of every column classified as a root source, in whichever database
//      it lives.

/// The **complete** column inventory of every database a store has, each column classified.
///
/// `RootSource(<Kind>)` means "this column holds a root and a `RootKind` covers it"; `NotARoot(..)`
/// records why it is not one. This is a golden list on purpose (card §7): a regex over column names
/// only catches names that happen to look root-ish, and Seal could name its reference column
/// `seal`, `chunk_set` or `payload_ref` — none of which a regex would ask about, so the root would be
/// silently unprotected. Comparing the **whole** inventory means a new column cannot be added
/// without a decision here, whatever it is called.
type Classification = (&'static str, &'static str, &'static str, &'static str);

const CLASSIFIED: &[Classification] = &[
    (
        "main",
        "branch",
        "head_commit",
        "NotARoot(a commit id; that commit row’s root_oid is the root)",
    ),
    (
        "main",
        "branch",
        "name",
        "NotARoot(a branch name, not a tree)",
    ),
    (
        "main",
        "branch",
        "scope_id",
        "NotARoot(a scope id, not a tree)",
    ),
    ("main", "branch_log", "branch", "NotARoot(a branch name)"),
    (
        "main",
        "branch_log",
        "created_ns",
        "NotARoot(audit timestamp)",
    ),
    (
        "main",
        "branch_log",
        "new_commit",
        "NotARoot(a commit id; the commit table enumerates its root)",
    ),
    (
        "main",
        "branch_log",
        "old_commit",
        "NotARoot(a commit id; the commit table enumerates its root)",
    ),
    ("main", "branch_log", "op", "NotARoot(the audit verb)"),
    (
        "main",
        "branch_log",
        "seq",
        "NotARoot(an audit sequence number)",
    ),
    (
        "main",
        "commit",
        "commit_id",
        "NotARoot(the commit’s own identity)",
    ),
    ("main", "commit", "created_ns", "NotARoot(metadata)"),
    ("main", "commit", "message", "NotARoot(metadata)"),
    (
        "main",
        "commit",
        "parent_commit",
        "NotARoot(a commit id; the commit table enumerates its root)",
    ),
    ("main", "commit", "root_oid", "RootSource(Commit)"),
    (
        "main",
        "object_location",
        "kind",
        "NotARoot(index bookkeeping)",
    ),
    (
        "main",
        "object_location",
        "length",
        "NotARoot(index bookkeeping)",
    ),
    (
        "main",
        "object_location",
        "location",
        "NotARoot(index bookkeeping)",
    ),
    (
        "main",
        "object_location",
        "offset",
        "NotARoot(index bookkeeping)",
    ),
    (
        "main",
        "object_location",
        "oid",
        "NotARoot(the index’s own id: the marked side, never a root)",
    ),
    (
        "main",
        "object_location",
        "role",
        "NotARoot(index bookkeeping)",
    ),
    ("main", "pack", "bytes", "NotARoot(pack bookkeeping)"),
    ("main", "pack", "created_ns", "NotARoot(pack bookkeeping)"),
    ("main", "pack", "objects", "NotARoot(pack bookkeeping)"),
    (
        "main",
        "pack",
        "pack_id",
        "NotARoot(names the container; its contents are object_location rows)",
    ),
    (
        "main",
        "schema_definition",
        "ddl",
        "NotARoot(schema bookkeeping)",
    ),
    (
        "main",
        "schema_definition",
        "id",
        "NotARoot(schema bookkeeping)",
    ),
    (
        "main",
        "schema_definition",
        "schema_version",
        "NotARoot(schema bookkeeping)",
    ),
    (
        "main",
        "scope_allocator",
        "next_serial",
        "NotARoot(a serial allocator)",
    ),
    (
        "main",
        "scope_allocator",
        "scope_id",
        "NotARoot(a scope id, not a tree)",
    ),
    (
        "main",
        "snapshot",
        "created_ns",
        "NotARoot(snapshot bookkeeping)",
    ),
    ("main", "snapshot", "root_oid", "RootSource(Snapshot)"),
    (
        "main",
        "snapshot",
        "workspace",
        "NotARoot(a workspace name)",
    ),
    ("main", "store_policy", "key", "NotARoot(a policy key)"),
    (
        "main",
        "store_policy",
        "value",
        "NotARoot(policy scalars; a key/value store is not a tree)",
    ),
    (
        "main",
        "workspace",
        "base_commit",
        "NotARoot(a commit id; the commit table enumerates that root)",
    ),
    (
        "main",
        "workspace",
        "base_root",
        "RootSource(WorkspaceBase)",
    ),
    ("main", "workspace", "branch", "NotARoot(a branch name)"),
    (
        "main",
        "workspace",
        "created_ns",
        "NotARoot(workspace bookkeeping)",
    ),
    ("main", "workspace", "id", "NotARoot(workspace bookkeeping)"),
    (
        "main",
        "workspace",
        "name",
        "NotARoot(workspace bookkeeping)",
    ),
    (
        "main",
        "workspace",
        "state",
        "NotARoot(workspace bookkeeping)",
    ),
    ("main", "workspace_policy", "key", "NotARoot(a policy key)"),
    (
        "main",
        "workspace_policy",
        "value",
        "NotARoot(policy scalars; a key/value store is not a tree)",
    ),
    (
        "overlay",
        "ov_entry",
        "child",
        "NotARoot(an inode serial inside the overlay, not an object id)",
    ),
    ("overlay", "ov_entry", "name", "NotARoot(a name)"),
    ("overlay", "ov_entry", "parent", "NotARoot(an inode serial)"),
    (
        "overlay",
        "ov_inode",
        "base_limit",
        "NotARoot(overlay bookkeeping)",
    ),
    (
        "overlay",
        "ov_inode",
        "created_ns",
        "NotARoot(overlay bookkeeping)",
    ),
    (
        "overlay",
        "ov_inode",
        "entries",
        "NotARoot(overlay bookkeeping)",
    ),
    (
        "overlay",
        "ov_inode",
        "kind",
        "NotARoot(overlay bookkeeping)",
    ),
    (
        "overlay",
        "ov_inode",
        "live",
        "NotARoot(overlay bookkeeping)",
    ),
    (
        "overlay",
        "ov_inode",
        "modified_ns",
        "NotARoot(overlay bookkeeping)",
    ),
    (
        "overlay",
        "ov_inode",
        "accessed_ns",
        "NotARoot(a timestamp, not an object reference; v3 column)",
    ),
    (
        "overlay",
        "ov_inode",
        "attributes",
        "NotARoot(attribute bits, not an object reference; v3 column)",
    ),
    (
        "overlay",
        "ov_inode",
        "changed_ns",
        "NotARoot(a timestamp, not an object reference; v3 column)",
    ),
    (
        "overlay",
        "ov_inode",
        "origin",
        "NotARoot(overlay bookkeeping)",
    ),
    (
        "overlay",
        "ov_inode",
        "security",
        "NotARoot(security descriptor bytes stored inline, not an object reference; v3 column)",
    ),
    (
        "overlay",
        "ov_inode",
        "serial",
        "NotARoot(inode identity inside the overlay, not a tree root)",
    ),
    (
        "overlay",
        "ov_inode",
        "size",
        "NotARoot(overlay bookkeeping)",
    ),
    (
        "overlay",
        "ov_meta",
        "key",
        "NotARoot(the selector; the root lives in `value` under key `pending_root`)",
    ),
    ("overlay", "ov_meta", "value", "RootSource(OverlayPending)"),
    (
        "overlay",
        "ov_page",
        "data",
        "NotARoot(overlay page bytes stored inline, not an object reference)",
    ),
    (
        "overlay",
        "ov_page",
        "page_index",
        "NotARoot(page bookkeeping)",
    ),
    ("overlay", "ov_page", "serial", "NotARoot(page bookkeeping)"),
];

/// Opens every database a store has: the metadata one plus each workspace overlay.
fn databases(dir: &Path) -> Vec<(String, rusqlite::Connection)> {
    let mut found = vec![(
        "main".to_owned(),
        rusqlite::Connection::open(dir.join("store.sqlite")).expect("main db"),
    )];
    let workspaces = dir.join("workspaces");
    if let Ok(entries) = std::fs::read_dir(&workspaces) {
        for entry in entries.flatten() {
            let overlay = entry.path().join("overlay.sqlite");
            if overlay.is_file() {
                found.push((
                    "overlay".to_owned(),
                    rusqlite::Connection::open(&overlay).expect("overlay db"),
                ));
            }
        }
    }
    found
}

/// The store the tripwires inspect: a commit **and** an overlay, so both database kinds exist.
fn fixture(label: &str) -> (TempRoot, PathBuf) {
    let root = TempRoot::new(label);
    let dir = root.store();
    let repo = std::sync::Arc::new(Repo::open(&dir, true).expect("open"));
    repo.init().expect("init");
    let mut workspace = repo.workspace_create("w1", "main").expect("workspace");
    workspace.create_file("/f").expect("create");
    workspace.write_at("/f", 0, b"tripwire").expect("write");
    workspace.snapshot().expect("snapshot");
    workspace.close().expect("close");
    drop(repo);
    (root, dir)
}

/// Every `(table, column)` a database actually has, as SQLite reports it.
///
/// One function so T1 and any later tripwire cannot drift apart in *how* they enumerate — the
/// interesting disagreement should be about classification, never about which columns were looked at.
///
/// Two filters, both deliberate:
///
/// - `type = 'table'` and name not starting with `sqlite_`: SQLite's own bookkeeping
///   (`sqlite_sequence`, `sqlite_stat1`, …) is not part of any schema this project can add a root to;
/// - `hidden = 0` via `pragma_table_xinfo`: `table_info` also lists **hidden** columns (a virtual
///   table's, and generated ones). A hidden column cannot be selected by a plain query, so calling it
///   a root source would describe protection the collector cannot implement.
fn schema_columns(connection: &rusqlite::Connection) -> Vec<(String, String)> {
    let mut statement = connection
        .prepare(
            "SELECT m.name, p.name FROM sqlite_master AS m
             JOIN pragma_table_xinfo(m.name) AS p
             WHERE m.type = 'table'
               AND m.name NOT LIKE 'sqlite_%'
               AND p.hidden = 0",
        )
        .expect("prepare a schema query");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("collect")
}

#[test]
fn t1_every_column_of_every_database_is_classified() {
    let (_root, dir) = fixture("t1-columns");

    let mut actual: Vec<(String, String, String)> = Vec::new();
    for (database, connection) in databases(&dir) {
        for (table, column) in schema_columns(&connection) {
            actual.push((database.clone(), table, column));
        }
    }
    actual.sort();

    let frozen: Vec<(String, String, String)> = {
        let mut list: Vec<(String, String, String)> = CLASSIFIED
            .iter()
            .map(|(db, table, column, _)| {
                ((*db).to_owned(), (*table).to_owned(), (*column).to_owned())
            })
            .collect();
        list.sort();
        list
    };

    // Both directions matter: an unclassified new column is a root that escapes the enumeration, and
    // a frozen column that no longer exists means the list describes a schema nobody has.
    let unclassified: Vec<_> = actual
        .iter()
        .filter(|entry| !frozen.contains(entry))
        .collect();
    let vanished: Vec<_> = frozen
        .iter()
        .filter(|entry| !actual.contains(entry))
        .collect();
    assert!(
        unclassified.is_empty(),
        "columns added without a classification: {unclassified:?}"
    );
    assert!(
        vanished.is_empty(),
        "the frozen list names columns that no longer exist: {vanished:?}"
    );
    assert_eq!(
        actual.len(),
        CLASSIFIED.len(),
        "one row per column, exactly"
    );
}

#[test]
fn t2_every_classified_root_source_has_a_kind_and_a_kind_has_a_column() {
    // Two independent sides: the classification table (T1) and the `RootKind` variants (the code).
    // Comparing them catches "added a source, forgot the kind" and "added a kind, forgot a column".
    let classified: std::collections::BTreeSet<&str> = CLASSIFIED
        .iter()
        .filter_map(|(_, _, _, classification)| {
            classification
                .strip_prefix("RootSource(")
                .map(|rest| rest.trim_end_matches(')').to_owned())
        })
        .map(|kind| Box::leak(kind.into_boxed_str()) as &str)
        .collect();

    let kinds: std::collections::BTreeSet<&str> = [
        RootKind::Commit,
        RootKind::Snapshot,
        RootKind::WorkspaceBase,
        RootKind::OverlayPending,
        RootKind::SealedContent,
    ]
    .iter()
    .map(|kind| match kind {
        RootKind::Commit => "Commit",
        RootKind::Snapshot => "Snapshot",
        RootKind::WorkspaceBase => "WorkspaceBase",
        RootKind::OverlayPending => "OverlayPending",
        RootKind::SealedContent => "SealedContent",
    })
    .collect();

    let missing: Vec<&&str> = kinds.difference(&classified).collect();
    // Card §7: `sealed_content` does not exist until Seal lands, and when it does, T1 must classify
    // it. Until then the only acceptable gap is exactly this one — a new kind without a column has
    // to be added here deliberately, which is what makes the tripwire live.
    assert_eq!(
        missing,
        vec![&"SealedContent"],
        "a RootKind without a classified column must be the documented pending one"
    );
    let unexpected: Vec<&&str> = classified.difference(&kinds).collect();
    assert!(
        unexpected.is_empty(),
        "T1 classifies roots of kinds RootKind does not know: {unexpected:?}"
    );
}

#[test]
fn t3_root_source_values_parse_as_object_ids_and_are_published() {
    let (_root, dir) = fixture("t3-values");
    let index = rusqlite::Connection::open(dir.join("store.sqlite")).expect("index");
    let mut checked = 0usize;

    for (database, table, column, classification) in CLASSIFIED {
        if !classification.starts_with("RootSource(") {
            continue;
        }
        // `ov_meta` is key/value storage: the root lives under the `pending_root` key.
        let sql = if *database == "overlay" && *table == "ov_meta" {
            // Key/value storage: only the `pending_root` row is a root, and a clean workspace has
            // no such row at all — an empty string here would be a *different* column, not this one.
            "SELECT value FROM ov_meta WHERE key = 'pending_root' AND value <> '' LIMIT 50"
                .to_owned()
        } else {
            format!("SELECT DISTINCT {column} FROM \"{table}\" LIMIT 50")
        };
        let Some((_, connection)) = databases(&dir)
            .into_iter()
            .find(|(name, _)| name == database)
        else {
            continue;
        };
        // A column that does not exist yet (Seal) is T1's business, not a value-domain failure.
        let Ok(mut statement) = connection.prepare(&sql) else {
            continue;
        };
        let values: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("collect");
        for value in values {
            checked += 1;
            let oid = value.parse::<ObjectId>().unwrap_or_else(|_| {
                panic!("{database}:{table}.{column} holds {value:?}, which is not an object id")
            });
            // Card §21: "parses as an ObjectId" is too weak — a commit id is 64 hex characters too,
            // so `commit.commit_id` would pass while being the wrong classification. A real root has
            // an index row. Only the index is consulted, never the file, so KI-11's external deleter
            // cannot make this criterion red.
            let published: i64 = index
                .query_row(
                    "SELECT COUNT(*) FROM object_location WHERE oid = ?1",
                    [oid.to_string()],
                    |row| row.get(0),
                )
                .expect("index lookup");
            assert_eq!(
                published, 1,
                "{database}:{table}.{column} holds {value:?}, which has no index row: a value that \
                 merely looks like an object id is the signature of a misclassified column"
            );
        }
    }
    assert!(checked > 0, "T3 sampled nothing, so it proves nothing");
}

/// The enumerator's two filters are **live**, not decorative: each is shown to exclude something real,
/// and the unfiltered query is shown to include it (otherwise a filter that excluded everything, or
/// nothing, would pass).
#[test]
fn the_column_enumerator_applies_both_filters() {
    let connection = rusqlite::Connection::open_in_memory().expect("memory db");
    connection
        .execute_batch(
            "CREATE TABLE with_generated (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 base INTEGER NOT NULL,
                 derived INTEGER GENERATED ALWAYS AS (base * 2) VIRTUAL
             ) STRICT;",
        )
        .expect("ddl");

    let filtered = schema_columns(&connection);
    let unfiltered: Vec<(String, String)> = connection
        .prepare(
            "SELECT m.name, p.name FROM sqlite_master AS m
             JOIN pragma_table_xinfo(m.name) AS p",
        )
        .expect("prepare")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("collect");

    assert!(
        filtered
            .iter()
            .any(|(table, column)| table == "with_generated" && column == "base"),
        "a real column must survive both filters"
    );
    assert!(
        unfiltered
            .iter()
            .any(|(table, column)| table == "with_generated" && column == "derived"),
        "the unfiltered catalog query sees the generated column, so the filter has something to remove"
    );
    assert!(
        !filtered.iter().any(|(_, column)| column == "derived"),
        "a generated (hidden) column is not selectable, so it cannot be a root source"
    );
    assert!(
        !filtered
            .iter()
            .any(|(table, _)| table.starts_with("sqlite_")),
        "SQLite's own bookkeeping tables are not part of this schema"
    );
    assert!(
        unfiltered
            .iter()
            .any(|(table, _)| table == "sqlite_sequence"),
        "the AUTOINCREMENT table made SQLite create sqlite_sequence, so that filter bites too"
    );
}
