//! The P3-3a contract scenarios, as **library code** (P3-3b moved them out of `tests/`).
//!
//! They were written generic over `WorkspaceOps` so that a real backend could run the same functions; keeping them in a
//! `tests/` file made that impossible across crates. Nothing about them changed in the move — the instantiation for the
//! mock still lives in `tests/contract.rs`, and the real backend instantiates them from `fs-api`.

use crate::{OpsError, WorkspaceOps, ROOT};
use fs_semantics::disposition::Disposition;

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

/// `SUPERSEDE`: the entry keeps its id and is truncated in place (ADR-009 §4.2 ruling (a)).
pub fn scenario_supersede_truncates_in_place<B: WorkspaceOps>(backend: &B) {
    let id = create(backend, b"/f");
    backend.write(id, 0, b"original").expect("write");
    let outcome = backend
        .create_or_open(b"/f", Disposition::Supersede)
        .expect("supersede");
    assert_eq!(
        outcome.replaced, None,
        "cell replaced: nothing was replaced"
    );
    assert_eq!(
        outcome.file_id, id,
        "cell identity: the same entry — a handle opened before it stays valid and now reads the new content"
    );
    assert_eq!(outcome.size, 0, "cell new size: the content is gone");
    assert_eq!(
        backend.read(id, 0, 8).expect("read"),
        vec![0u8; 8],
        "cell content: reading the (only) id yields the overwritten bytes, not the old ones"
    );
}

/// The three removal cases, in the order the adapter meets them.
pub fn scenario_removal_cases<B: WorkspaceOps>(backend: &B) {
    let file = create(backend, b"/a");
    let stranger = create(backend, b"/b");
    assert!(
        matches!(
            backend.remove_at(ROOT, b"a", stranger),
            Err(OpsError::InvalidRecord(_))
        ),
        "cell 1: another inode's name is refused — the guard reports a stale identity, not a missing entry"
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
