//! # Criterion 3 is split: the core half lives here, the adapter half lives in P3-4
//!
//! The card's two-phase deletion has two halves and they belong to different layers:
//!
//! - **core half (this file, `scenario_removal_cases`)**: `remove_at` refuses another inode's name with a *stale identity*
//!   error, is idempotent for an already-retired id, and reports `NotFound` when the name is free but the id is still
//!   live. A backend can be held to all three without any handle bookkeeping.
//! - **adapter half (P3-4)**: `set_delete(true)` making a *new* open fail with `DeletePending` while the handles opened
//!   before it keep working. The core has no notion of "handle", so this half cannot be asserted here — it needs the
//!   adapter's handle table, which arrives with the FFI layer.
//!
//! Written down because the absence of those assertions looks like an omission otherwise: the scenario runs on both
//! backends and passes, and a reader would reasonably ask "where is the `DeletePending` case?".
//!
//! The P3-3a contract scenarios, as **library code** (P3-3b moved them out of `tests/`).
//!
//! They were written generic over `WorkspaceOps` so that a real backend could run the same functions; keeping them in a
//! `tests/` file made that impossible across crates. Nothing about them changed in the move — the instantiation for the
//! mock still lives in `tests/contract.rs`, and the real backend instantiates them from `fs-api`.

use crate::ops::Path;
use crate::{OpsError, WorkspaceOps, ROOT};

/// Wraps literal bytes for calls that now take a [`Path`].
fn p(bytes: &[u8]) -> Path<'_> {
    Path::new(bytes)
}
use fs_semantics::disposition::Disposition;

fn create<B: WorkspaceOps>(backend: &B, path: &[u8]) -> u64 {
    backend
        .create_or_open(&p(path), Disposition::Create)
        .expect("create")
        .file_id
}

/// Identity across rename: the id keeps working, the old name is gone, the new name resolves to the same object.
pub fn scenario_identity_across_rename<B: WorkspaceOps>(backend: &B) {
    let id = create(backend, b"/f");
    backend.write(id, 0, b"body").expect("write");
    backend.rename(&p(b"/f"), &p(b"/g"), false).expect("rename");
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
        .create_or_open(&p(b"/f"), Disposition::Supersede)
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
    backend.rename(&p(b"/c"), &p(b"/d"), false).expect("rename");
    assert_eq!(
        backend.remove_at(ROOT, b"c", moved),
        Err(OpsError::NotFound),
        "cell 3: the name is free but the id is still live"
    );
}

/// Enumeration by id survives another directory taking over the old path.
pub fn scenario_read_dir_by_id<B: WorkspaceOps>(backend: &B) {
    let d1 = backend.mkdir(&p(b"/d1")).expect("mkdir");
    backend.mkdir(&p(b"/d2")).expect("mkdir");
    create(backend, b"/d1/x");
    create(backend, b"/d1/y");
    create(backend, b"/d2/p");
    backend
        .rename(&p(b"/d1"), &p(b"/d1-moved"), false)
        .expect("rename d1");
    backend
        .rename(&p(b"/d2"), &p(b"/d1"), false)
        .expect("rename d2");
    let entries = backend.read_dir(d1).expect("read_dir by id");
    let names: Vec<Vec<u8>> = entries.into_iter().map(|entry| entry.name).collect();
    assert_eq!(
        names,
        vec![b"x".to_vec(), b"y".to_vec()],
        "cell by id: the old id still enumerates its own two entries"
    );
}

/// A directory with children **cannot** be removed — and the refusal must not take the children with it.
///
/// Measured defect (L2 class 06): `rmdir` on a non-empty directory **succeeded** on the mounted volume. That is not a wrong status code, it is
/// **data loss**, so the criterion asserts more than the error: the children are still there afterwards, and still listable.
pub fn scenario_remove_non_empty_directory<B: WorkspaceOps>(backend: &B) {
    let dir = backend.mkdir(&p(b"/d")).expect("mkdir d");
    let child = backend
        .create_or_open(&p(b"/d/a"), fs_semantics::disposition::Disposition::Create)
        .expect("create /d/a");

    let refused = backend.remove_at(0, b"d", dir);
    assert_eq!(
        refused,
        Err(OpsError::NotEmpty),
        "cell refusal: a non-empty directory answers `NotEmpty` — the same rule the core applies"
    );
    // The name is still bound, the child is still reachable, and the listing still shows it: **nothing was deleted**.
    assert!(
        backend.get_info(dir).is_ok(),
        "cell directory: it is still there after the refused removal"
    );
    assert_eq!(
        backend.get_info(child.file_id).map(|info| info.kind),
        Ok(crate::Kind::File),
        "cell child: and so is the entry inside it — a refusal must not be a partial deletion"
    );
    let listing = backend.read_dir(dir).expect("list d");
    assert_eq!(
        listing.len(),
        1,
        "cell listing: one entry remains ({:?})",
        listing
            .iter()
            .map(|entry| String::from_utf8_lossy(&entry.name).into_owned())
            .collect::<Vec<_>>()
    );

    // And the empty case still works, so the rule is "non-empty", not "directories".
    backend
        .remove_at(1, b"a", child.file_id)
        .expect("remove the child");
    backend
        .remove_at(0, b"d", dir)
        .expect("now the directory is empty and goes away");
}

/// **Case-insensitive names, asked of both backends** (472/480/481).
///
/// This is the scenario that makes "the mock and the real backend answer the same question the same way" a reading rather than a hope. Both had
/// their own story about folding: the mock folded inside its own `split`, the real backend did not fold at all, and a criterion that passed on one
/// therefore said nothing about the other.
///
/// Four claims, each with the negative next to it:
///
/// 1. an existing name answers under **any** spelling — and a different name still does not;
/// 2. a case-only rename keeps the identity, replaces nothing;
/// 3. the listing shows **exactly one** entry, under the spelling that was asked for last (display is kept, not folded);
/// 4. creating the other spelling is a **collision** — the two spellings cannot become two entries.
pub fn scenario_case_insensitive_names<B: WorkspaceOps>(backend: &B) {
    let id = create(backend, b"/ReadMe.txt");
    backend.write(id, 0, b"body").expect("write");

    // 1. Folding is for matching, not for inventing.
    let folded = backend
        .create_or_open(&p(b"/readme.txt"), Disposition::Open)
        .expect("the other spelling resolves to the existing entry");
    assert_eq!(folded.file_id, id, "cell 1: same entry under another case");
    assert!(
        matches!(
            backend.create_or_open(&p(b"/other.txt"), Disposition::Open),
            Err(OpsError::NotFound)
        ),
        "cell 1 negative: a different name is still missing"
    );

    // 2. Case-only rename: same object, nothing replaced.
    let outcome = backend
        .rename(&p(b"/ReadMe.txt"), &p(b"/readme.txt"), true)
        .expect("case-only rename");
    assert_eq!(outcome.file_id, id, "cell 2: identity kept");
    assert_eq!(outcome.replaced, None, "cell 2: nothing lost the name");

    // 3. One entry, and its display bytes are the ones asked for last.
    let entries = backend.read_dir(ROOT).expect("list");
    assert_eq!(entries.len(), 1, "cell 3: exactly one entry");
    assert_eq!(
        entries[0].name, b"readme.txt",
        "cell 3: display follows the rename"
    );

    // 4. The two spellings cannot both exist.
    assert!(
        matches!(
            backend.create_or_open(&p(b"/README.TXT"), Disposition::Create),
            Err(OpsError::NameCollision)
        ),
        "cell 4: creating the other spelling is a collision"
    );

    // The bytes survived every step — the identity half of "same entry".
    assert_eq!(backend.read(id, 0, 4).expect("read"), b"body");
}
