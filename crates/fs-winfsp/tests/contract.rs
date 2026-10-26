//! The §3 scenarios, written **once** and generic over the backend (P3-3a card §3 criterion 7a).
//!
//! Each `scenario_*` function takes `&B where B: WorkspaceOps`, so P3-3b can run the same functions against the real
//! adapter without editing them. The card's mechanical check (`criterion_7a_shape.rs`) asserts that every scenario is
//! instantiated **exactly once**, with the mock — a second instantiation that is *not* the real backend would mean the
//! suite is pretending to cover two backends.
//!
//! The scenarios here are the trait-level ones: identity across rename, `SUPERSEDE`, the three removal cases, and
//! directory enumeration by id. Handle/share/delete-pending behaviour is adapter policy and lives in
//! `handles_criteria.rs`, which is why it is not duplicated as a backend-generic scenario.

use fs_semantics::disposition::Disposition;
use fs_winfsp::{MockBackend, OpsError, WorkspaceOps, ROOT};

fn create<B: WorkspaceOps>(backend: &B, path: &[u8]) -> u64 {
    backend
        .create_or_open(path, Disposition::Create)
        .expect("create")
        .file_id
}

/// Identity across rename: the id keeps working, the old name is gone, the new name resolves to the same object.
pub fn scenario_identity_across_rename<B: WorkspaceOps>(backend: &B) {
    let id = create(backend, b"/f");
    backend.write(id, 0, b"body").expect("write");
    backend.rename(b"/f", b"/g", false).expect("rename");
    assert_eq!(
        backend.read(id, 0, 4).expect("read by id"),
        b"body",
        "cell id: the id still reads its bytes after the rename"
    );
    assert_eq!(
        backend
            .read_dir(ROOT)
            .expect("read_dir")
            .iter()
            .filter(|entry| entry.name == b"f")
            .count(),
        0,
        "cell old name: the old name no longer appears in the root listing"
    );
    assert_eq!(backend.get_info(id).expect("info").file_id, id);
}

/// `SUPERSEDE`: the name moves to a fresh id, the old object stays readable and keeps its size.
pub fn scenario_supersede_keeps_old_object<B: WorkspaceOps>(backend: &B) {
    let old = create(backend, b"/f");
    backend.write(old, 0, b"original").expect("write");
    let outcome = backend
        .create_or_open(b"/f", Disposition::Supersede)
        .expect("supersede");
    assert_eq!(outcome.replaced, Some(old), "cell replaced");
    assert_ne!(outcome.file_id, old, "cell new id");
    assert_eq!(outcome.size, 0, "cell new size");
    assert_eq!(
        backend.get_info(old).expect("old info").size,
        8,
        "cell old size: a truncate-in-place implementation would report 0 here"
    );
    assert_eq!(
        backend.read(old, 0, 8).expect("old read"),
        b"original",
        "cell old content"
    );
}

/// The three removal cases, in the order the adapter meets them.
pub fn scenario_removal_cases<B: WorkspaceOps>(backend: &B) {
    let file = create(backend, b"/a");
    let stranger = create(backend, b"/b");
    assert_eq!(
        backend.remove_at(ROOT, b"a", stranger),
        Err(OpsError::NotFound),
        "cell 1: another inode's name is refused"
    );
    assert_eq!(
        backend.remove_at(ROOT, b"a", file),
        Ok(()),
        "cell 2: the removal"
    );
    assert_eq!(
        backend.remove_at(ROOT, b"a", file),
        Ok(()),
        "cell 2b: a repeated removal is idempotent"
    );
    let moved = create(backend, b"/c");
    backend.rename(b"/c", b"/d", false).expect("rename");
    assert_eq!(
        backend.remove_at(ROOT, b"c", moved),
        Err(OpsError::NotFound),
        "cell 3: the name is free but the id is still live"
    );
}

/// Enumeration by id survives another directory taking over the old path.
pub fn scenario_read_dir_by_id<B: WorkspaceOps>(backend: &B) {
    let d1 = backend.mkdir(b"/d1").expect("mkdir");
    backend.mkdir(b"/d2").expect("mkdir");
    create(backend, b"/d1/x");
    create(backend, b"/d1/y");
    create(backend, b"/d2/p");
    backend
        .rename(b"/d1", b"/d1-moved", false)
        .expect("rename d1");
    backend.rename(b"/d2", b"/d1", false).expect("rename d2");
    let entries = backend.read_dir(d1).expect("read_dir by id");
    let names: Vec<Vec<u8>> = entries.into_iter().map(|entry| entry.name).collect();
    assert_eq!(
        names,
        vec![b"x".to_vec(), b"y".to_vec()],
        "cell by id: the old id still enumerates its own two entries"
    );
}

/// The only instantiation, and it is the mock (the card's 7a rule).
#[test]
fn the_mock_satisfies_the_trait_contract() {
    let backend = MockBackend::new();
    scenario_identity_across_rename::<MockBackend>(&backend);
    let backend = MockBackend::new();
    scenario_supersede_keeps_old_object::<MockBackend>(&backend);
    let backend = MockBackend::new();
    scenario_removal_cases::<MockBackend>(&backend);
    let backend = MockBackend::new();
    scenario_read_dir_by_id::<MockBackend>(&backend);
}
