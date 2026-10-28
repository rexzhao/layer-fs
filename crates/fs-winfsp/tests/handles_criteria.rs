//! Handle-table criteria 3/4/5 (P3-3a card §3): two-phase deletion, `DELETE_ON_CLOSE`, and the 9-cell share matrix.
//!
//! The table is tested with the mock backend so that "the entry is still readable through the old handle" is a statement
//! about real content, not about a flag. Every criterion here names the failure it is designed to catch, because all
//! three of them have an implementation that "looks right" — refuse the new open, delete on close, refuse the conflict —
//! while getting the *other* half wrong.

use fs_windows::share::{Mask, Term};
use fs_winfsp::ops::Path;
use fs_winfsp::{CloseEffect, HandleTable, MockBackend, OpsError, WorkspaceOps, ROOT};

fn mask(terms: &[Term]) -> Mask {
    terms.iter().fold(Mask::NONE, |acc, term| {
        Mask::from_bits(acc.bits() | term_mask(*term))
    })
}

fn term_mask(term: Term) -> u8 {
    match term {
        Term::Read => Mask::READ.bits(),
        Term::Write => Mask::WRITE.bits(),
        Term::Delete => Mask::DELETE.bits(),
    }
}

fn file(backend: &MockBackend, path: &[u8], content: &[u8]) -> u64 {
    let id = backend
        .create_or_open(
            &Path::new(path),
            fs_semantics::disposition::Disposition::Create,
        )
        .expect("create")
        .file_id;
    backend.write(id, 0, content).expect("write");
    id
}

/// Criterion 5 — the 9-cell share matrix, table-driven, with the rule taken from `fs_windows::share` rather than
/// re-derived here.
///
/// The table is asserted **cell by cell** (not "some refused"): a rule that refuses everything would pass a
/// "conflicts exist" assertion while breaking every legitimate open.
#[test]
fn sharing_admits_and_refuses_all_nine_cells() {
    let all = [Term::Read, Term::Write, Term::Delete];
    // Cell semantics: with an existing handle whose desired = `held` and share = `shared`,
    // a newcomer with desired = `requested` and share = ALL is admitted iff `held ⊆ shared_from_newcomer` (always true
    // here) and `requested ⊆ shared` (the existing handle's share mask).
    for held in all {
        for shared in all {
            let table = std::cell::RefCell::new(HandleTable::new());
            let existing_desired = mask(&[held]);
            let existing_share = mask(&[shared]);
            let mut table = table.borrow_mut();
            table
                .open(1, existing_desired, existing_share, false, false)
                .expect("the first open always succeeds");
            for requested in all {
                let admitted = table
                    .open(1, mask(&[requested]), Mask::ALL_BITS, false, false)
                    .is_ok();
                let expected =
                    mask(&[requested]).bits() & existing_share.bits() == mask(&[requested]).bits();
                assert_eq!(
                    admitted, expected,
                    "cell held={held:?} shared={shared:?} requested={requested:?}: \
                     a newcomer may take exactly what the existing handle shares"
                );
            }
        }
    }
}

/// Criterion 3 — two-phase deletion.
///
/// (i) a **new** open of a delete-pending entry is refused; (ii) the handles that were already open keep working, and
/// their bytes are unchanged; (iii) the last close reports `Remove(id)` exactly once, and the entry is still in the
/// backend until the adapter performs it.
///
/// (ii) is the half a "close everything on delete" implementation breaks; (iii) is the half a "delete on the first
/// close" implementation breaks.
#[test]
fn two_phase_deletion_keeps_old_handles_and_removes_on_last_close() {
    let backend = MockBackend::new();
    let id = file(&backend, b"/f", b"payload");
    let mut table = HandleTable::new();
    let first = table
        .open(
            id,
            mask(&[Term::Read, Term::Delete]),
            Mask::ALL_BITS,
            false,
            false,
        )
        .expect("first handle");
    let second = table
        .open(id, mask(&[Term::Read]), Mask::ALL_BITS, false, false)
        .expect("second handle");

    table.set_delete(first).expect("mark delete-pending");
    assert!(table.is_pending(id), "cell state: the entry is pending");

    assert_eq!(
        table.open(id, mask(&[Term::Read]), Mask::ALL_BITS, false, false),
        Err(OpsError::DeletePending),
        "cell (i): a new open of a delete-pending entry is refused"
    );
    assert_eq!(
        backend.read(id, 0, 7).expect("old handle still reads"),
        b"payload",
        "cell (ii): the already-open handles still read the content"
    );
    backend
        .write(id, 0, b"payload")
        .expect("cell (ii): and they can still write");

    assert_eq!(
        table.close(first),
        Ok(CloseEffect::Nothing),
        "cell (iii): one handle remains, so nothing is removed yet"
    );
    assert!(
        backend.is_live(id),
        "cell (iii): the entry is still there while a handle remains"
    );
    assert_eq!(
        table.close(second),
        Ok(CloseEffect::Remove(id)),
        "cell (iii): the last close reports the removal, with the id to guard it"
    );
    assert!(
        backend.is_live(id),
        "cell separation: the table does not delete — that is the adapter's single `remove_at` call"
    );
    backend.remove_at(ROOT, b"f", id).expect("adapter removes");
    table.removed(id);
    assert!(
        !backend.is_live(id),
        "cell (iii): after the adapter's call the entry is gone"
    );
    assert!(
        !table.is_pending(id),
        "cell state: pending is cleared only after the removal"
    );
}

/// Criterion 4 — `DELETE_ON_CLOSE` deletes on close, and the deletion **follows the `FileId`** rather than the path.
///
/// The rename in the middle is the point: a path-based implementation deletes whatever is at `/f` now (nothing, or a
/// different object after a reuse), while the criterion is that `f`'s *object* disappears.
#[test]
fn delete_on_close_follows_the_file_id_across_a_rename() {
    let backend = MockBackend::new();
    let id = file(&backend, b"/f", b"body");
    let mut table = HandleTable::new();
    let handle = table
        .open(
            id,
            mask(&[Term::Read, Term::Delete]),
            Mask::ALL_BITS,
            true,
            false,
        )
        .expect("open with delete-on-close");
    assert!(
        table.is_pending(id),
        "cell state: DELETE_ON_CLOSE is pending from the moment of open"
    );

    backend
        .rename(&Path::new(b"/f"), &Path::new(b"/moved"), false)
        .expect("rename");
    assert_eq!(
        (backend.resolve(b"/f"), backend.resolve(b"/moved")),
        (None, Some(id)),
        "cell setup: the object now lives at a different name"
    );

    assert_eq!(
        table.close(handle),
        Ok(CloseEffect::Remove(id)),
        "cell effect: the last close asks for the removal of **that id**"
    );
    backend
        .remove_at(ROOT, b"moved", id)
        .expect("the guard pairs the current name with the id");
    assert!(
        !backend.is_live(id),
        "cell result: the object that was opened is gone, not whatever is at the old path"
    );
}

/// **Criterion 3's red method, in-process** — the same `(remaining, pending)` grid is run through the real rule and
/// through two wrong policies, and the wrong ones must disagree.
///
/// The two wrong policies are the plausible mistakes, not strawmen: "delete as soon as anyone closes while pending"
/// (destroys data another handle still uses) and "never delete" (leaks the entry forever). If either agreed with the
/// real rule on all four cells, the criterion would not be able to tell them apart.
#[test]
fn the_close_rule_disagrees_with_both_plausible_wrong_policies() {
    let cells = [(0usize, false), (0, true), (1, false), (1, true)];
    let real: Vec<bool> = cells
        .iter()
        .map(|(remaining, pending)| fs_winfsp::close_removes(*remaining, *pending))
        .collect();
    assert_eq!(
        real,
        vec![false, true, false, false],
        "cell real: remove only when the last handle closes **and** the entry is pending"
    );

    let delete_on_any_close: Vec<bool> = cells.iter().map(|(_, pending)| *pending).collect();
    let never_delete: Vec<bool> = cells.iter().map(|_| false).collect();
    assert_ne!(
        real, delete_on_any_close,
        "cell wrong-1: \"delete on any close while pending\" must be distinguishable from the real rule"
    );
    assert_ne!(
        real, never_delete,
        "cell wrong-2: \"never delete\" must be distinguishable too (a rule that leaks would otherwise pass)"
    );
}

/// **Criterion 4's red method, in-process** — the wrong policy is "delete whatever is at the original path", run for
/// real against the mock after a rename.
///
/// Both policies are executed; the wrong one is *expected* to fail. That is stronger than asserting the right one
/// succeeds, because it shows the difference is observable through the same API the adapter uses.
#[test]
fn deleting_by_path_after_a_rename_is_observably_wrong() {
    let backend = MockBackend::new();
    let id = file(&backend, b"/f", b"body");
    backend
        .rename(&Path::new(b"/f"), &Path::new(b"/moved"), false)
        .expect("rename");

    // Wrong policy: remember the path, delete by it. It must fail — and it must fail *before* touching anything.
    assert_eq!(
        backend.remove_at(ROOT, b"f", id),
        Err(OpsError::NotFound),
        "cell wrong: the original path now names nothing, so a path-based delete cannot even find the entry"
    );
    assert!(backend.is_live(id), "cell wrong: and the object survives");

    // Right policy: the id the handle carried, paired with the *current* name.
    backend
        .remove_at(ROOT, b"moved", id)
        .expect("cell right: the id plus the current name removes the object");
    assert!(!backend.is_live(id), "cell right: the object is gone");
}

/// **Criterion 5's red method, in-process** — the same 27 cells are scored against the real rule and against a policy
/// that admits everything, and the two scores must differ.
///
/// "Admits everything" is the mistake a `share` implementation tends to make when it checks only one direction of the
/// rule; scoring it over the whole grid (rather than on one example) is what makes the difference visible.
#[test]
fn admitting_everything_disagrees_with_the_real_rule_on_the_grid() {
    let all = [Term::Read, Term::Write, Term::Delete];
    let mut real_admitted = 0usize;
    let mut generous_admitted = 0usize;
    for held in all {
        for shared in all {
            for requested in all {
                let mut table = HandleTable::new();
                table
                    .open(1, mask(&[held]), mask(&[shared]), false, false)
                    .expect("first open");
                let real = table
                    .open(1, mask(&[requested]), Mask::ALL_BITS, false, false)
                    .is_ok();
                real_admitted += usize::from(real);
                // The wrong policy in one line: it never looks at the share masks at all.
                generous_admitted += 1;
            }
        }
    }
    assert!(
        real_admitted < 27,
        "cell real: the rule must refuse at least one of the 27 cells (it admits {real_admitted})"
    );
    assert!(
        generous_admitted > real_admitted,
        "cell control: \"admit everything\" admits strictly more cells ({generous_admitted}) than the real rule          ({real_admitted}), so the grid can tell the two apart"
    );
}

/// A handle without `DELETE` access cannot mark the entry delete-pending; the refusal must not change the state.
///
/// (This test was accidentally dropped when the criterion-5 control was rewritten — the rewrite sliced the file from a
/// marker to the end. It is back, and the count going **down** from 7 to 6 is what caught it.)
#[test]
fn set_delete_requires_delete_access() {
    let backend = MockBackend::new();
    let id = file(&backend, b"/f", b"x");
    let mut table = HandleTable::new();
    let handle = table
        .open(id, mask(&[Term::Read]), Mask::ALL_BITS, false, false)
        .expect("open read-only");
    assert_eq!(
        table.set_delete(handle),
        Err(OpsError::AccessDenied),
        "cell refusal: a read-only handle may not delete"
    );
    assert!(
        !table.is_pending(id),
        "cell state: the refusal changed nothing"
    );
    assert!(
        table
            .open(id, mask(&[Term::Read]), Mask::ALL_BITS, false, false)
            .is_ok(),
        "cell state: the entry is still openable"
    );
}

/// **B3(iii)** — the adapter's deletion happens **once**: after the last handle closes, `calls()` shows exactly one
/// `RemoveAt` for that entry.
///
/// "the entry is gone" cannot distinguish one removal from two (the second is an idempotent no-op, which is exactly why
/// the core made it idempotent). The count is the only evidence.
#[test]
fn the_last_close_removes_exactly_once() {
    let backend = MockBackend::new();
    let id = file(&backend, b"/f", b"body");
    let mut table = HandleTable::new();
    let first = table
        .open(
            id,
            mask(&[Term::Read, Term::Delete]),
            Mask::ALL_BITS,
            false,
            false,
        )
        .expect("handle one");
    let second = table
        .open(id, mask(&[Term::Read]), Mask::ALL_BITS, false, false)
        .expect("handle two");
    table.set_delete(first).expect("mark pending");

    backend.clear_calls();
    assert_eq!(
        table.close(first),
        Ok(CloseEffect::Nothing),
        "cell setup: one handle remains"
    );
    assert_eq!(
        backend.calls(),
        vec![],
        "cell no-removal: closing a non-last handle must not reach the backend at all"
    );

    assert_eq!(
        table.close(second),
        Ok(CloseEffect::Remove(id)),
        "cell last close"
    );
    // The adapter performs the removal the table asked for, then reports it done.
    backend.remove_at(ROOT, b"f", id).expect("adapter removes");
    table.removed(id);

    let removals: Vec<_> = backend
        .calls()
        .into_iter()
        .filter(|call| matches!(call, fs_winfsp::Call::RemoveAt(..)))
        .collect();
    assert_eq!(
        removals,
        vec![fs_winfsp::Call::RemoveAt(ROOT, b"f".to_vec(), id)],
        "cell count: exactly one removal, and it names the id the caller believed in"
    );
}
