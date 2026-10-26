//! §3 criteria 1 and 2: a handle survives a rename, and survives a `SUPERSEDE` (P3-3a card §3).
//!
//! Both criteria are about the same property seen from two directions: **a handle names an object, not a path**. So both
//! tests are written as pairs — what the old handle sees, and what the name resolves to afterwards. Asserting only the
//! first half would pass for an implementation that also rewrote the old handle to point somewhere else.
//!
//! # Status of the "read by `FileId`" half
//!
//! Criterion 1's "the same `FileId` can still be read after the rename" and criterion 2's "the old `FileId` stays
//! readable" both need the core's by-`FileId` read path (P3-2 G2). On the mock they are decided; against the real
//! backend they are **`未验证（依赖 G2）`** — the card's three-state vocabulary, and deliberately not "partially
//! verified": a criterion whose second half has not run is not half true, it is unverified.

use fs_semantics::disposition::Disposition;
use fs_windows::share::{Mask, Term};
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

/// Criterion 1 — a handle opened before a rename still reads the original content, and `get_info` reports the same
/// identity.
///
/// The counter-half is that the **old path no longer resolves**: an implementation that "kept the old handle working" by
/// leaving the old name bound would satisfy the first assertion while breaking the namespace.
#[test]
fn a_handle_survives_a_rename_of_its_own_entry() {
    let backend = MockBackend::new();
    let id = backend
        .create_or_open(b"/f", Disposition::Create)
        .expect("create")
        .file_id;
    backend.write(id, 0, b"payload").expect("write");
    let mut table = HandleTable::new();
    let handle = table
        .open(id, mask(&[Term::Read]), Mask::ALL_BITS, false)
        .expect("open");

    let outcome = backend.rename(b"/f", b"/renamed", false).expect("rename");
    assert_eq!(outcome.replaced, None, "cell rename: nothing was replaced");

    let bound = table.get(handle).expect("the handle is still open");
    assert_eq!(
        bound.file_id, id,
        "cell handle: the handle still names the same id"
    );
    assert_eq!(
        backend.read(bound.file_id, 0, 7).expect("read"),
        b"payload",
        "cell content: reading through the handle returns the original bytes"
    );
    assert_eq!(
        backend.get_info(bound.file_id).expect("info").file_id,
        id,
        "cell identity: get_info reports the same FileId"
    );
    assert_eq!(
        backend.resolve(b"/f"),
        None,
        "cell counter-half: the old name is gone, so the handle is not being kept alive by a stale binding"
    );
    assert_eq!(
        backend.resolve(b"/renamed"),
        Some(id),
        "cell counter-half: the new name resolves to the same object"
    );
}

/// Criterion 1, directory form — a directory handle keeps enumerating **its own** entries after the directory is renamed
/// and another directory takes over the old path.
///
/// The two directories have different entry counts on purpose: comparing empty against empty would not tell "enumerated
/// the right directory" from "enumerated whatever is at the path now".
#[test]
fn a_directory_handle_survives_a_rename_and_a_reused_path() {
    let backend = MockBackend::new();
    let d1 = backend.mkdir(b"/d1").expect("mkdir d1");
    backend.mkdir(b"/d2").expect("mkdir d2");
    for name in [&b"/d1/one"[..], b"/d1/two"] {
        backend
            .create_or_open(name, Disposition::Create)
            .expect("create");
    }
    for name in [&b"/d2/a"[..], b"/d2/b", b"/d2/c"] {
        backend
            .create_or_open(name, Disposition::Create)
            .expect("create");
    }
    let mut table = HandleTable::new();
    let handle = table
        .open(d1, mask(&[Term::Read]), Mask::ALL_BITS, false)
        .expect("open d1");

    backend
        .rename(b"/d1", b"/d1-moved", false)
        .expect("rename d1");
    backend
        .rename(b"/d2", b"/d1", false)
        .expect("rename d2 onto d1's old path");

    let bound = table.get(handle).expect("still open").file_id;
    assert_eq!(bound, d1, "cell handle: the handle still names d1");
    assert_eq!(
        backend.read_dir(bound).expect("read_dir").len(),
        2,
        "cell enumeration: the old handle still sees d1's two entries"
    );
    let now = backend.resolve(b"/d1").expect("the reused path");
    assert_ne!(
        now, d1,
        "cell setup: the reused path is a different directory"
    );
    assert_eq!(
        backend.read_dir(now).expect("read_dir").len(),
        3,
        "cell control: the directory that took the path has three entries"
    );
}

/// Criterion 2 — `SUPERSEDE` replaces the entry *and* leaves the old object readable through its handle.
///
/// Three assertions, all required: the old handle reads the **old** bytes, the new entry is **empty** (size 0), and the
/// name resolves to the **new** id. Any two of the three are satisfiable by a wrong implementation — a truncate-in-place
/// keeps the id and the name but loses the old bytes; a "delete the old inode" loses the old handle; a "copy the old
/// inode" keeps the bytes but reports the wrong size.
#[test]
fn supersede_keeps_the_old_object_readable_through_its_handle() {
    let backend = MockBackend::new();
    let old = backend
        .create_or_open(b"/f", Disposition::Create)
        .expect("create")
        .file_id;
    backend.write(old, 0, b"original").expect("write");
    let mut table = HandleTable::new();
    let handle = table
        .open(old, mask(&[Term::Read]), Mask::ALL_BITS, false)
        .expect("open the old entry");

    let outcome = backend
        .create_or_open(b"/f", Disposition::Supersede)
        .expect("supersede");
    assert_eq!(
        outcome.replaced,
        Some(old),
        "cell outcome: the caller is told which id lost the name"
    );
    assert_ne!(outcome.file_id, old, "cell outcome: a fresh id");
    assert_eq!(outcome.size, 0, "cell new size: the new entry is empty");
    assert_eq!(
        backend.resolve(b"/f"),
        Some(outcome.file_id),
        "cell name: the name now resolves to the new id"
    );

    let bound = table.get(handle).expect("the old handle is still open");
    assert_eq!(bound.file_id, old, "cell handle: still the old id");
    assert_eq!(
        backend.read(bound.file_id, 0, 8).expect("old read"),
        b"original",
        "cell old content: the old object keeps its bytes"
    );
    assert_eq!(
        backend.get_info(bound.file_id).expect("old info").size,
        8,
        "cell old size: the old object still reports its own size (a truncated-in-place implementation would say 0)"
    );
    assert_eq!(
        backend.read(outcome.file_id, 0, 8).expect("new read"),
        vec![0u8; 8],
        "cell new content: reading the new entry yields zeros, not the old bytes"
    );
}

/// Criterion 2, deletion form — `SUPERSEDE` does **not** delete the old object: removing the new entry leaves the old
/// one untouched and readable.
///
/// This is what makes `replaced` usable by the adapter: it unbinds a name in its handle table, and the object behind an
/// open handle must still be there when that handle closes.
#[test]
fn replacing_an_entry_does_not_retire_the_old_object() {
    let backend = MockBackend::new();
    let old = backend
        .create_or_open(b"/f", Disposition::Create)
        .expect("create")
        .file_id;
    backend.write(old, 0, b"keep me").expect("write");
    let outcome = backend
        .create_or_open(b"/f", Disposition::Supersede)
        .expect("supersede");

    backend
        .remove_at(ROOT, b"f", outcome.file_id)
        .expect("remove the replacement");
    assert!(
        !backend.is_live(outcome.file_id),
        "cell new: the replacement is gone"
    );
    assert!(
        backend.is_live(old),
        "cell old: the superseded object is still live — SUPERSEDE unbinds a name, it does not delete"
    );
    assert_eq!(
        backend.read(old, 0, 7).expect("old still readable"),
        b"keep me",
        "cell old: and still holds its bytes"
    );
    assert_eq!(
        backend.remove_at(ROOT, b"f", old),
        Err(OpsError::NotFound),
        "cell guard: the name is already free, so the old id no longer names anything"
    );
}

/// The handle table and the backend agree about the **deletion** half of a `DELETE_ON_CLOSE` handle whose entry was
/// superseded: the close reports the handle's own id, so the adapter removes that object rather than the replacement.
#[test]
fn a_handle_deletes_its_own_object_after_a_supersede() {
    let backend = MockBackend::new();
    let old = backend
        .create_or_open(b"/f", Disposition::Create)
        .expect("create")
        .file_id;
    let mut table = HandleTable::new();
    let handle = table
        .open(old, mask(&[Term::Read, Term::Delete]), Mask::ALL_BITS, true)
        .expect("open with delete-on-close");

    let outcome = backend
        .create_or_open(b"/f", Disposition::Supersede)
        .expect("supersede");
    assert_ne!(
        outcome.file_id, old,
        "cell setup: the name now holds a different object"
    );
    assert_eq!(
        table.close(handle),
        Ok(CloseEffect::Remove(old)),
        "cell effect: the close reports the id the handle carries, not the one now at the path"
    );
    backend
        .remove_at(ROOT, b"f", old)
        .expect_err("the guard refuses: /f names the replacement, not the old object");
    assert!(
        backend.is_live(outcome.file_id),
        "cell survivor: the replacement is untouched by the old handle's deletion"
    );
}
