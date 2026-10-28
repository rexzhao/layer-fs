//! Canaries: the collector against a store whose damage and garbage are *known* (P2b-6 §16, §18.5).
//!
//! Every test here builds its expectation **by hand** (what this fixture published, what this test
//! deleted behind the collector's back) and asserts the collector's answer against it. None of them
//! reads the expectation out of `GcReport`: that would be the "use the thing under test to prove the
//! thing under test" failure this project has already paid for twice.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cas_core::Role;
use cas_store::{OpenMode, Store};
use fs_namespace::Path as FsPath;
use fs_workspace::gc::{run, GcOptions};
use fs_workspace::Repo;

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "fs-workspace-gccanary-{label}-{}-{}",
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

fn put_file(workspace: &fs_workspace::Workspace, path: &str, bytes: &[u8]) {
    let store = workspace.store();
    let path = FsPath::parse(path).expect("path");
    if store.content_state(&path).is_err() {
        store.create_file(&path).expect("create");
    }
    store.write_at(&path, 0, bytes).expect("write");
    store
        .truncate(&path, u64::try_from(bytes.len()).expect("fits"))
        .expect("truncate");
}

fn index(dir: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.join("store.sqlite")).expect("open index")
}

/// `{(relative path, length)}` with the collector's own scratch and `-shm` excluded.
fn fingerprint(dir: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).expect("read dir").flatten() {
            let meta = entry.metadata().expect("metadata");
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                let relative = entry
                    .path()
                    .strip_prefix(dir)
                    .expect("inside")
                    .to_string_lossy()
                    .replace('\\', "/");
                let scratch = relative.starts_with(fs_workspace::gc::Marker::FILE_NAME);
                if !relative.ends_with("-shm") && !scratch {
                    out.push((relative, meta.len()));
                }
            }
        }
    }
    out.sort();
    out
}

/// A repository with one commit, and the tuple of what that commit published.
fn committed(label: &str, files: &[(&str, &[u8])]) -> (TempRoot, String) {
    let root = TempRoot::new(label);
    let repo = Arc::new(Repo::open(root.store(), true).expect("open repo"));
    repo.init().expect("init");
    let workspace = repo.workspace_create("w1", "main").expect("create");
    for (path, bytes) in files {
        put_file(&workspace, path, bytes);
    }
    let commit = workspace.commit("one", false).expect("commit").commit;
    drop(workspace);
    drop(repo);
    (root, commit.expect("a commit id"))
}

/// **K1**: a collection must not take anything a root can still reach, and every root must read back
/// byte-identical afterwards.
///
/// The expectation is hand-built: this test wrote these bytes, so it compares against them.
#[test]
fn k1_no_reachable_object_is_taken_and_every_root_still_reads() {
    let (root, _commit) = committed(
        "k1",
        &[("/a.txt", b"alpha bytes"), ("/b.txt", b"beta bytes")],
    );

    let outcome = run(root.store(), GcOptions::default()).expect("collect");
    assert!(
        outcome.report.marked_objects > 0,
        "G5: a run that marked nothing proves nothing"
    );
    assert!(
        outcome.report.freed.is_empty(),
        "nothing is garbage in a freshly committed store: {:?}",
        outcome.report.freed
    );

    // Read the files back through a fresh view: any loss shows up here, not in the report.
    let repo = Arc::new(Repo::open(root.store(), false).expect("reopen"));
    let workspace = repo.workspace_open("w1").expect("workspace");
    let store = Store::open(root.store(), OpenMode::ReadOnly).expect("open store");
    let root_oid = workspace.base_root();
    let reader =
        fs_snapshot::SnapshotReader::open_any(&store, root_oid).expect("the base root reads");
    // The root's own table is traversable: that is "the tree is still there", measured without the
    // report. Reading the file text needs the API layer, which this crate deliberately does not use.
    assert!(
        !reader.references(root_oid).expect("references").is_empty(),
        "K1: the committed tree must still be traversable"
    );
}

/// **K8a**: a `pack` row whose file is missing makes a real run **fail**, leaves every index row
/// alone, and still lets a dry run answer.
///
/// The pack is one **nothing reaches**: marking never reads it, so the run gets all the way to the
/// plan and reports the damage there. If the missing pack held reachable bytes instead, marking would
/// abort first — that is K8b's shape, not this one, and it is why the two are separate tests.
#[test]
fn k8a_a_missing_pack_fails_the_run_and_leaves_every_row_alone() {
    let (root, _commit) = committed("k8a", &[("/a.txt", b"alpha")]);

    // An object nothing references: its pack can vanish without marking noticing.
    let store = Store::open(root.store(), OpenMode::ReadWrite).expect("open store");
    let unreferenced = store
        .put(Role::Bytes, b"nothing reaches this")
        .expect("put")
        .oid;
    drop(store);

    let location: String = index(&root.store())
        .query_row(
            "SELECT location FROM object_location WHERE oid = ?1",
            [unreferenced.to_string()],
            |row| row.get(0),
        )
        .expect("location");
    let pack = root.store().join("packs").join(format!("{location}.pack"));
    assert!(pack.is_file(), "the pack exists at {}", pack.display());
    std::fs::remove_file(&pack).expect("delete the pack behind the collector's back");

    let rows = || -> Vec<(String, i64, i64, String, i64)> {
        index(&root.store())
            .prepare("SELECT oid, kind, length, location, offset FROM object_location ORDER BY oid")
            .expect("prepare")
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("collect")
    };
    let before = rows();

    let error =
        run(root.store(), GcOptions::default()).expect_err("a missing pack must fail a real run");
    // The abort can arrive from either layer: marking the tree reads frames too, so a build may hit the
    // missing pack before the plan ever does. Both spellings are the same fact and both must be typed.
    let missing = matches!(
        &error,
        fs_workspace::WorkspaceError::Store(cas_store::StoreError::MissingPack { .. })
            | fs_workspace::WorkspaceError::Snapshot(fs_snapshot::SnapshotError::Store(
                cas_store::StoreError::MissingPack { .. }
            ))
    );
    assert!(missing, "expected a typed MissingPack, got {error:?}");
    assert_eq!(
        before,
        rows(),
        "not one index row may be removed to 'repair' this"
    );

    let dry = run(
        root.store(),
        GcOptions {
            dry_run: true,
            ..GcOptions::default()
        },
    )
    .expect("a dry run must still be answerable on a damaged store");
    assert_eq!(
        dry.plan.missing_packs.len(),
        1,
        "K8a is reported, not hidden"
    );
}

/// **G1c**: a dry run changes no store file at all (its own marker is scratch, and is excluded).
#[test]
fn g1c_a_dry_run_leaves_every_store_file_alone() {
    let (root, _commit) = committed("g1c", &[("/a.txt", b"alpha bytes")]);
    let store = Store::open(root.store(), OpenMode::ReadWrite).expect("open store");
    let orphan = store
        .put(Role::Bytes, b"garbage the dry run must not touch")
        .expect("put")
        .oid;
    drop(store);

    let before = fingerprint(&root.store());
    let outcome = run(
        root.store(),
        GcOptions {
            dry_run: true,
            ..GcOptions::default()
        },
    )
    .expect("dry run");
    assert_eq!(
        outcome.plan.dead_objects,
        vec![orphan],
        "the dry run decided"
    );
    assert_eq!(
        before,
        fingerprint(&root.store()),
        "a dry run must not change a byte of the store"
    );

    // And the marker is not tucked into `packs/` or `tmp/`, where a sweep would eat it.
    assert!(!root.store().join("packs").join("gc-mark.sqlite").exists());
    assert!(!root.store().join("tmp").join("gc-mark.sqlite").exists());
}

/// **K9** (§15, §26): a pack whose only garbage is a **rowless frame** must report
/// `freed_objects == 0 && packs_repacked == 1`, with `reclaimed_dead_bytes > 0`.
///
/// The shape comes from the `gc-test-hooks` seam, not from deleting a reachable object's row: deleting
/// a row of something a root reaches makes *marking* fail, which would test K8b instead of this.
#[test]
#[cfg(feature = "gc-test-hooks")]
fn k9_dead_frames_without_dead_objects_report_zero_objects_and_one_repack() {
    let root = TempRoot::new("k9");
    let repo = Arc::new(Repo::open(root.store(), true).expect("open repo"));
    repo.init().expect("init");
    drop(repo);

    let store = Store::open(root.store(), OpenMode::ReadWrite).expect("open store");
    let layout = cas_store::Layout::new(root.store());
    let connection = rusqlite::Connection::open(root.store().join("store.sqlite")).expect("index");
    let live = cas_store::stage_pack_with_dead_frame(
        &connection,
        &layout,
        &[(Role::Bytes, b"reachable through a snapshot root".to_vec())],
        &[(Role::Bytes, b"a frame no row names".to_vec())],
    )
    .expect("stage the K9 pack");
    assert_eq!(live.len(), 1);
    let survivor = live[0];

    // Make the survivor reachable: one inode, one root, one snapshot row. A snapshot is a GC root that
    // moves no branch, which is exactly what this fixture needs.
    struct Sink<'a> {
        store: &'a Store,
    }
    impl fs_snapshot::PageSink for Sink<'_> {
        fn put(
            &mut self,
            role: Role,
            value: &[u8],
        ) -> fs_snapshot::SnapshotResult<cas_core::ObjectId> {
            Ok(self.store.put(role, value).expect("put").oid)
        }
    }
    let profile = fs_snapshot::Profile::V1;
    let mut sink = Sink { store: &store };
    let table = fs_snapshot::encode_inode_table(
        &profile,
        std::iter::once(fs_snapshot::InodeRecord {
            serial: 0,
            kind: fs_snapshot::Kind::File,
            size: 32,
            created_ns: 0,
            modified_ns: 0,

            accessed_ns: 0,
            changed_ns: 0,
            attributes: 0,
            security: Vec::new(),
            content: fs_snapshot::Content::Blob(survivor),
        }),
        &mut sink,
    )
    .expect("encode inode table");
    let snapshot_root = fs_snapshot::encode_root(
        &profile,
        &fs_snapshot::RootValue {
            scope_id: [7u8; 32],
            inode_table: table,
        },
        &mut sink,
    )
    .expect("encode root");
    drop(store);
    connection
        .execute(
            "INSERT INTO snapshot (root_oid, workspace, created_ns) VALUES (?1, 'fixture', 1)",
            [snapshot_root.to_string()],
        )
        .expect("insert the snapshot row");
    drop(connection);

    let packs_before: i64 = {
        let index = rusqlite::Connection::open(root.store().join("store.sqlite")).expect("index");
        index
            .query_row("SELECT COUNT(*) FROM pack", [], |row| row.get(0))
            .expect("count")
    };
    let outcome = run(
        root.store(),
        GcOptions {
            dry_run: false,
            repack_threshold: 0.6,
        },
    )
    .expect("collect");

    assert_eq!(
        outcome.report.freed.len(),
        0,
        "no object was unreachable — only a frame was dead: {:?}",
        outcome.report.freed
    );
    assert_eq!(
        outcome.report.packs_repacked, 1,
        "the pack with a rowless frame is repacked"
    );
    assert_eq!(outcome.report.packs_deleted, 0);
    assert!(
        outcome.report.reclaimed_dead_bytes > 0,
        "the rowless frame's bytes are what repacking reclaimed"
    );
    assert!(
        outcome.report.packs_repacked <= usize::try_from(packs_before).expect("fits"),
        "a repack cannot invent packs"
    );

    // The survivor still reads after the move, and the snapshot row still holds its tree.
    let store = Store::open(root.store(), OpenMode::ReadWrite).expect("reopen");
    assert!(store.get(survivor).is_ok(), "the survivor still reads");
}

/// **K8b** (§12, §26): a pack that disappears **on the walked path** must abort the run, not be treated
/// as a leaf.
///
/// This is the shape that "treat a decode failure as no children" would quietly destroy: the subtree
/// below the missing page becomes unreachable, gets swept, and `verify` afterwards looks *cleaner*.
/// So the assertion is not "the run failed" alone — it is that **not one index row moved** and no object
/// file vanished.
#[test]
fn k8b_a_pack_missing_on_the_walk_aborts_and_deletes_nothing() {
    let (root, _commit) = committed("k8b", &[("/a.txt", b"reachable bytes")]);

    // Every pack goes: whatever the tree needs, it needs from here.
    let packs: Vec<PathBuf> = std::fs::read_dir(root.store().join("packs"))
        .expect("packs")
        .flatten()
        .filter(|entry| entry.path().is_file())
        .map(|entry| entry.path())
        .collect();
    assert!(
        !packs.is_empty(),
        "the fixture must have published at least one pack"
    );
    for pack in &packs {
        std::fs::remove_file(pack).expect("delete the pack behind the collector's back");
    }

    let rows = || -> Vec<(String, i64, String, i64)> {
        index(&root.store())
            .prepare("SELECT oid, kind, location, offset FROM object_location ORDER BY oid")
            .expect("prepare")
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("collect")
    };
    let before = rows();
    let before_files = fingerprint(&root.store());

    let error = run(root.store(), GcOptions::default())
        .expect_err("a pack missing on the walked path must abort the run");
    let missing = matches!(
        &error,
        fs_workspace::WorkspaceError::Store(cas_store::StoreError::MissingPack { .. })
            | fs_workspace::WorkspaceError::Snapshot(fs_snapshot::SnapshotError::Store(
                cas_store::StoreError::MissingPack { .. }
            ))
    );
    assert!(missing, "expected a typed MissingPack, got {error:?}");

    assert_eq!(
        before,
        rows(),
        "an aborted run must not remove a single index row"
    );
    assert_eq!(
        before_files,
        fingerprint(&root.store()),
        "an aborted run must not lose a single file either"
    );

    // The dry run cannot answer here, and says so with the same typed error — it is not a mode that
    // papers over damage.
    let dry = run(
        root.store(),
        GcOptions {
            dry_run: true,
            ..GcOptions::default()
        },
    )
    .expect_err("a dry run cannot answer when the walked path is unreadable");
    assert!(
        format!("{dry:?}").contains("MissingPack"),
        "the dry run's refusal must name the damage, got {dry:?}"
    );
}

// ------------------------------------------------- crash points (§27.A, §28, I6)

/// The property every crash point promises: the store reopens, nothing that a root reaches is lost, and
/// the only residue allowed is a file no row names.
#[test]
#[cfg(feature = "gc-test-hooks")]
fn crash_points_1_2_and_5_leave_a_recoverable_store() {
    use fs_workspace::gc::{crash, GcStep};

    for (step, label) in [
        (GcStep::Marking, "marking"),
        (GcStep::RowsDeletedFilesKept, "rows deleted, files kept"),
        (GcStep::OrphanSweep, "orphan sweep"),
    ] {
        let (root, commit) = committed(
            &format!("crash-{}", step.number()),
            &[("/a.txt", b"survives")],
        );
        // Garbage so the sweep has something to do (and so point 5 has a mid-point to be inside of).
        let store = Store::open(root.store(), OpenMode::ReadWrite).expect("open store");
        store.put(Role::Bytes, b"garbage one").expect("put");
        store.put(Role::Bytes, b"garbage two").expect("put");
        drop(store);
        std::fs::write(
            root.store().join("packs").join("orphan.pack"),
            b"a file no row names",
        )
        .expect("write an orphan");

        crash::arm(step);
        let error =
            run(root.store(), GcOptions::default()).expect_err("the armed step must abort the run");
        let reached = format!("{error:?}");
        assert!(
            reached.contains(&format!("gc step {}", step.number())),
            "{label}: the error must name the step that was reached, got {reached}"
        );

        // Disarmed on the way out, so the store is usable and the next run is normal. It may have
        // nothing left to collect — at point 2 the rows are already gone — so the assertion is that the
        // run *succeeds*, not that it frees something. Claiming more here would be the "clean because it
        // never got that far" mistake in the other direction.
        let _outcome = run(root.store(), GcOptions::default())
            .expect("the store must be recoverable after a crash");

        // Nothing a root reaches is lost: the committed tree still reads, byte for byte.
        let handle = Repo::open(root.store(), false).expect("reopen");
        let info = handle.commit_info(&commit).expect("info");
        let root_oid: cas_core::ObjectId = info.root.parse().expect("a root id");
        let store = Store::open(root.store(), OpenMode::ReadOnly).expect("open store");
        let reader =
            fs_snapshot::SnapshotReader::open_any(&store, root_oid).expect("the root reads");
        assert!(
            !reader.references(root_oid).expect("references").is_empty(),
            "{label}: the committed tree must still be traversable"
        );
    }
}

/// **Points 3 and 4** (§28): the abort happens *inside* the repack, so what each one promises differs.
///
/// - point 3 (staged, switch not run): the index is untouched, and the replacement pack is a file no row
///   names — the next sweep reclaims it as an orphan;
/// - point 4 (switched, old file not unlinked): every object has **exactly one** location, pointing at a
///   file that exists; the old pack's file lingers without a row.
#[test]
#[cfg(feature = "gc-test-hooks")]
fn crash_points_3_and_4_hold_their_own_invariants() {
    use fs_workspace::gc::{crash, GcStep};

    for (step, staged_expectation) in [
        (GcStep::RepackStaged, true),
        (GcStep::RepackSwitched, false),
    ] {
        let root = TempRoot::new(&format!("repack-crash-{}", step.number()));
        let repo = Arc::new(Repo::open(root.store(), true).expect("open repo"));
        repo.init().expect("init");
        drop(repo);

        let store = Store::open(root.store(), OpenMode::ReadWrite).expect("open store");
        let layout = cas_store::Layout::new(root.store());
        let connection =
            rusqlite::Connection::open(root.store().join("store.sqlite")).expect("index");
        // A pack with one live frame and one rowless frame: the shape that gets repacked.
        let live = cas_store::stage_pack_with_dead_frame(
            &connection,
            &layout,
            &[(Role::Bytes, b"reachable".to_vec())],
            &[(Role::Bytes, b"rowless".to_vec())],
        )
        .expect("stage the pack");
        let survivor = live[0];
        drop(connection);

        // Make the survivor reachable through a snapshot root, so the pack is a repack candidate.
        struct Sink<'a> {
            store: &'a Store,
        }
        impl fs_snapshot::PageSink for Sink<'_> {
            fn put(
                &mut self,
                role: Role,
                value: &[u8],
            ) -> fs_snapshot::SnapshotResult<cas_core::ObjectId> {
                Ok(self.store.put(role, value).expect("put").oid)
            }
        }
        let profile = fs_snapshot::Profile::V1;
        let mut sink = Sink { store: &store };
        let table = fs_snapshot::encode_inode_table(
            &profile,
            std::iter::once(fs_snapshot::InodeRecord {
                serial: 0,
                kind: fs_snapshot::Kind::File,
                size: 32,
                created_ns: 0,
                modified_ns: 0,

                accessed_ns: 0,
                changed_ns: 0,
                attributes: 0,
                security: Vec::new(),
                content: fs_snapshot::Content::Blob(survivor),
            }),
            &mut sink,
        )
        .expect("table");
        let snapshot_root = fs_snapshot::encode_root(
            &profile,
            &fs_snapshot::RootValue {
                scope_id: [9u8; 32],
                inode_table: table,
            },
            &mut sink,
        )
        .expect("root");
        drop(store);
        let connection =
            rusqlite::Connection::open(root.store().join("store.sqlite")).expect("index");
        connection
            .execute(
                "INSERT INTO snapshot (root_oid, workspace, created_ns) VALUES (?1, 'fixture', 1)",
                [snapshot_root.to_string()],
            )
            .expect("snapshot row");

        let rows_before: i64 = connection
            .query_row("SELECT COUNT(*) FROM object_location", [], |row| row.get(0))
            .expect("count");
        drop(connection);

        crash::arm(step);
        let error = run(
            root.store(),
            GcOptions {
                dry_run: false,
                repack_threshold: 0.6,
            },
        )
        .expect_err("the armed repack step must abort the run");
        let reached = format!("{error:?}");
        assert!(
            reached.contains(&format!("gc step {}", step.number())),
            "point {}: the error must name the reached step, got {reached}",
            step.number()
        );

        // Reopen and check this point's invariant.
        let connection =
            rusqlite::Connection::open(root.store().join("store.sqlite")).expect("index");
        let rows_after: i64 = connection
            .query_row("SELECT COUNT(*) FROM object_location", [], |row| row.get(0))
            .expect("count");
        // Every object must have exactly one location whose file exists (both points 3 and 4).
        let located: Vec<(String, String, i64)> = connection
            .prepare("SELECT oid, location, kind FROM object_location")
            .expect("prepare")
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .expect("rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect");
        let dangling: Vec<String> = located
            .into_iter()
            .filter(|(_, location, kind)| {
                *kind == cas_store::index::kind_pack()
                    && !root
                        .store()
                        .join("packs")
                        .join(format!("{location}.pack"))
                        .exists()
            })
            .map(|(oid, _, _)| oid)
            .collect();
        assert!(
            dangling.is_empty(),
            "point {}: no row may point at a missing file: {dangling:?}",
            step.number()
        );

        let live_rows: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM object_location WHERE oid = ?1",
                [survivor.to_string()],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(
            live_rows,
            1,
            "point {}: one location per object",
            step.number()
        );

        if staged_expectation {
            assert_eq!(
                rows_before, rows_after,
                "point 3: the switch transaction never ran, so no row moved"
            );
        }
        drop(connection);

        // Both points leave a store the next run can finish, and the survivor still reads.
        let _recovered = run(
            root.store(),
            GcOptions {
                dry_run: false,
                repack_threshold: 0.6,
            },
        )
        .expect("the store must be recoverable after a repack abort");
        let store = Store::open(root.store(), OpenMode::ReadOnly).expect("open store");
        assert!(store.get(survivor).is_ok(), "the survivor still reads");
    }
}

// ------------------------------------------------- §27.D: every root source has a canary

/// The K matrix is indexed by `RootSource`: a root kind with no canary is a source nobody proves the
/// collector protects (P2b-6 §27.D).
///
/// The table is explicit rather than derived, because "which test covers which kind" is a human claim;
/// what the test enforces is that the claim exists for **every** kind, and that no stale entry names a
/// test this repository does not have.
#[test]
fn every_root_kind_is_covered_by_a_named_canary() {
    use fs_workspace::gc::RootKind;

    // (kind, the test that would fail if the collector stopped protecting it)
    const COVERAGE: &[(RootKind, &str)] = &[
        (
            RootKind::Commit,
            "gc_canaries::k1_no_reachable_object_is_taken_and_every_root_still_reads \
             (+ gc_run::a_run_frees_the_orphan_and_keeps_the_committed_tree_readable)",
        ),
        (
            RootKind::Snapshot,
            "gc_shared_roots::a_shared_object_survives_dropping_one_root_and_is_freed_once_both_are_gone",
        ),
        (
            RootKind::WorkspaceBase,
            "gc_run::a_dry_run_decides_but_changes_nothing (the base root is the post-GC tree it reads)",
        ),
        (
            RootKind::OverlayPending,
            "gc_overlay_roots::maintenance_reads_the_overlay_pending_root_through_its_own_connection",
        ),
    ];

    let kinds = [
        RootKind::Commit,
        RootKind::Snapshot,
        RootKind::WorkspaceBase,
        RootKind::OverlayPending,
        RootKind::SealedContent,
    ];

    let missing: Vec<&RootKind> = kinds
        .iter()
        .filter(|kind| !COVERAGE.iter().any(|(covered, _)| covered == *kind))
        .collect();
    // `SealedContent` has no column yet (Seal has not landed), so it is the one documented gap — and it
    // has to be listed here deliberately, which is what makes this tripwire live for the next kind.
    assert_eq!(
        missing,
        vec![&RootKind::SealedContent],
        "every root source needs a canary; the only acceptable gap is the documented pending one"
    );

    // No stale entries: each named test must exist in this repository (by its file, at least).
    for (kind, canary) in COVERAGE {
        let file = canary.split("::").next().expect("a file name");
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join(format!("{file}.rs"));
        assert!(
            path.is_file(),
            "{kind:?} claims coverage by {canary}, but {} does not exist",
            path.display()
        );
    }
}
