//! Criterion 3's **adapter half** (P3-4 step 0b): delete-pending and "removed exactly once", in pure Rust.
//!
//! This is the half P3-3a could not assert — the core has no notion of a handle. Doing it here, before the FFI layer, means
//! the mount work is plumbing rather than plumbing plus first semantic verification of the hardest rule in the card.

use fs_semantics::disposition::Disposition;
use fs_windows::share::{Mask, Term};
use fs_winfsp::{Adapter, Call, HandleId, MockBackend, OpsError, ROOT};

fn mask(terms: &[Term]) -> Mask {
    terms.iter().fold(Mask::NONE, |acc, term| {
        Mask::from_bits(
            acc.bits()
                | match term {
                    Term::Read => Mask::READ.bits(),
                    Term::Write => Mask::WRITE.bits(),
                    Term::Delete => Mask::DELETE.bits(),
                },
        )
    })
}

fn open_file(adapter: &mut Adapter<MockBackend>, path: &[u8]) -> (HandleId, u64) {
    let (handle, id, _kind) = adapter
        .open(
            path,
            Disposition::Create,
            mask(&[Term::Read, Term::Write, Term::Delete]),
            Mask::ALL_BITS,
            false,
        )
        .expect("open");
    (handle, id)
}

/// The three parts of two-phase deletion, in one flow:
/// (i) a **new** open of a pending entry is refused with `DeletePending`;
/// (ii) the handles opened earlier keep reading and writing;
/// (iii) the last close removes the entry **exactly once**.
#[test]
fn a_pending_entry_refuses_new_opens_but_serves_old_handles() {
    let mut adapter = Adapter::new(MockBackend::new());
    let (first, id) = open_file(&mut adapter, b"/f");
    let (second, _) = {
        let (handle, _, _) = adapter
            .open(
                b"/f",
                Disposition::Open,
                mask(&[Term::Read]),
                Mask::ALL_BITS,
                false,
            )
            .expect("second handle");
        (handle, id)
    };
    adapter
        .write(first, 0, b"payload")
        .expect("write through the first handle");

    adapter.set_delete(first).expect("mark delete-pending");

    // (i) a new open is refused — and it is the *adapter* that refuses, before touching the entry.
    match adapter.open(
        b"/f",
        Disposition::Open,
        mask(&[Term::Read]),
        Mask::ALL_BITS,
        false,
    ) {
        Err(OpsError::DeletePending) => {}
        other => panic!("cell (i): expected `DeletePending`, got {other:?}"),
    }

    // (ii) the earlier handles still work, in both directions.
    assert_eq!(
        adapter.read(first, 0, 7).expect("read"),
        b"payload",
        "cell (ii): the handle opened before the mark still reads the content"
    );
    adapter
        .write(first, 0, b"payload")
        .expect("cell (ii): and can still write");

    // (iii) closing one handle removes nothing; closing the last one removes **once**, with the current name.
    adapter.ops(); // keep the backend call log readable below
    adapter.close(first).expect("close first");
    let removals_after_first = adapter
        .ops()
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::RemoveAt(..)))
        .count();
    assert_eq!(
        removals_after_first, 0,
        "cell (iii): closing a non-last handle must not reach `remove_at`"
    );

    adapter.close(second).expect("close last");
    let removals = adapter
        .ops()
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::RemoveAt(..)))
        .collect::<Vec<_>>();
    assert_eq!(
        removals,
        vec![Call::RemoveAt(ROOT, b"f".to_vec(), id)],
        "cell (iii): exactly one removal, carrying the id the caller believed in"
    );
}

/// The name used for the removal is the **current** one: a rename between `set_delete` and the last close must not make the
/// guard refuse (which would look like "cannot delete" rather than "wrong name").
#[test]
fn the_removal_after_a_rename_uses_the_new_name() {
    let mut adapter = Adapter::new(MockBackend::new());
    let (handle, id) = open_file(&mut adapter, b"/f");
    adapter.set_delete(handle).expect("mark pending");
    adapter.rename(b"/f", b"/moved", false).expect("rename");

    adapter.close(handle).expect("close last");
    let removals = adapter
        .ops()
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::RemoveAt(..)))
        .collect::<Vec<_>>();
    assert_eq!(
        removals,
        vec![Call::RemoveAt(ROOT, b"moved".to_vec(), id)],
        "cell name: the adapter removed the entry by its **new** name — the binding table is what makes that possible"
    );
}

/// A read-only handle cannot write: the adapter enforces the access bits it recorded.
#[test]
fn a_read_only_handle_cannot_write() {
    let mut adapter = Adapter::new(MockBackend::new());
    let (handle, _) = {
        let (handle, id, _) = adapter
            .open(
                b"/f",
                Disposition::Create,
                mask(&[Term::Read]),
                Mask::ALL_BITS,
                false,
            )
            .expect("open read-only");
        (handle, id)
    };
    match adapter.write(handle, 0, b"x") {
        Err(OpsError::AccessDenied) => {}
        other => panic!("cell access: expected `AccessDenied`, got {other:?}"),
    }
}
