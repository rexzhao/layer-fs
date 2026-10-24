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
    let decided = plan(&index(&root.store()), &marker, &root.store(), 0.5).expect("plan");
    assert_eq!(
        decided.missing_packs.len(),
        1,
        "K8a: the row's file is gone"
    );
    let after = index(&root.store())
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
