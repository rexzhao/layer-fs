//! `SetFileSize` (slot 13): the criterion for "accepted" versus "is" (write-chain batch).
//!
//! The dangerous failure on the write side is not an error — it is a backend that accepts a length and does not apply it, leaving the
//! caller to believe the file is now that big (382: the write-side mirror of "zero records read as an empty file"). So the shell
//! does two things and the tests hold them apart:
//!
//! 1. ask the backend to resize, **exactly once**, with the exact value;
//! 2. read the entry back and answer only if the size really is the requested one — otherwise refuse.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use fs_semantics::disposition::Disposition;
use fs_winfsp::abi::FileInfo;
use fs_winfsp::callbacks::{
    set_file_size_shell, set_size_call_count, set_size_disagreement_count, EntryContext,
    NOT_IMPLEMENTED, OURS_INVALID_PARAMETER,
};
use fs_winfsp::ops::DynOps;
use fs_winfsp::ops::Path;
use fs_winfsp::ops::{
    Attributes, DirEntry, Kind, NodeInfo, OpenOutcome, OpsError, OpsResult, RenameOutcome,
    WorkspaceOps,
};
use fs_winfsp::DynAdapter;

/// A backend whose stored size can be made to disagree with what it was told — the two behaviours the shell must separate.
struct Resizer {
    /// The size the backend will report.
    stored: Arc<AtomicU64>,
    /// When false, `set_size` succeeds **without** changing the stored size (the silent-ignore backend).
    applies: bool,
    calls: Arc<AtomicUsize>,
    last_request: Arc<AtomicU64>,
}

impl Resizer {
    fn applying(size: u64) -> Self {
        Self {
            stored: Arc::new(AtomicU64::new(size)),
            applies: true,
            calls: Arc::new(AtomicUsize::new(0)),
            last_request: Arc::new(AtomicU64::new(u64::MAX)),
        }
    }

    fn ignoring() -> Self {
        Self {
            applies: false,
            ..Self::applying(16)
        }
    }
}

fn node(size: u64) -> NodeInfo {
    NodeInfo {
        file_id: 9,
        kind: Kind::File,
        size,
        times: fs_winfsp::file_info::times(1, 1, 1, 1),
        attributes: Attributes(0x80),
    }
}

impl WorkspaceOps for Resizer {
    fn create_or_open(
        &self,
        _path: &Path<'_>,
        _disposition: Disposition,
    ) -> OpsResult<OpenOutcome> {
        Err(OpsError::ReadOnly)
    }
    fn mkdir(&self, _path: &Path<'_>) -> OpsResult<u64> {
        Err(OpsError::ReadOnly)
    }
    fn read(&self, _file_id: u64, _offset: u64, _len: usize) -> OpsResult<Vec<u8>> {
        Err(OpsError::ReadOnly)
    }
    fn write(&self, _file_id: u64, _offset: u64, _bytes: &[u8]) -> OpsResult<usize> {
        Err(OpsError::ReadOnly)
    }
    fn set_size(&self, _file_id: u64, len: u64) -> OpsResult<()> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.last_request.store(len, Ordering::Relaxed);
        if self.applies {
            self.stored.store(len, Ordering::Relaxed);
        }
        Ok(())
    }
    fn read_dir(&self, _dir_id: u64) -> OpsResult<Vec<DirEntry>> {
        Ok(Vec::new())
    }
    fn get_info(&self, _file_id: u64) -> OpsResult<NodeInfo> {
        Ok(node(self.stored.load(Ordering::Relaxed)))
    }
    fn set_times(
        &self,
        _file_id: u64,
        _a: Option<i64>,
        _w: Option<i64>,
        _c: Option<i64>,
        _cr: Option<i64>,
    ) -> OpsResult<()> {
        Err(OpsError::ReadOnly)
    }
    fn set_attributes(&self, _file_id: u64, _a: Attributes) -> OpsResult<()> {
        Err(OpsError::ReadOnly)
    }
    fn rename(&self, _f: &Path<'_>, _t: &Path<'_>, _r: bool) -> OpsResult<RenameOutcome> {
        Err(OpsError::ReadOnly)
    }
    fn remove_at(&self, _p: u64, _n: &[u8], _e: u64) -> OpsResult<()> {
        Err(OpsError::ReadOnly)
    }
    fn flush(&self) -> OpsResult<()> {
        Ok(())
    }
}

/// Forwards to the shared `Resizer` so the test can read back what happened.
struct Shared(Arc<Resizer>);

impl WorkspaceOps for Shared {
    fn create_or_open(&self, p: &Path<'_>, d: Disposition) -> OpsResult<OpenOutcome> {
        self.0.create_or_open(p, d)
    }
    fn mkdir(&self, p: &Path<'_>) -> OpsResult<u64> {
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
    fn rename(&self, f: &Path<'_>, t: &Path<'_>, r: bool) -> OpsResult<RenameOutcome> {
        self.0.rename(f, t, r)
    }
    fn remove_at(&self, p: u64, n: &[u8], e: u64) -> OpsResult<()> {
        self.0.remove_at(p, n, e)
    }
    fn flush(&self) -> OpsResult<()> {
        self.0.flush()
    }
}

fn run(adapter: &DynAdapter, new_size: u64) -> (i32, FileInfo) {
    let context = fs_winfsp::callbacks::register_context_for_test(EntryContext {
        file_id: 9,
        handle: fs_winfsp::handles::HandleId::from_raw(1),
    });
    let mut out = FileInfo::zeroed();
    let status = unsafe {
        set_file_size_shell(
            Some(adapter),
            context,
            new_size,
            false,
            &mut out as *mut FileInfo,
        )
    };
    (status, out)
}

#[test]
fn the_size_is_asked_for_once_and_the_answer_is_read_back() {
    let resizer = Arc::new(Resizer::applying(16));
    let adapter = DynAdapter::new(Box::new(Shared(Arc::clone(&resizer))) as DynOps);

    let (status, record) = run(&adapter, 4096);

    assert_eq!(
        status, 0,
        "cell ok: a backend that applies the size succeeds"
    );
    assert_eq!(
        resizer.calls.load(Ordering::Relaxed),
        1,
        "cell ok: **one** request — not a retry loop and not zero"
    );
    assert_eq!(
        resizer.last_request.load(Ordering::Relaxed),
        4096,
        "cell ok: with the exact value the caller asked for"
    );
    assert_eq!(
        record.file_size, 4096,
        "cell ok: and the record describes the store, not the request"
    );
}

#[test]
fn a_backend_that_accepts_and_ignores_is_caught() {
    // The silent-ignore backend: `set_size` returns `Ok` and the size never changes. Answering success here is the write-side
    // version of "an empty file looks like a healthy empty listing".
    let resizer = Arc::new(Resizer::ignoring());
    let adapter = DynAdapter::new(Box::new(Shared(Arc::clone(&resizer))) as DynOps);
    let before = set_size_disagreement_count();

    let (status, record) = run(&adapter, 4096);

    assert_eq!(
        status, NOT_IMPLEMENTED,
        "cell ignore: refused — \"accepted\" is not \"is\""
    );
    assert_eq!(
        record,
        FileInfo::zeroed(),
        "cell ignore: and the caller's buffer is untouched"
    );
    assert_eq!(
        set_size_disagreement_count() - before,
        1,
        "cell ignore: the disagreement is **counted**, so it cannot be mistaken for an ordinary failure"
    );
}

#[test]
fn a_null_context_or_buffer_is_refused_before_anything_else() {
    let resizer = Arc::new(Resizer::applying(16));
    let adapter = DynAdapter::new(Box::new(Shared(Arc::clone(&resizer))) as DynOps);
    let mut out = FileInfo::zeroed();

    assert_eq!(
        unsafe {
            set_file_size_shell(
                Some(&adapter),
                std::ptr::null(),
                10,
                false,
                &mut out as *mut FileInfo,
            )
        },
        OURS_INVALID_PARAMETER,
        "cell null context: refused"
    );
    assert_eq!(
        resizer.calls.load(Ordering::Relaxed),
        0,
        "cell null context: **the backend was not asked** — an invalid call costs nothing downstream"
    );
}

#[test]
fn no_mount_is_refused_and_nothing_is_written() {
    let mut out = FileInfo::zeroed();
    let context = fs_winfsp::callbacks::register_context_for_test(EntryContext {
        file_id: 9,
        handle: fs_winfsp::handles::HandleId::from_raw(1),
    });
    let status =
        unsafe { set_file_size_shell(None, context, 10, false, &mut out as *mut FileInfo) };
    assert_eq!(status, NOT_IMPLEMENTED, "cell no mount: refused");
    assert_eq!(out, FileInfo::zeroed(), "cell no mount: buffer untouched");
}

#[test]
fn the_entry_point_is_counted() {
    let resizer = Arc::new(Resizer::applying(0));
    let adapter = DynAdapter::new(Box::new(Shared(resizer)) as DynOps);
    let before = set_size_call_count();
    let _ = run(&adapter, 1);
    assert_eq!(
        set_size_call_count() - before,
        1,
        "cell count: one entry, one count — the counter a future L2 comparison will read"
    );
}
