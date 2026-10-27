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
use fs_winfsp::ops::Path;
use fs_winfsp::{HandleTable, MockBackend, WorkspaceOps, ROOT};

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
        .create_or_open(&Path::new(b"/f"), Disposition::Create)
        .expect("create")
        .file_id;
    backend.write(id, 0, b"payload").expect("write");
    let mut table = HandleTable::new();
    let handle = table
        .open(id, mask(&[Term::Read]), Mask::ALL_BITS, false)
        .expect("open");

    let outcome = backend
        .rename(&Path::new(b"/f"), &Path::new(b"/renamed"), false)
        .expect("rename");
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
    let d1 = backend.mkdir(&Path::new(b"/d1")).expect("mkdir d1");
    backend.mkdir(&Path::new(b"/d2")).expect("mkdir d2");
    for name in [&b"/d1/one"[..], b"/d1/two"] {
        backend
            .create_or_open(&Path::new(name), Disposition::Create)
            .expect("create");
    }
    for name in [&b"/d2/a"[..], b"/d2/b", b"/d2/c"] {
        backend
            .create_or_open(&Path::new(name), Disposition::Create)
            .expect("create");
    }
    let mut table = HandleTable::new();
    let handle = table
        .open(d1, mask(&[Term::Read]), Mask::ALL_BITS, false)
        .expect("open d1");

    backend
        .rename(&Path::new(b"/d1"), &Path::new(b"/d1-moved"), false)
        .expect("rename d1");
    backend
        .rename(&Path::new(b"/d2"), &Path::new(b"/d1"), false)
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

/// Criterion 2 (rewritten for ruling (a)) — after a `SUPERSEDE` **both** handles see the new content.
///
/// The old criterion asserted the opposite ("the old handle still reads the old bytes") and needed a second test for
/// "the old object was not retired". Ruling (a) removes that object: there is one entry, truncated in place, so the
/// question becomes "does every handle agree on what is there" — which is why this is **two** tests, not three. The
/// reduction is expected, not a loss.
#[test]
fn a_supersede_is_visible_through_every_handle() {
    let backend = MockBackend::new();
    let id = backend
        .create_or_open(&Path::new(b"/f"), Disposition::Create)
        .expect("create")
        .file_id;
    backend.write(id, 0, b"original").expect("write");
    let mut table = HandleTable::new();
    let before = table
        .open(id, mask(&[Term::Read]), Mask::ALL_BITS, false)
        .expect("handle opened before the supersede");

    let outcome = backend
        .create_or_open(&Path::new(b"/f"), Disposition::Supersede)
        .expect("supersede");
    assert_eq!(outcome.file_id, id, "cell identity: unchanged");
    assert_eq!(outcome.replaced, None, "cell replaced: nothing lost a name");

    let after = table
        .open(id, mask(&[Term::Read]), Mask::ALL_BITS, false)
        .expect("handle opened after the supersede");
    for handle in [before, after] {
        let bound = table.get(handle).expect("open").file_id;
        assert_eq!(bound, id, "cell handle: both name the same entry");
        assert_eq!(
            backend.read(bound, 0, 8).expect("read"),
            vec![0u8; 8],
            "cell content: both handles read the overwritten content — a handle is not a snapshot"
        );
        assert_eq!(
            backend.get_info(bound).expect("info").size,
            0,
            "cell size: and both report the new size"
        );
    }
}

/// Criterion 2, removal half — the (single) entry is still the deletable one, guarded by its id.
///
/// This replaces "the old object was not retired": with ruling (a) there is no second object to retire, so the surviving
/// question is whether the id the caller holds is still the one that names the entry.
#[test]
fn the_superseded_id_is_still_the_deletable_one() {
    let backend = MockBackend::new();
    let id = backend
        .create_or_open(&Path::new(b"/f"), Disposition::Create)
        .expect("create")
        .file_id;
    let outcome = backend
        .create_or_open(&Path::new(b"/f"), Disposition::Supersede)
        .expect("supersede");
    assert_eq!(outcome.file_id, id, "cell setup: same entry");

    backend
        .remove_at(ROOT, b"f", id)
        .expect("cell removal: the id still guards the entry");
    assert!(!backend.is_live(id), "cell result: the entry is gone");
    assert_eq!(
        backend.resolve(b"/f"),
        None,
        "cell name: and the name is free again"
    );
}
