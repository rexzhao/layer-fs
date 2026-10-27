//! A boxed backend **is** a backend (P3-4 batch (i), the `dyn WorkspaceOps` decision).
//!
//! The registry holds `Adapter<Box<dyn WorkspaceOps>>` so the callbacks never name a concrete backend. That only works if the
//! boxed forwarding is faithful — and eleven hand-written `(**self)` calls are exactly where a swapped argument would hide. So
//! the criterion is **behaviour through the box equals behaviour through the concrete type**, including the error variants.

use fs_winfsp::mock::MockBackend;
use fs_semantics::disposition::Disposition;
use fs_winfsp::ops::{DynOps, OpsError, WorkspaceOps};

#[test]
fn a_boxed_backend_behaves_like_the_concrete_one() {
    let concrete = MockBackend::new();
    let boxed: DynOps = Box::new(MockBackend::new());

    // Same creation answer.
    let path = b"/alpha.txt";
    let a = concrete
        .create_or_open(path, Disposition::Create)
        .expect("concrete: create");
    let b = boxed
        .create_or_open(path, Disposition::Create)
        .expect("boxed: create — the forwarding reached the real implementation");
    assert_eq!(a.kind, b.kind, "cell kind: same answer through the box");
    assert_eq!(a.size, b.size, "cell size: same answer through the box");

    // Same bytes back for the same id — a swapped `(file_id, offset, len)` would show up here, not in a type error.
    concrete
        .write(a.file_id, 0, b"hello")
        .expect("concrete: write");
    boxed.write(b.file_id, 0, b"hello").expect("boxed: write");
    assert_eq!(
        boxed.read(b.file_id, 0, 5).expect("boxed: read"),
        concrete.read(a.file_id, 0, 5).expect("concrete: read"),
        "cell read: identical bytes through both paths"
    );

    // Same error **variant** for the same situation (the family that a status-only comparison would blur).
    let concrete_missing = concrete
        .read(0xDEAD, 0, 1)
        .expect_err("concrete: missing id");
    let boxed_missing = boxed.read(0xDEAD, 0, 1).expect_err("boxed: missing id");
    assert_eq!(
        std::mem::discriminant(&concrete_missing),
        std::mem::discriminant(&boxed_missing),
        "cell error: the same variant, not merely \"a failure\""
    );
    assert!(
        matches!(boxed_missing, OpsError::NotFound),
        "cell error: and it is NotFound"
    );

    // Same directory listing through the box.
    assert_eq!(
        concrete.read_dir(0).expect("concrete: list root").len(),
        boxed.read_dir(0).expect("boxed: list root").len(),
        "cell list: the same entries through the box"
    );
}

/// The erased adapter is constructible — the shape the registry needs.
#[test]
fn the_registry_can_hold_a_backend_erased_adapter() {
    use fs_winfsp::DynAdapter;

    let adapter: DynAdapter = DynAdapter::new(Box::new(MockBackend::new()));
    assert_eq!(
        adapter.context_counts(),
        (0, 0),
        "cell shape: an erased adapter is an adapter, with its own per-mount counters"
    );
}
