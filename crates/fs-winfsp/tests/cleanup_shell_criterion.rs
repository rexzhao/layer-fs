//! `Cleanup` (slot 6) and `Close` (slot 7): **two-phase** deletion at the FFI entry points.
//!
//! Windows marks with `Cleanup(FSP_CLEANUP_DELETE)` and deletes at `Close` — only for the **last** handle. The rule itself was already
//! written in P3-3a (`adapter_criterion3`, `handles_criteria`); this file checks the part that is new: the callbacks really reach it.
//!
//! **Sharing is not implemented yet** (card U3), so every handle is admitted with all bits. That is stated rather than simulated: a
//! fixture that pretended to enforce sharing would be a false green for the 9-cell matrix.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use fs_semantics::disposition::Disposition;
use fs_winfsp::callbacks::{cleanup_shell, close_shell, EntryContext, FSP_CLEANUP_DELETE};
use fs_winfsp::handles::HandleId;
use fs_winfsp::ops::DynOps;
use fs_winfsp::ops::{
    Attributes, DirEntry, Kind, NodeInfo, OpenOutcome, OpsError, OpsResult, RenameOutcome,
    WorkspaceOps,
};
use fs_winfsp::DynAdapter;

/// Counts `remove_at` and remembers which id it was told to remove.
struct Deleter {
    removed: Arc<AtomicUsize>,
    ids: Arc<Mutex<Vec<u64>>>,
}

fn node() -> NodeInfo {
    NodeInfo {
        file_id: 3,
        kind: Kind::File,
        size: 0,
        times: fs_winfsp::file_info::times(1, 1, 1, 1),
        attributes: Attributes(0x80),
    }
}

impl WorkspaceOps for Deleter {
    fn create_or_open(&self, _path: &[u8], disposition: Disposition) -> OpsResult<OpenOutcome> {
        if disposition != Disposition::Open {
            return Err(OpsError::ReadOnly);
        }
        Ok(OpenOutcome {
            file_id: 3,
            kind: Kind::File,
            action: fs_semantics::disposition::Action::Open,
            size: 0,
            times: node().times,
            attributes: Attributes(0x80),
            binding: (0, b"victim".to_vec()),
            replaced: None,
        })
    }
    fn mkdir(&self, _path: &[u8]) -> OpsResult<u64> {
        Err(OpsError::ReadOnly)
    }
    fn read(&self, _id: u64, _o: u64, _l: usize) -> OpsResult<Vec<u8>> {
        Ok(Vec::new())
    }
    fn write(&self, _id: u64, _o: u64, _b: &[u8]) -> OpsResult<usize> {
        Err(OpsError::ReadOnly)
    }
    fn set_size(&self, _id: u64, _l: u64) -> OpsResult<()> {
        Err(OpsError::ReadOnly)
    }
    fn read_dir(&self, _id: u64) -> OpsResult<Vec<DirEntry>> {
        Ok(Vec::new())
    }
    fn get_info(&self, id: u64) -> OpsResult<NodeInfo> {
        Ok(NodeInfo {
            file_id: id,
            ..node()
        })
    }
    fn set_times(
        &self,
        _id: u64,
        _a: Option<i64>,
        _w: Option<i64>,
        _c: Option<i64>,
        _cr: Option<i64>,
    ) -> OpsResult<()> {
        Ok(())
    }
    fn set_attributes(&self, _id: u64, _a: Attributes) -> OpsResult<()> {
        Ok(())
    }
    fn rename(&self, _f: &[u8], _t: &[u8], _r: bool) -> OpsResult<RenameOutcome> {
        Err(OpsError::ReadOnly)
    }
    fn remove_at(&self, _parent: u64, _name: &[u8], expected_id: u64) -> OpsResult<()> {
        self.removed.fetch_add(1, Ordering::Relaxed);
        self.ids.lock().expect("ids").push(expected_id);
        Ok(())
    }
    fn flush(&self) -> OpsResult<()> {
        Ok(())
    }
}

/// Forwards to the shared `Deleter`, so the test can read the counters after the adapter owns the backend.
struct Shared(Arc<Deleter>);

impl WorkspaceOps for Shared {
    fn create_or_open(&self, p: &[u8], d: Disposition) -> OpsResult<OpenOutcome> {
        self.0.create_or_open(p, d)
    }
    fn mkdir(&self, p: &[u8]) -> OpsResult<u64> {
        self.0.mkdir(p)
    }
    fn read(&self, i: u64, o: u64, l: usize) -> OpsResult<Vec<u8>> {
        self.0.read(i, o, l)
    }
    fn write(&self, i: u64, o: u64, b: &[u8]) -> OpsResult<usize> {
        self.0.write(i, o, b)
    }
    fn set_size(&self, i: u64, l: u64) -> OpsResult<()> {
        self.0.set_size(i, l)
    }
    fn read_dir(&self, i: u64) -> OpsResult<Vec<DirEntry>> {
        self.0.read_dir(i)
    }
    fn get_info(&self, i: u64) -> OpsResult<NodeInfo> {
        self.0.get_info(i)
    }
    fn set_times(
        &self,
        i: u64,
        a: Option<i64>,
        w: Option<i64>,
        c: Option<i64>,
        cr: Option<i64>,
    ) -> OpsResult<()> {
        self.0.set_times(i, a, w, c, cr)
    }
    fn set_attributes(&self, i: u64, a: Attributes) -> OpsResult<()> {
        self.0.set_attributes(i, a)
    }
    fn rename(&self, f: &[u8], t: &[u8], r: bool) -> OpsResult<RenameOutcome> {
        self.0.rename(f, t, r)
    }
    fn remove_at(&self, p: u64, n: &[u8], e: u64) -> OpsResult<()> {
        self.0.remove_at(p, n, e)
    }
    fn flush(&self) -> OpsResult<()> {
        self.0.flush()
    }
}

fn fixture() -> (DynAdapter, Arc<AtomicUsize>, Arc<Mutex<Vec<u64>>>) {
    let removed = Arc::new(AtomicUsize::new(0));
    let ids = Arc::new(Mutex::new(Vec::new()));
    let deleter = Deleter {
        removed: Arc::clone(&removed),
        ids: Arc::clone(&ids),
    };
    (
        DynAdapter::new(Box::new(Shared(Arc::new(deleter))) as DynOps),
        removed,
        ids,
    )
}

fn context(file_id: u64, handle: HandleId) -> EntryContext {
    EntryContext { file_id, handle }
}

/// Registers a hand-built context the way the frontend does — the value that crosses the FFI boundary is an **id**, not an address.
fn alloc(context: EntryContext) -> *mut c_void {
    fs_winfsp::callbacks::register_context_for_test(context)
}

#[test]
fn cleanup_marks_and_the_last_close_deletes_exactly_once() {
    let (adapter, removed, ids) = fixture();
    let first = adapter
        .open_handle(3, Some((0, b"victim".to_vec())))
        .expect("first handle");
    let second = adapter
        .open_handle(3, Some((0, b"victim".to_vec())))
        .expect("second handle");
    let a = alloc(context(3, first));
    let b = alloc(context(3, second));

    assert_eq!(
        unsafe { cleanup_shell(Some(&adapter), a, FSP_CLEANUP_DELETE) },
        0,
        "cell mark: cleanup accepted the delete flag"
    );
    assert_eq!(
        removed.load(Ordering::Relaxed),
        0,
        "cell mark: **marking is not deleting**"
    );

    unsafe { close_shell(Some(&adapter), b) };
    assert_eq!(
        removed.load(Ordering::Relaxed),
        0,
        "cell other handle: a close with a live handle left does not delete"
    );

    unsafe { close_shell(Some(&adapter), a) };
    assert_eq!(
        removed.load(Ordering::Relaxed),
        1,
        "cell last close: **exactly one** `remove_at`"
    );
    assert_eq!(
        ids.lock().expect("ids").as_slice(),
        &[3],
        "cell guard: the removal names the entry that was opened"
    );
}

#[test]
fn a_close_without_the_delete_flag_never_deletes() {
    let (adapter, removed, _ids) = fixture();
    let handle = adapter
        .open_handle(3, Some((0, b"victim".to_vec())))
        .expect("handle");
    let a = alloc(context(3, handle));

    assert_eq!(
        unsafe { cleanup_shell(Some(&adapter), a, 0) },
        0,
        "cell no flag: accepted"
    );
    unsafe { close_shell(Some(&adapter), a) };
    assert_eq!(
        removed.load(Ordering::Relaxed),
        0,
        "cell no flag: closing a plain handle is not deleting"
    );
}

#[test]
fn a_closed_handle_cannot_be_closed_twice() {
    // A second `close_shell` would read a freed `Box`, so the pinned property is the **table's**: a closed handle is an error, not a
    // no-op — otherwise ids could be reused silently and the second close would delete somebody else's entry.
    let (adapter, removed, _ids) = fixture();
    let handle = adapter
        .open_handle(3, Some((0, b"victim".to_vec())))
        .expect("handle");
    adapter.set_delete(handle).expect("mark");
    adapter
        .close(handle)
        .expect("first close removes the entry");
    assert_eq!(
        removed.load(Ordering::Relaxed),
        1,
        "cell first close: one removal"
    );
    assert!(
        adapter.close(handle).is_err(),
        "cell second close: refused by the table"
    );
    assert_eq!(
        removed.load(Ordering::Relaxed),
        1,
        "cell second close: and it does not delete a second time"
    );
}

#[test]
fn null_and_forged_contexts_are_refused_rather_than_dereferenced() {
    assert_ne!(
        unsafe { cleanup_shell(None, std::ptr::null(), FSP_CLEANUP_DELETE) },
        0,
        "cell null: refused"
    );
    // Closing a null context is a no-op: WinFsp may deliver `Close` for an entry that never opened.
    unsafe { close_shell(None, std::ptr::null_mut()) };

    // A forged handle names nothing, and with no mount there is nothing to mark. The context is still allocated, and nothing frees it
    // here: `cleanup_shell` never takes ownership, which is the contract the shell documents.
    let forged = alloc(context(3, HandleId::from_raw(4242)));
    assert_ne!(
        unsafe { cleanup_shell(None, forged, FSP_CLEANUP_DELETE) },
        0,
        "cell forged: no mount means no marking"
    );
}
