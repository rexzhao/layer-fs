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

#[cfg(feature = "gc-test-hooks")]
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

// ------------------------------------------------------ T1 (tripwire): every root-ish column is classified
//
// A new column that carries a root and is not classified here means the enumeration silently
// missed a source: the collector would then look correct while protecting nothing. The table below
// is the classification; the test fails when the schema grows a column nobody classified.

#[test]
fn every_root_shaped_column_is_classified() {
    // (table, column) → which RootSource covers it, or why it is not a root.
    // Keep this in sync with `RootKind`; T2 (registry ↔ criterion mapping) builds on it.
    const CLASSIFIED: &[(&str, &str, &str)] = &[
        ("commit", "root_oid", "RootSource(Commit)"),
        ("snapshot", "root_oid", "RootSource(Snapshot)"),
        ("workspace", "base_root", "RootSource(WorkspaceBase)"),
        (
            "workspace",
            "base_commit",
            "NotARoot(points at a commit row, whose root_oid is the root)",
        ),
        ("branch", "head_commit", "NotARoot(points at a commit row)"),
        (
            "branch_log",
            "old_commit",
            "NotARoot(points at a commit row; its root_oid is the root)",
        ),
        (
            "branch_log",
            "new_commit",
            "NotARoot(points at a commit row)",
        ),
        (
            "object_location",
            "oid",
            "NotARoot(the CAS index's own object id — the *marked* side, not a root)",
        ),
        (
            "pack",
            "pack_id",
            "NotARoot(pack names the container; the objects inside are rows of object_location)",
        ),
    ];

    let root = TempRoot::new("t1-columns");
    let repo = Repo::open(root.store(), true).expect("open");
    repo.init().expect("init");
    let connection = rusqlite::Connection::open(repo.metadata_path()).expect("db");

    let mut statement = connection
        .prepare(
            "SELECT m.name, p.name FROM sqlite_master AS m
             JOIN pragma_table_info(m.name) AS p
             WHERE m.type = 'table'",
        )
        .expect("prepare");
    let columns: Vec<(String, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("collect");

    let looks_like_a_root = |column: &str| {
        let lower = column.to_ascii_lowercase();
        lower == "root"
            || lower.starts_with("root_")
            || lower.ends_with("_root")
            || lower == "oid"
            || lower.ends_with("_oid")
            || lower.contains("content")
    };

    let mut unclassified = Vec::new();
    for (table, column) in &columns {
        if !looks_like_a_root(column) {
            continue;
        }
        let known = CLASSIFIED
            .iter()
            .any(|(known_table, known_column, _)| known_table == table && known_column == column);
        if !known {
            unclassified.push(format!("{table}.{column}"));
        }
    }
    assert!(
        unclassified.is_empty(),
        "root-shaped columns added without classification: {unclassified:?}"
    );
}
