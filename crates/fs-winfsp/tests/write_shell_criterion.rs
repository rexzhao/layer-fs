//! `Write` (slot 9): the write-side criterion, with the same rule as `SetFileSize` — **accepted is not written**.
//!
//! Three header details are each a way to be silently wrong (`winfsp.h:452-466`):
//!
//! - `WriteToEndOfFile` arrives as `Offset == -1`: the write goes to the current end;
//! - `ConstrainedIo` forbids growing the file: the write is trimmed, not extended;
//! - `*PBytesTransferred` is the caller's only evidence that anything happened.
//!
//! The read-back rule (021ed97) is applied here too: after `write`, the size must be exactly what the write implies, or the call is
//! refused and counted. A backend that accepts bytes and drops them must not look like a successful write.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use fs_semantics::disposition::Disposition;
use fs_winfsp::abi::FileInfo;
use fs_winfsp::callbacks::{
    write_disagreement_count, write_shell, EntryContext, NOT_IMPLEMENTED, OURS_INVALID_PARAMETER,
};
use fs_winfsp::handles::HandleId;
use fs_winfsp::ops::DynOps;
use fs_winfsp::ops::{
    Attributes, DirEntry, Kind, NodeInfo, OpenOutcome, OpsError, OpsResult, RenameOutcome,
    WorkspaceOps,
};
use fs_winfsp::DynAdapter;

/// A one-file backend that really stores bytes — or pretends to.
struct Store {
    content: Mutex<Vec<u8>>,
    /// When false, `write` returns the count and stores nothing (the silent-drop backend).
    applies: bool,
    calls: Arc<AtomicUsize>,
    last_offset: Arc<AtomicU64>,
}

impl Store {
    fn applying(bytes: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            content: Mutex::new(bytes.to_vec()),
            applies: true,
            calls: Arc::new(AtomicUsize::new(0)),
            last_offset: Arc::new(AtomicU64::new(u64::MAX)),
        })
    }

    fn dropping(bytes: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            content: Mutex::new(bytes.to_vec()),
            applies: false,
            calls: Arc::new(AtomicUsize::new(0)),
            last_offset: Arc::new(AtomicU64::new(u64::MAX)),
        })
    }
}

fn node(size: u64) -> NodeInfo {
    NodeInfo {
        file_id: 4,
        kind: Kind::File,
        size,
        times: fs_winfsp::file_info::times(1, 1, 1, 1),
        attributes: Attributes(0x80),
    }
}

impl WorkspaceOps for Store {
    fn create_or_open(&self, _path: &[u8], _d: Disposition) -> OpsResult<OpenOutcome> {
        Err(OpsError::ReadOnly)
    }
    fn mkdir(&self, _path: &[u8]) -> OpsResult<u64> {
        Err(OpsError::ReadOnly)
    }
    fn read(&self, _id: u64, offset: u64, len: usize) -> OpsResult<Vec<u8>> {
        let content = self.content.lock().expect("content");
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(content.len());
        let end = (start + len).min(content.len());
        Ok(content[start..end].to_vec())
    }
    fn write(&self, _id: u64, offset: u64, bytes: &[u8]) -> OpsResult<usize> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.last_offset.store(offset, Ordering::Relaxed);
        if self.applies {
            let mut content = self.content.lock().expect("content");
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if start > content.len() {
                content.resize(start, 0);
            }
            let end = start + bytes.len();
            if end > content.len() {
                content.resize(end, 0);
            }
            content[start..end].copy_from_slice(bytes);
        }
        Ok(bytes.len())
    }
    fn set_size(&self, _id: u64, len: u64) -> OpsResult<()> {
        let mut content = self.content.lock().expect("content");
        content.resize(usize::try_from(len).unwrap_or(usize::MAX), 0);
        Ok(())
    }
    fn read_dir(&self, _id: u64) -> OpsResult<Vec<DirEntry>> {
        Ok(Vec::new())
    }
    fn get_info(&self, id: u64) -> OpsResult<NodeInfo> {
        let size = self.content.lock().expect("content").len() as u64;
        Ok(NodeInfo {
            file_id: id,
            ..node(size)
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
    fn remove_at(&self, _p: u64, _n: &[u8], _e: u64) -> OpsResult<()> {
        Ok(())
    }
    fn flush(&self) -> OpsResult<()> {
        Ok(())
    }
}

/// Forwards to the shared `Store`.
struct Shared(Arc<Store>);

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

struct Request {
    store: Arc<Store>,
    adapter: DynAdapter,
}

fn fixture(bytes: &[u8], applies: bool) -> Request {
    let store = if applies {
        Store::applying(bytes)
    } else {
        Store::dropping(bytes)
    };
    let adapter = DynAdapter::new(Box::new(Shared(Arc::clone(&store))) as DynOps);
    Request { store, adapter }
}

/// Runs one `write`; returns `(status, bytes_transferred, size reported in the record)`.
fn write(
    request: &Request,
    offset: u64,
    data: &[u8],
    write_to_end_of_file: bool,
    constrained: bool,
) -> (i32, u32, u64) {
    let context = fs_winfsp::callbacks::register_context_for_test(EntryContext {
        file_id: 4,
        handle: HandleId::from_raw(1),
    });
    let mut record = FileInfo::zeroed();
    let mut transferred = u32::MAX;
    let status = unsafe {
        write_shell(
            Some(&request.adapter),
            context,
            data.as_ptr().cast::<c_void>(),
            offset,
            u32::try_from(data.len()).expect("small"),
            write_to_end_of_file,
            constrained,
            std::ptr::addr_of_mut!(transferred),
            std::ptr::addr_of_mut!(record),
        )
    };
    (status, transferred, record.file_size)
}

#[test]
fn a_write_at_an_offset_lands_there_and_the_record_shows_the_new_size() {
    let request = fixture(b"0123456789", true);
    let (status, transferred, size) = write(&request, 4, b"abc", false, false);

    assert_eq!(status, 0, "cell ok: the write succeeds");
    assert_eq!(
        transferred, 3,
        "cell ok: and the caller is told how many bytes landed"
    );
    assert_eq!(
        size, 10,
        "cell ok: the size did not change — the write stayed inside"
    );
    assert_eq!(
        request.store.last_offset.load(Ordering::Relaxed),
        4,
        "cell ok: the backend was asked at the **offset from the callback**, not at zero"
    );
    assert_eq!(
        request.store.read(4, 0, 10).expect("read back"),
        b"0123abc789".to_vec(),
        "cell ok: the bytes really are there"
    );
}

#[test]
fn write_to_end_of_file_ignores_the_sentinel_offset() {
    // `WriteToEndOfFile` arrives with `Offset == u64::MAX`; using that number directly would try to write at 16 exabytes.
    let request = fixture(b"abc", true);
    let (status, transferred, size) = write(&request, u64::MAX, b"de", true, false);

    assert_eq!(status, 0, "cell eof: success");
    assert_eq!(transferred, 2, "cell eof: two bytes");
    assert_eq!(size, 5, "cell eof: the file grew to the end, not to 2^64");
    assert_eq!(
        request.store.read(0, 0, 16).expect("read back"),
        b"abcde".to_vec(),
        "cell eof: appended at the old end"
    );
}

#[test]
fn constrained_io_trims_instead_of_growing_the_file() {
    // "Must not extend the file" (`winfsp.h:461`): a write past the end takes only what fits.
    let request = fixture(b"abcd", true);
    let (status, transferred, size) = write(&request, 2, b"WXYZ", false, true);

    assert_eq!(status, 0, "cell constrained: success");
    assert_eq!(
        transferred, 2,
        "cell constrained: only the two bytes that fit were written"
    );
    assert_eq!(size, 4, "cell constrained: **and the file did not grow**");
    assert_eq!(
        request.store.read(0, 0, 8).expect("read back"),
        b"abWX".to_vec(),
        "cell constrained: the bytes replaced the tail, the length is unchanged"
    );
}

#[test]
fn a_backend_that_accepts_and_drops_the_bytes_is_caught() {
    let request = fixture(b"0123456789", false);
    let before = write_disagreement_count();
    let (status, _, _) = write(&request, 4, b"abc", false, false);

    assert_eq!(
        status, NOT_IMPLEMENTED,
        "cell drop: refused — the size does not follow from the bytes"
    );
    assert_eq!(
        write_disagreement_count() - before,
        1,
        "cell drop: counted, so it cannot be mistaken for an ordinary failure"
    );
}

#[test]
fn null_pointers_are_refused_before_the_backend_is_asked() {
    let request = fixture(b"abc", true);
    let context = fs_winfsp::callbacks::register_context_for_test(EntryContext {
        file_id: 4,
        handle: HandleId::from_raw(1),
    });
    let mut record = FileInfo::zeroed();
    let mut transferred = 0u32;

    let status = unsafe {
        write_shell(
            Some(&request.adapter),
            std::ptr::null(),
            b"x".as_ptr().cast::<c_void>(),
            0,
            1,
            false,
            false,
            std::ptr::addr_of_mut!(transferred),
            std::ptr::addr_of_mut!(record),
        )
    };
    assert_eq!(status, OURS_INVALID_PARAMETER, "cell null context: refused");
    assert_eq!(
        request.store.calls.load(Ordering::Relaxed),
        0,
        "cell null context: the backend was **not** asked"
    );

    // A null buffer with a non-zero length is the same class of caller bug.
    let status = unsafe {
        write_shell(
            Some(&request.adapter),
            context,
            std::ptr::null(),
            0,
            4,
            false,
            false,
            std::ptr::addr_of_mut!(transferred),
            std::ptr::addr_of_mut!(record),
        )
    };
    assert_eq!(status, OURS_INVALID_PARAMETER, "cell null buffer: refused");
    assert_eq!(
        request.store.calls.load(Ordering::Relaxed),
        0,
        "cell null buffer: still not asked"
    );
}
