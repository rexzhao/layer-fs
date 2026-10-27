//! The mock's identity and binding behaviour (P3-3a card §10 M1/M2/M4/M6), and the path-reuse trick (§3.9).
//!
//! These tests exist before the adapter does, because the mock is what the adapter's contract tests will run against:
//! if the mock were a stub that "remembers the last path", the sharpest criterion in the card (§3.9: reuse the old path
//! for a *different* object, then act on the old id) would pass trivially. So the mock is held to the same standard as
//! the core it stands in for.

use fs_semantics::disposition::Disposition;
use fs_winfsp::ops::Path;
use fs_winfsp::{MockBackend, OpsError, WorkspaceOps, ROOT};

fn create(backend: &MockBackend, path: &[u8]) -> u64 {
    backend
        .create_or_open(&Path::new(path), Disposition::Create)
        .expect("create")
        .file_id
}

/// M1 — identity is allocated on create, **stable across rename**, and **never reused**.
///
/// The "never reused" half is the one a counter that decrements would violate while every rename test still passed; it
/// is asserted by remove-then-create and comparing ids, not by inspecting any counter.
#[test]
fn identity_is_stable_across_rename_and_never_reused() {
    let backend = MockBackend::new();
    let original = create(&backend, b"/a");
    backend.write(original, 0, b"body").expect("write");

    let outcome = backend
        .rename(&Path::new(b"/a"), &Path::new(b"/b"), false)
        .expect("rename");
    assert_eq!(outcome.binding, (ROOT, b"b".to_vec()));
    assert_eq!(outcome.replaced, None);
    assert_eq!(
        backend.resolve(b"/b"),
        Some(original),
        "cell rename: the id is the same one, reached by the new name"
    );
    assert_eq!(
        backend.read(original, 0, 4).expect("read"),
        b"body",
        "cell rename: the old id still reads the same bytes"
    );

    backend
        .remove_at(ROOT, b"b", original)
        .expect("remove the renamed entry");
    assert!(!backend.is_live(original), "the id is retired");
    let reused = create(&backend, b"/b");
    assert_ne!(
        reused, original,
        "cell reuse: a fresh create must not hand back a retired id"
    );
}

/// **The volume folds names and keeps the bytes the caller wrote** — the two halves of `CasePreservedNames`.
///
/// This replaces `bindings_are_raw_bytes_so_a_and_upper_a_coexist`, which pinned the *previous* semantics (raw byte keys, so `/A` and `/a` were two
/// entries). Those semantics contradicted the volume's own declaration (`CaseSensitiveSearch = 0`) and are exactly what the mounted volume got wrong:
/// Windows renames and deletes by an **upcased** name, and a byte-keyed store answered "not found" (`rename "/L2-FILE" -> "/l2-renamed"` on a file
/// created as `/l2-file`). So this is not a design change — it is the declaration and the behaviour being made to agree.
#[test]
fn the_fold_makes_a_and_upper_a_one_entry_that_keeps_its_display_bytes() {
    let backend = MockBackend::new();
    let upper = create(&backend, b"/A");
    // Creating the other spelling is a **collision**: after folding it is the same name.
    let collision = backend.create_or_open(&Path::new(b"/a"), Disposition::Create);
    assert!(
        matches!(collision, Err(OpsError::NameCollision)),
        "cell collision: `/a` and `/A` are one name after folding (got {collision:?})"
    );
    assert_eq!(backend.resolve(b"/A"), Some(upper), "cell upper: resolves");
    assert_eq!(
        backend.resolve(b"/a"),
        Some(upper),
        "cell lower: the same entry through the fold"
    );
    assert_eq!(
        backend.raw_names(ROOT),
        vec![b"A".to_vec()],
        "cell names: one binding, displayed with the bytes the caller wrote"
    );

    // A case-only rename changes the **display bytes** and keeps the **id** — three semantics in one assertion (manager 442): the fold matches,
    // a case-only rename is not a collision, and the identity survives.
    backend
        .rename(&Path::new(b"/A"), &Path::new(b"/a"), true)
        .expect("case-only rename");
    assert_eq!(backend.resolve(b"/a"), Some(upper), "cell rename: same id");
    assert_eq!(
        backend.raw_names(ROOT),
        vec![b"a".to_vec()],
        "cell rename: the display follows the most recent spelling"
    );
}

/// M4 — SUPERSEDE **keeps the id** and drops the content (ADR-009 §4.2 ruling (a)).
///
/// The old semantics (retire the name, keep the old inode readable through its handle) described what a *replacement*
/// would do; a real system sends SUPERSEDE as an overwrite of the same file object, so a handle opened before it reads
/// the **overwritten** content. This test is the mock's half of that: same id, size 0, replaced `None`.
#[test]
fn supersede_truncates_in_place_and_keeps_the_id() {
    let backend = MockBackend::new();
    let id = create(&backend, b"/f");
    backend.write(id, 0, b"old").expect("write");

    let outcome = backend
        .create_or_open(&Path::new(b"/f"), Disposition::Supersede)
        .expect("supersede");
    assert_eq!(
        outcome.file_id, id,
        "cell identity: the entry keeps its FileId — nothing is replaced"
    );
    assert_eq!(
        outcome.replaced, None,
        "cell replaced: no inode lost the name, so there is nothing to report"
    );
    assert_eq!(outcome.size, 0, "cell size: the content is gone");
    assert_eq!(
        backend.resolve(b"/f"),
        Some(id),
        "cell name: still the same entry"
    );
    assert_eq!(
        backend.read(id, 0, 3).expect("read after supersede"),
        vec![0u8; 3],
        "cell content: a handle on the old id now reads the **overwritten** bytes (zeros), not the old ones"
    );
}

/// §3.9 — path reuse: after `/a` -> `/b`, a **new** `/a` is a different object; the old id with the *current* name must
/// not delete it.
///
/// Two assertions, in opposite directions: the guarded removal succeeds on `/b`, and `/a` still holds the **new** object.
/// A name-based implementation fails the first; an unguarded one fails the second.
#[test]
fn the_old_id_cannot_delete_the_object_that_reused_its_path() {
    let backend = MockBackend::new();
    let old = create(&backend, b"/a");
    backend
        .rename(&Path::new(b"/a"), &Path::new(b"/b"), false)
        .expect("rename");
    let new = create(&backend, b"/a");
    assert_ne!(
        old, new,
        "cell setup: the reused path names a different object"
    );

    assert_eq!(
        backend.remove_at(ROOT, b"b", old),
        Ok(()),
        "cell guarded: the old id with the current name removes the moved entry"
    );
    assert!(
        backend.is_live(new),
        "cell survivor: the object that reused the path is untouched"
    );
    assert_eq!(
        backend.resolve(b"/a"),
        Some(new),
        "cell survivor: /a still names the new object"
    );

    // Counter-proof for the guard itself: the *new* id against the now-free name `/b` must fail, and nothing may change.
    let before = backend.calls().len();
    assert_eq!(
        backend.remove_at(ROOT, b"b", new),
        Err(OpsError::NotFound),
        "cell guard: a free name is NotFound, not a silent success"
    );
    assert!(
        backend.is_live(new),
        "cell guard: a refused removal deletes nothing"
    );
    assert_eq!(
        backend.calls().len(),
        before + 1,
        "cell guard: the refusal is recorded as one call (it did reach the backend)"
    );
}

/// M6's three cases of `remove_at`, matching the core's **three cases** (ADR-009's ruling): another inode under the name
/// is refused, an already-retired id is an idempotent `Ok`, a still-live id whose name is gone is `NotFound`.
///
/// The error **type** is folded, not identical: the core returns `InvalidRecord` for the first case and this returns
/// `NotFound`, because `OpsError` has no `InvalidRecord`. §3.9 only requires "refused, and nothing deleted", so the
/// fold does not weaken the criterion — but it is recorded for P3-3b, where the real adapter sees the other variant.
///
/// The middle case is the one the adapter depends on: NT sends `Cleanup` and `Close` on the same handle, so the second
/// removal must not surface as an error. Asserting only "the entry is gone" would pass for all three implementations.
#[test]
fn remove_at_classifies_three_ways() {
    let backend = MockBackend::new();
    let file = create(&backend, b"/f");
    let stranger = create(&backend, b"/g");

    // A **stale identity**, not a missing name: the core reports `InvalidRecord` here for the same reason, and the two
    // map to different NTSTATUS values.
    assert!(
        matches!(
            backend.remove_at(ROOT, b"f", stranger),
            Err(OpsError::InvalidRecord(_))
        ),
        "cell 1: the name belongs to another inode — a stale handle, not a missing entry"
    );
    assert!(
        backend.is_live(file),
        "cell 1: refused, so nothing was deleted"
    );

    assert_eq!(
        backend.remove_at(ROOT, b"f", file),
        Ok(()),
        "cell 2 setup: the first removal succeeds"
    );
    assert!(!backend.is_live(file), "cell 2 setup: the inode is retired");
    assert_eq!(
        backend.remove_at(ROOT, b"f", file),
        Ok(()),
        "cell 2: a repeated removal of an already-retired id is an idempotent Ok, not an error"
    );

    let moved = create(&backend, b"/h");
    assert_eq!(backend.remove_at(ROOT, b"h", moved), Ok(()), "cell 3 setup");
    // Now the name is free but the id was live a moment ago; that is still the idempotent case, which is why the live
    // variant needs a *different* setup: an inode that was moved away while its old name stayed free.
    backend
        .write(moved, 0, b"x")
        .expect_err("a retired id is not writable");
    let live = create(&backend, b"/live");
    backend
        .rename(&Path::new(b"/live"), &Path::new(b"/elsewhere"), false)
        .expect("rename");
    assert_eq!(
        backend.remove_at(ROOT, b"live", live),
        Err(OpsError::NotFound),
        "cell 3: the name is free but the id is still live (it moved), so this is NotFound"
    );
    assert!(backend.is_live(live), "cell 3: the moved inode survives");
}

/// §3.9 applied to `read_dir`: enumerating a directory **by id** keeps returning *that* directory's entries after
/// another directory takes over its old path.
///
/// `d1` has two entries and `d2` has three, so the counts distinguish "enumerated the right directory" from "enumerated
/// whatever is at the path now" — an empty-vs-empty comparison would not.
#[test]
fn read_dir_by_id_survives_another_directory_taking_the_old_path() {
    let backend = MockBackend::new();
    let d1 = backend.mkdir(&Path::new(b"/d1")).expect("mkdir d1");
    backend.mkdir(&Path::new(b"/d2")).expect("mkdir d2");
    create(&backend, b"/d1/one");
    create(&backend, b"/d1/two");
    create(&backend, b"/d2/a");
    create(&backend, b"/d2/b");
    create(&backend, b"/d2/c");
    assert_eq!(
        backend.read_dir(d1).expect("read_dir").len(),
        2,
        "cell setup"
    );

    backend
        .rename(&Path::new(b"/d1"), &Path::new(b"/d1-renamed"), false)
        .expect("rename d1");
    backend
        .rename(&Path::new(b"/d2"), &Path::new(b"/d1"), false)
        .expect("rename d2 onto d1's old path");
    assert_eq!(
        backend.read_dir(d1).expect("read_dir by id").len(),
        2,
        "cell id: the old id still enumerates d1's two entries, not the three that now live at /d1"
    );
    let now = backend.resolve(b"/d1").expect("the reused path");
    assert_ne!(
        now, d1,
        "cell setup: the reused path names a different directory"
    );
    assert_eq!(
        backend.read_dir(now).expect("read_dir").len(),
        3,
        "cell control: the directory that took the path has three entries"
    );
}

/// `set_attributes` refuses the **derived** bits (`DIRECTORY 0x10`, `NORMAL 0x80`): they are outside the stored value
/// domain, so "set DIRECTORY on a file" must not look like it worked.
#[test]
fn set_attributes_refuses_derived_bits() {
    let backend = MockBackend::new();
    let file = create(&backend, b"/f");
    for derived in [0x10u32, 0x80, 0x10 | 0x01] {
        assert_eq!(
            backend.set_attributes(file, fs_winfsp::Attributes(derived)),
            Err(OpsError::InvalidParameter),
            "cell {derived:#x}: a derived bit is refused, not silently dropped"
        );
    }
    // Control: the writable bits really are accepted, so the refusal above is about the bit, not about the call.
    backend
        .set_attributes(file, fs_winfsp::Attributes(0x01 | 0x20))
        .expect("cell control: writable bits are accepted");
    assert_eq!(
        backend.get_info(file).expect("info").attributes,
        fs_winfsp::Attributes(0x01 | 0x20),
        "cell control: and the stored value is what comes back"
    );
}

/// **The two-way attribute rule** (`ReplaceFileAttributes`), which is the only difference left between the dispositions
/// that share the truncate action (ADR-009 §4.2 ruling (a)).
///
/// SUPERSEDE **replaces** the stored attributes wholesale; OVERWRITE / OVERWRITE_IF **keep** them. Two assertions in
/// opposite directions, because a backend that always replaced (or never did) satisfies either one alone:
///
/// | disposition | content | stored attributes |
/// | --- | --- | --- |
/// | SUPERSEDE | dropped | back to nothing set |
/// | OVERWRITE / OVERWRITE_IF | dropped | unchanged |
#[test]
fn supersede_replaces_attributes_while_overwrite_keeps_them() {
    for (disposition, expect_replaced) in [
        (Disposition::Supersede, true),
        (Disposition::Overwrite, false),
        (Disposition::OverwriteIf, false),
    ] {
        let backend = MockBackend::new();
        let id = create(&backend, b"/f");
        backend.write(id, 0, b"body").expect("write");
        backend
            .set_attributes(id, fs_winfsp::Attributes(0x01 | 0x20))
            .expect("set attributes");

        let outcome = backend
            .create_or_open(&Path::new(b"/f"), disposition)
            .expect("truncating disposition");
        assert_eq!(outcome.file_id, id, "cell {disposition:?}: identity kept");
        assert_eq!(outcome.size, 0, "cell {disposition:?}: content dropped");
        let attributes = backend.get_info(id).expect("info").attributes.0;
        if expect_replaced {
            assert_eq!(
                attributes, 0,
                "cell {disposition:?}: SUPERSEDE replaces the attributes (ReplaceFileAttributes = TRUE)"
            );
        } else {
            assert_eq!(
                attributes,
                0x01 | 0x20,
                "cell {disposition:?}: OVERWRITE keeps them (ReplaceFileAttributes = FALSE) — the content is dropped, \
                 the metadata is not"
            );
        }
    }
}

/// **The root is a path, not an exception**: `"/"` resolves, opens as a directory, and lists.
///
/// This is the entry-point fix for a smell the mounted L2 run exposed: `create_or_open` carried a root special case and `open_dir` did not, so
/// `stat` on the volume root failed while every other operation on the same path worked. Two special cases were the symptom; refusing `"/"` in the
/// path splitter was the cause.
#[test]
fn the_root_resolves_like_any_other_directory() {
    let backend = MockBackend::new();
    assert_eq!(
        backend.resolve(b"/"),
        Some(ROOT),
        "cell resolve: the root has an id like every other entry"
    );
    assert_eq!(
        backend.open_dir(&Path::new(b"/")),
        Ok(ROOT),
        "cell open_dir: and it opens as a directory — no special case needed now"
    );
    assert_eq!(
        backend
            .get_info(ROOT)
            .map(|info| info.kind)
            .expect("root info"),
        fs_winfsp::Kind::Directory,
        "cell kind: it reports the kind a `stat` needs"
    );
    assert!(
        backend.read_dir(ROOT).is_ok(),
        "cell list: and it lists (empty, since nothing was created)"
    );
}
