//! `HandleTable`'s bindings: where an open entry currently lives (P3-4 step 0a).
//!
//! The table needs `(parent, name)` because two-phase deletion ends with `remove_at(parent, name, expected_id)`, which is a
//! **name** operation guarded by an id. Three properties matter, and each fails differently:
//!
//! - the binding belongs to the **FileId** (two handles on one file must not disagree after a rename);
//! - a rename **updates** it (a stale name makes the guard refuse, which surfaces as "the file will not delete" — the
//!   hardest failure to read back from an NTSTATUS);
//! - an unrecorded id returns `None` rather than a guess (a guessed name would name another object).

use fs_windows::share::Mask;
use fs_winfsp::{HandleId, HandleTable};

fn open(table: &mut HandleTable, file_id: u64, parent: u64, name: &[u8]) -> HandleId {
    let handle = table
        .open(file_id, Mask::ALL_BITS, Mask::ALL_BITS, false)
        .expect("open");
    table.remember_binding(file_id, parent, name.to_vec());
    handle
}

/// Two handles on the same entry share **one** binding, and a rename moves it for both.
#[test]
fn the_binding_belongs_to_the_file_id_not_the_handle() {
    let mut table = HandleTable::new();
    let first = open(&mut table, 7, 1, b"a");
    let second = open(&mut table, 7, 1, b"a");
    assert_ne!(
        first, second,
        "cell setup: two distinct handles on one entry"
    );
    assert_eq!(
        table.binding_of(7),
        Some((1, b"a".to_vec())),
        "cell setup: one binding for the entry, recorded by the second open as well"
    );

    // Move the entry once; both handles see the same new location because the binding is per id.
    table.rebind(7, 9, b"b".to_vec());
    assert_eq!(
        table.binding_of(7),
        Some((9, b"b".to_vec())),
        "cell rename: the binding followed the entry, and there is only one of them to follow"
    );
    assert!(
        table.get(first).is_some() && table.get(second).is_some(),
        "cell handles: the handles themselves are untouched by the move"
    );
}

/// A rename updates the binding to the **current** name — not the one recorded at open.
#[test]
fn a_rename_updates_the_recorded_name() {
    let mut table = HandleTable::new();
    open(&mut table, 7, 1, b"a");
    assert_eq!(table.binding_of(7), Some((1, b"a".to_vec())), "cell setup");

    table.rebind(7, 1, b"b".to_vec());
    assert_eq!(
        table.binding_of(7),
        Some((1, b"b".to_vec())),
        "cell after rename: the recorded name is the current one, so `remove_at` can pass the guard"
    );
}

/// **Rename to the same name** (`x -> x`, or a case-only rename): the recorded name must be the current name, and the
/// assertion must not depend on whether the backend short-circuited the rename.
///
/// This is what makes the test about **state** rather than about a path: the original binding *is* the current name, so
/// re-recording it is a no-op and the assertion holds either way.
#[test]
fn a_rename_to_the_same_name_leaves_the_binding_correct() {
    let mut table = HandleTable::new();
    open(&mut table, 7, 1, b"same");
    table.rebind(7, 1, b"same".to_vec());
    assert_eq!(
        table.binding_of(7),
        Some((1, b"same".to_vec())),
        "cell identity rename: the binding still names the entry — it did not move, and it did not vanish"
    );
}

/// An id nobody recorded has **no** binding: guessing would name another object.
#[test]
fn an_unrecorded_id_has_no_binding() {
    let table = HandleTable::new();
    assert_eq!(
        table.binding_of(42),
        None,
        "cell unknown: `None`, not a guess — a guessed name would point at whatever occupies it and the guard would then \
         refuse the removal, which reads as `NotFound` (\"cannot delete\") instead of \"wrong handle\""
    );
}

/// Re-binding an id the table never saw does **not** create a binding: the table records location only for entries it is
/// actually holding, otherwise a rename of an unopened path would invent state.
#[test]
fn rebinding_an_unknown_id_creates_nothing() {
    let mut table = HandleTable::new();
    table.rebind(99, 1, b"ghost".to_vec());
    assert_eq!(
        table.binding_of(99),
        None,
        "cell no-op: `rebind` updates an existing binding, it does not fabricate one"
    );
}
