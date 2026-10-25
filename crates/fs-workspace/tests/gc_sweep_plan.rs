//! The sweep plan: what garbage is, decided before anything is deleted (P2b-6 §18.3).

use std::path::PathBuf;

use cas_core::Role;
use cas_store::{OpenMode, Store};
use fs_workspace::gc::{plan, Marker};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "fs-workspace-gcsweep-{label}-{}-{}",
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

/// A marker containing exactly `keep`.
fn marker_with(store_dir: &std::path::Path, keep: &[cas_core::ObjectId]) -> Marker {
    let mut marker = Marker::create_in(store_dir).expect("marker");
    for oid in keep {
        assert!(
            marker.insert(*oid).expect("mark"),
            "fixture marks are distinct"
        );
    }
    marker
}

/// The index is read through the maintenance connection, so the test opens its own: what matters is
/// the rows, not which handle produced them.
fn index(store_dir: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(store_dir.join("store.sqlite")).expect("open index")
}

#[test]
fn an_unmarked_loose_object_is_the_only_thing_freed() {
    let root = TempRoot::new("loose");
    let store = Store::open(root.store(), OpenMode::Create).expect("open store");
    let keep = store.put(Role::Bytes, b"reachable").expect("put").oid;
    let garbage = store.put(Role::Bytes, b"unreachable").expect("put").oid;
    assert_ne!(keep, garbage, "the fixture's two objects must be different");

    // Control first: with both marked there is nothing to do, so a plan that always frees would be
    // caught before the real assertion runs.
    let marker = marker_with(&root.store(), &[keep, garbage]);
    let nothing = plan(&index(&root.store()), &marker, &root.store(), 0.5).expect("plan");
    assert!(
        nothing.dead_objects.is_empty(),
        "everything is reachable; got {:?}",
        nothing.dead_objects
    );
    assert_eq!(nothing.rows_checked, 2, "both rows were examined");
    drop(marker);

    let marker = marker_with(&root.store(), &[keep]);
    let decided = plan(&index(&root.store()), &marker, &root.store(), 0.5).expect("plan");
    assert_eq!(
        decided.dead_objects,
        vec![garbage],
        "only the unmarked object is garbage"
    );
    assert_eq!(
        decided.freed_object_bytes,
        u64::try_from(b"unreachable".len()).expect("fits") + 14,
        "the byte count is the object's framed length as the index records it"
    );
    assert_eq!(decided.freed_objects(), 1);
}

#[test]
fn a_pack_whose_frames_are_all_unreachable_is_deleted() {
    let root = TempRoot::new("pack-delete");
    let store = Store::open(root.store(), OpenMode::Create).expect("open store");
    let mut batch = store.batch();
    let first = batch.put(Role::Bytes, b"one").expect("put");
    let second = batch.put(Role::Bytes, b"two").expect("put");
    batch.commit().expect("commit batch");
    let packs = std::fs::read_dir(root.store().join("packs"))
        .expect("packs dir")
        .flatten()
        .filter(|entry| entry.path().is_file())
        .count();
    assert!(packs >= 1, "the batch must have produced a pack");

    // Keeping one frame makes the pack a repack candidate, not a delete: the threshold decides.
    // 1 of 2 survivors is exactly 0.5, and the comparison is strict, so 0.5 keeps the pack and only
    // a threshold above it makes it a repack candidate. Both sides are asserted, because an
    // off-by-one in that comparison is invisible if only one is.
    let marker = marker_with(&root.store(), &[first]);
    let at_half = plan(&index(&root.store()), &marker, &root.store(), 0.5).expect("plan");
    assert_eq!(
        at_half.packs_repack.len(),
        0,
        "exactly at the threshold is kept"
    );
    assert_eq!(at_half.packs_keep, 1);
    let decided = plan(&index(&root.store()), &marker, &root.store(), 0.6).expect("plan");
    assert_eq!(decided.packs_delete.len(), 0, "{:?}", decided.packs_delete);
    assert_eq!(decided.packs_repack.len(), 1, "half the frames survive");
    assert_eq!(decided.packs_repack[0].survivors, 1);
    assert_eq!(decided.packs_repack[0].objects, 2);
    assert_eq!(decided.dead_objects, vec![second]);
    drop(marker);

    // Nothing reachable: the whole pack goes.
    let marker = marker_with(&root.store(), &[]);
    let decided = plan(&index(&root.store()), &marker, &root.store(), 0.5).expect("plan");
    assert_eq!(decided.packs_delete.len(), 1);
    assert!(decided.packs_repack.is_empty());
    assert_eq!(decided.packs_delete[0].survivors, 0);
}

#[test]
fn a_missing_pack_file_is_reported_and_its_rows_are_left_alone() {
    let root = TempRoot::new("missing");
    let store = Store::open(root.store(), OpenMode::Create).expect("open store");
    let mut batch = store.batch();
    batch.put(Role::Bytes, b"one").expect("put");
    batch.commit().expect("commit batch");
    let pack = std::fs::read_dir(root.store().join("packs"))
        .expect("packs dir")
        .flatten()
        .find(|entry| entry.path().is_file())
        .expect("a pack file")
        .path();
    std::fs::remove_file(&pack).expect("delete the pack behind the collector's back");
    let before = index(&root.store())
        .query_row("SELECT COUNT(*) FROM object_location", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count");

    let marker = marker_with(&root.store(), &[]);
    // One connection for the whole comparison. Opening and closing a *second* connection checkpoints
    // the WAL and changes `store.sqlite-wal`'s length, which made this test flaky once (the assertion
    // was right, the measurement was not).
    let connection = index(&root.store());
    // After the marker exists: the comparison is about what *the plan* touches, not about the
    // fixture's own files.
    let before_bytes = fingerprint(&root.store());
    let decided = plan(&connection, &marker, &root.store(), 0.5).expect("plan");
    assert_eq!(
        decided.missing_packs.len(),
        1,
        "K8a: the row's file is gone"
    );
    assert_eq!(
        before_bytes,
        fingerprint(&root.store()),
        "a plan is a pure read: deciding must not touch the store"
    );
    let after = connection
        .query_row("SELECT COUNT(*) FROM object_location", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count");
    assert_eq!(before, after, "the plan must not delete index rows");
}

#[test]
fn a_threshold_outside_the_unit_interval_is_refused() {
    let root = TempRoot::new("threshold");
    let store = Store::open(root.store(), OpenMode::Create).expect("open store");
    store.put(Role::Bytes, b"x").expect("put");
    let marker = marker_with(&root.store(), &[]);

    for bad in [-0.1, 1.1, f64::NAN] {
        let error = plan(&index(&root.store()), &marker, &root.store(), bad)
            .expect_err("a threshold outside [0, 1] must be refused");
        assert!(
            matches!(error, fs_workspace::WorkspaceError::Inconsistent { .. }),
            "expected a typed inconsistency for {bad}, got {error:?}"
        );
    }
    // Control: the boundary values are legal.
    for good in [0.0, 1.0] {
        plan(&index(&root.store()), &marker, &root.store(), good).expect("a legal threshold");
    }
}

#[test]
fn a_pack_file_without_a_row_is_an_orphan() {
    let root = TempRoot::new("orphan");
    let store = Store::open(root.store(), OpenMode::Create).expect("open store");
    store.put(Role::Bytes, b"x").expect("put");
    let orphan = root.store().join("packs").join("orphan.pack");
    std::fs::write(&orphan, b"not a pack anybody claims").expect("write");

    let marker = marker_with(&root.store(), &[]);
    let decided = plan(&index(&root.store()), &marker, &root.store(), 0.5).expect("plan");
    assert_eq!(
        decided.orphan_pack_files,
        vec![orphan],
        "a pack file no row claims can never be read"
    );
}

// ------------------------------------------------------------------- apply

use fs_workspace::gc::apply;

/// A store with one reachable packed object and one **loose** (legacy-shape) object: the small case
/// that exercises both storage kinds in one apply.
///
/// Writes always produce packs now, so a loose row can only be produced the way a legacy store has
/// one: a file under `objects/ab/cd/` plus a `KIND_LOOSE` row. `KIND_LOOSE` is "read only, P1/P2a
/// shape", which is exactly why the collector has to handle it.
fn packed_and_loose(label: &str) -> (TempRoot, cas_core::ObjectId, cas_core::ObjectId) {
    let root = TempRoot::new(label);
    let store = Store::open(root.store(), OpenMode::Create).expect("open store");
    let keep = store.put(Role::Bytes, b"reachable").expect("put").oid;

    let framed = cas_core::Object::new(Role::Bytes, b"legacy loose object")
        .expect("framed")
        .to_vec();
    let garbage = cas_core::Object::decode(&framed)
        .expect("decode")
        .id()
        .expect("id");
    assert_ne!(keep, garbage);
    let layout = cas_store::Layout::new(root.store());
    let path = layout.object_path(garbage);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(&path, &framed).expect("write loose object");
    let connection = rusqlite::Connection::open(root.store().join("store.sqlite")).expect("index");
    connection
        .execute(
            "INSERT INTO object_location (oid, role, length, kind, location, offset)
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            rusqlite::params![
                garbage.to_string(),
                i64::from(Role::Bytes.tag()),
                i64::try_from(framed.len()).expect("fits"),
                cas_store::index::kind_loose(),
                layout.object_relative(garbage),
            ],
        )
        .expect("insert loose row");
    drop(connection);
    (root, keep, garbage)
}

#[test]
fn apply_deletes_a_loose_object_file_and_its_row() {
    let (root, keep, garbage) = packed_and_loose("apply-loose");
    // Positive control: the loose object is readable *before* the sweep, so "it is gone afterwards"
    // is a change and not a fixture that never had it.
    {
        let store = Store::open(root.store(), OpenMode::ReadWrite).expect("reopen");
        assert!(store.get(garbage).is_ok(), "the loose object reads before");
        assert!(store.get(keep).is_ok(), "the packed object reads before");
    }

    let marker = marker_with(&root.store(), &[keep]);
    let mut connection = index(&root.store());
    let decided = plan(&connection, &marker, &root.store(), 0.5).expect("plan");
    assert_eq!(
        decided.dead_objects,
        vec![garbage],
        "only the loose object is unreachable"
    );

    let layout = cas_store::Layout::new(root.store());
    assert!(
        layout.object_path(garbage).exists(),
        "the file is there before"
    );

    let applied = apply(&mut connection, &decided, &root.store()).expect("apply");
    assert_eq!(applied.index_rows_deleted, 1);
    assert_eq!(applied.loose_files_deleted, 1);
    assert!(
        !layout.object_path(garbage).exists(),
        "the dead object's file must be gone"
    );

    let store = Store::open(root.store(), OpenMode::ReadWrite).expect("reopen");
    assert!(store.get(keep).is_ok(), "the survivor is still readable");
    assert!(
        store.get(garbage).is_err(),
        "the deleted object must no longer be readable"
    );

    // A second plan over the same marker has nothing left to free.
    let again = plan(&index(&root.store()), &marker, &root.store(), 0.5).expect("plan again");
    assert!(again.dead_objects.is_empty(), "{:?}", again.dead_objects);
}

#[test]
fn apply_removes_an_empty_pack_rows_first_then_file() {
    let root = TempRoot::new("apply-pack");
    let store = Store::open(root.store(), OpenMode::Create).expect("open store");
    let mut batch = store.batch();
    batch.put(Role::Bytes, b"one").expect("put");
    batch.put(Role::Bytes, b"two").expect("put");
    batch.commit().expect("commit batch");

    let marker = marker_with(&root.store(), &[]);
    let mut connection = index(&root.store());
    let decided = plan(&connection, &marker, &root.store(), 0.5).expect("plan");
    assert_eq!(decided.packs_delete.len(), 1, "{:?}", decided);
    let pack_path = decided.packs_delete[0].path.clone();
    assert!(pack_path.exists());

    let applied = apply(&mut connection, &decided, &root.store()).expect("apply");
    assert_eq!(applied.packs_deleted, 1);
    assert_eq!(applied.index_rows_deleted, 2, "both frames' rows");
    assert!(!pack_path.exists(), "the pack file must be gone");
    let packs_left: i64 = connection
        .query_row("SELECT COUNT(*) FROM pack", [], |row| row.get(0))
        .expect("count packs");
    assert_eq!(packs_left, 0, "no pack row may outlive its file");
}

/// Copy-repacking: the survivor moves to a new pack, the dead frame's row goes, and the old file is
/// gone only after no row names it.
#[test]
fn apply_repacks_a_half_surviving_pack_and_the_survivor_still_reads() {
    let root = TempRoot::new("apply-repack");
    let store = Store::open(root.store(), OpenMode::Create).expect("open store");
    let mut batch = store.batch();
    let survivor = batch.put(Role::Bytes, b"one").expect("put");
    let dead = batch.put(Role::Bytes, b"two").expect("put");
    batch.commit().expect("commit batch");
    let survivor_bytes = store.get(survivor).expect("survivor reads before");
    drop(store);

    let marker = marker_with(&root.store(), &[survivor]);
    let mut connection = index(&root.store());
    let decided = plan(&connection, &marker, &root.store(), 0.6).expect("plan");
    assert_eq!(decided.packs_repack.len(), 1, "{decided:?}");
    let old = decided.packs_repack[0].clone();
    assert_eq!(decided.dead_objects, vec![dead]);

    let applied = apply(&mut connection, &decided, &root.store()).expect("apply");
    assert_eq!(applied.packs_repacked, 1);

    // The old pack file is gone, and no row names it any more.
    assert!(
        !old.path.exists(),
        "the repacked-away pack must be unlinked"
    );
    let old_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM object_location WHERE location = ?1",
            [&old.pack_id],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(old_rows, 0, "no row may still point at the old pack");
    let old_pack: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pack WHERE pack_id = ?1",
            [&old.pack_id],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(old_pack, 0, "the old pack's row is gone");

    // Exactly one pack row remains, and it is the replacement.
    let pack_rows: Vec<(String, i64)> = connection
        .prepare("SELECT pack_id, objects FROM pack")
        .expect("prepare")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("collect");
    assert_eq!(pack_rows.len(), 1, "{pack_rows:?}");
    assert_ne!(pack_rows[0].0, old.pack_id, "the row names a new pack");
    assert_eq!(pack_rows[0].1, 1, "the replacement holds only the survivor");

    // The dead object's row is gone, and the survivor is byte-identical.
    let dead_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM object_location WHERE oid = ?1",
            [dead.to_string()],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(dead_rows, 0);
    let reopened = Store::open(root.store(), OpenMode::ReadWrite).expect("reopen");
    assert_eq!(
        reopened.get(survivor).expect("survivor reads after"),
        survivor_bytes,
        "repacking must move bytes, not rewrite them"
    );
    assert!(
        reopened.get(dead).is_err(),
        "the dead object must not be readable through its old coordinates"
    );
}

#[test]
fn apply_removes_orphan_packs_and_temporaries() {
    let root = TempRoot::new("apply-orphans");
    let store = Store::open(root.store(), OpenMode::Create).expect("open store");
    let keep = store.put(Role::Bytes, b"reachable").expect("put").oid;
    let orphan = root.store().join("packs").join("orphan.pack");
    std::fs::write(&orphan, b"nobody claims this").expect("write orphan");
    let temporary = root.store().join("tmp").join("half-written.pack");
    std::fs::write(&temporary, b"half").expect("write temp");

    let marker = marker_with(&root.store(), &[keep]);
    let mut connection = index(&root.store());
    let decided = plan(&connection, &marker, &root.store(), 0.5).expect("plan");
    assert_eq!(decided.orphan_pack_files, vec![orphan.clone()]);

    let applied = apply(&mut connection, &decided, &root.store()).expect("apply");
    assert_eq!(applied.orphan_pack_files_deleted, 1);
    assert_eq!(applied.temporaries_deleted, 1);
    assert!(!orphan.exists(), "the orphan pack is gone");
    assert!(!temporary.exists(), "tmp/ holds nothing worth keeping");
}

/// `{(relative path, length)}`, `-shm` excluded (ADR-008 §6.1).
fn fingerprint(dir: &std::path::Path) -> Vec<(String, u64)> {
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
                // The marker is the collector's own scratch: reading it is allowed to grow its file,
                // and its `-wal` is created and checkpointed at SQLite's discretion. Excluding it (and
                // its companions) is what makes "the plan changed nothing" a statement about the
                // *store*, which is what it is meant to be.
                let is_marker = relative.starts_with(fs_workspace::gc::Marker::FILE_NAME);
                if !relative.ends_with("-shm") && !is_marker {
                    out.push((relative, meta.len()));
                }
            }
        }
    }
    out.sort();
    out
}
