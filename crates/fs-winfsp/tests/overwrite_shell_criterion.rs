//! `Overwrite` (slot 5): truncate **in place**, then apply the attribute policy — and prove both directions of it.
//!
//! The two facts from `winfsp.h:326-346` that these tests pin down:
//!
//! - the entry keeps its **identity**: this is a truncation of the same file, not a new one (ruling (a) for `SUPERSEDE`, now at the
//!   mount layer);
//! - `ReplaceFileAttributes` chooses between **replace** and **merge (or)**. A shell that always replaced would pass a replace-only
//!   test, so both cells are asserted — the same "two-way" shape the G1 revision used for `SUPERSEDE`.
//!
//! `AllocationSize` is accepted and ignored, because this backend has no allocation concept; a test asserting otherwise would be
//! asserting a promise nobody keeps.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use fs_semantics::disposition::Disposition;
use fs_winfsp::abi::FileInfo;
use fs_winfsp::callbacks::{
    overwrite_disagreement_count, overwrite_shell, EntryContext, NOT_IMPLEMENTED,
    OURS_INVALID_PARAMETER,
};
use fs_winfsp::handles::HandleId;
use fs_winfsp::ops::DynOps;
use fs_winfsp::ops::Path;
use fs_winfsp::ops::{
    Attributes, DirEntry, Kind, NodeInfo, OpenOutcome, OpsError, OpsResult, RenameOutcome,
    WorkspaceOps,
};
use fs_winfsp::DynAdapter;

/// A one-file backend whose stored attributes and size the test can watch.
struct File {
    content: Mutex<Vec<u8>>,
    attributes: Mutex<u32>,
    size_calls: Arc<AtomicUsize>,
    /// When false, `set_size` keeps the old content (the silent-ignore backend).
    truncates: bool,
}

impl File {
    fn new(content: &[u8], attributes: u32) -> Arc<Self> {
        Arc::new(Self {
            content: Mutex::new(content.to_vec()),
            attributes: Mutex::new(attributes),
            size_calls: Arc::new(AtomicUsize::new(0)),
            truncates: true,
        })
    }

    fn ignoring(content: &[u8], attributes: u32) -> Arc<Self> {
        Arc::new(Self {
            content: Mutex::new(content.to_vec()),
            attributes: Mutex::new(attributes),
            size_calls: Arc::new(AtomicUsize::new(0)),
            truncates: false,
        })
    }

    fn stored_attributes(&self) -> u32 {
        *self.attributes.lock().expect("attributes")
    }

    fn content(&self) -> Vec<u8> {
        self.content.lock().expect("content").clone()
    }
}

fn node(size: u64, attributes: u32) -> NodeInfo {
    NodeInfo {
        file_id: 6,
        kind: Kind::File,
        size,
        times: fs_winfsp::file_info::times(1, 1, 1, 1),
        attributes: Attributes(attributes),
    }
}

impl WorkspaceOps for File {
    fn create_or_open(&self, _p: &Path<'_>, _d: Disposition) -> OpsResult<OpenOutcome> {
        Err(OpsError::ReadOnly)
    }
    fn mkdir(&self, _p: &Path<'_>) -> OpsResult<u64> {
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
    fn write(&self, _id: u64, _o: u64, _b: &[u8]) -> OpsResult<usize> {
        Err(OpsError::ReadOnly)
    }
    fn set_size(&self, _id: u64, len: u64) -> OpsResult<()> {
        self.size_calls.fetch_add(1, Ordering::Relaxed);
        if self.truncates {
            self.content
                .lock()
                .expect("content")
                .resize(usize::try_from(len).unwrap_or(usize::MAX), 0);
        }
        Ok(())
    }
    fn read_dir(&self, _id: u64) -> OpsResult<Vec<DirEntry>> {
        Ok(Vec::new())
    }
    fn get_info(&self, id: u64) -> OpsResult<NodeInfo> {
        let size = self.content.lock().expect("content").len() as u64;
        Ok(NodeInfo {
            file_id: id,
            ..node(size, self.stored_attributes())
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
    fn set_attributes(&self, _id: u64, attributes: Attributes) -> OpsResult<()> {
        *self.attributes.lock().expect("attributes") = attributes.0;
        Ok(())
    }
    fn rename(&self, _f: &Path<'_>, _t: &Path<'_>, _r: bool) -> OpsResult<RenameOutcome> {
        Err(OpsError::ReadOnly)
    }
    fn remove_at(&self, _p: u64, _n: &[u8], _e: u64) -> OpsResult<()> {
        Ok(())
    }
    fn flush(&self) -> OpsResult<()> {
        Ok(())
    }
}

/// Forwards to the shared `File`.
struct Shared(Arc<File>);

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

fn adapter(file: &Arc<File>) -> DynAdapter {
    DynAdapter::new(Box::new(Shared(Arc::clone(file))) as DynOps)
}

fn overwrite(adapter: &DynAdapter, attributes: u32, replace: bool) -> (i32, u64) {
    let context = fs_winfsp::callbacks::register_context_for_test(EntryContext {
        file_id: 6,
        handle: HandleId::from_raw(1),
    });
    let mut record = FileInfo::zeroed();
    let status = unsafe {
        overwrite_shell(
            Some(adapter),
            context,
            attributes,
            replace,
            1 << 20,
            std::ptr::addr_of_mut!(record),
        )
    };
    (status, record.file_size)
}

#[test]
fn overwrite_truncates_in_place_and_keeps_the_identity() {
    // The file is `id 6` before and after: a truncation, not a replacement. The read-back confirms the store agrees.
    let file = File::new(b"0123456789", 0x20);
    let adapter = adapter(&file);
    let (status, size) = overwrite(&adapter, 0x20, true);

    assert_eq!(status, 0, "cell truncate: success");
    assert_eq!(size, 0, "cell truncate: the record shows an empty file");
    assert_eq!(
        file.content(),
        Vec::<u8>::new(),
        "cell truncate: the content is gone"
    );
    assert_eq!(
        file.size_calls.load(Ordering::Relaxed),
        1,
        "cell truncate: one truncation call, not zero"
    );
}

#[test]
fn replace_attributes_true_stores_exactly_the_new_bits() {
    let file = File::new(b"x", 0x20); // ARCHIVE stored
    let adapter = adapter(&file);
    let (status, _) = overwrite(&adapter, 0x04, true); // SYSTEM requested
    assert_eq!(status, 0, "cell replace: success");
    assert_eq!(
        file.stored_attributes(),
        0x04,
        "cell replace: **replace** means the old bit is gone"
    );
}

#[test]
fn replace_attributes_false_merges_the_bits() {
    let file = File::new(b"x", 0x20); // ARCHIVE stored
    let adapter = adapter(&file);
    let (status, _) = overwrite(&adapter, 0x04, false); // SYSTEM requested
    assert_eq!(status, 0, "cell merge: success");
    assert_eq!(
        file.stored_attributes(),
        0x24,
        "cell merge: **merge** keeps the old bit — a shell that always replaced would fail exactly here"
    );
}

#[test]
fn a_normal_file_request_is_not_an_attribute_error() {
    // `FILE_ATTRIBUTE_NORMAL` (0x80) is *derived*, never stored: a caller passing the ordinary value must not be refused, and the stored
    // value must stay inside the writable set.
    let file = File::new(b"x", 0x20);
    let adapter = adapter(&file);
    let (status, _) = overwrite(&adapter, 0x80, true);
    assert_eq!(status, 0, "cell normal: accepted");
    assert_eq!(
        file.stored_attributes(),
        0,
        "cell normal: the derived bit is not stored"
    );
}

#[test]
fn a_backend_that_accepts_the_truncation_and_ignores_it_is_caught() {
    let file = File::ignoring(b"0123456789", 0x20);
    let adapter = adapter(&file);
    let before = overwrite_disagreement_count();
    let (status, _) = overwrite(&adapter, 0x20, true);

    assert_eq!(
        status, NOT_IMPLEMENTED,
        "cell ignore: refused — the size is still not zero"
    );
    assert_eq!(
        overwrite_disagreement_count() - before,
        1,
        "cell ignore: counted, not confused with an ordinary failure"
    );
}

#[test]
fn a_null_context_is_refused_before_the_backend_is_asked() {
    let file = File::new(b"x", 0x20);
    let adapter = adapter(&file);
    let mut record = FileInfo::zeroed();
    let status = unsafe {
        overwrite_shell(
            Some(&adapter),
            std::ptr::null(),
            0x20,
            true,
            0,
            std::ptr::addr_of_mut!(record),
        )
    };
    assert_eq!(status, OURS_INVALID_PARAMETER, "cell null: refused");
    assert_eq!(
        file.size_calls.load(Ordering::Relaxed),
        0,
        "cell null: the backend was **not** asked"
    );
}
