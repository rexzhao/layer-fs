//! The `Create` criterion for `FILE_CREATE` (write-chain batch, first cut).
//!
//! The read-only stage answered **every** create option by opening, which meant `FILE_CREATE` silently did nothing: the caller
//! believed it had created a file and the store disagreed. The criterion has to be able to tell those apart:
//!
//! - `FILE_CREATE` ⇒ **exactly one** `create_or_open` with `Disposition::Create`;
//! - `FILE_OPEN` ⇒ `Disposition::Open` (the translation must not have moved the other member);
//! - a backend that cannot create ⇒ **refusal**, never a `FileContext` that pretends the entry exists;
//! - an undefined disposition value (0x07) ⇒ refused, and **nothing is called**.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use fs_semantics::disposition::Disposition;
use fs_winfsp::abi::FileInfo;
use fs_winfsp::callbacks::{
    create_shell, split_create_options, FILE_CREATE, FILE_DIRECTORY_FILE, FILE_OPEN,
    NOT_IMPLEMENTED,
};
use fs_winfsp::ops::DynOps;
use fs_winfsp::ops::Path;
use fs_winfsp::ops::{
    Attributes, DirEntry, Kind, NodeInfo, OpenOutcome, OpsError, OpsResult, RenameOutcome,
    WorkspaceOps,
};
use fs_winfsp::DynAdapter;

/// Records the dispositions it was asked about, and can refuse to create — the two facts the criterion needs.
struct Recorder {
    seen: Mutex<Vec<Disposition>>,
    can_create: bool,
}

impl Recorder {
    fn read_only() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            can_create: false,
        })
    }

    fn writable() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            can_create: true,
        })
    }

    fn seen(&self) -> Vec<Disposition> {
        self.seen.lock().expect("recorder lock").clone()
    }
}

fn node() -> NodeInfo {
    NodeInfo {
        file_id: 5,
        kind: Kind::File,
        size: 0,
        times: fs_winfsp::file_info::times(1, 1, 1, 1),
        attributes: Attributes(0x80),
    }
}

impl WorkspaceOps for Recorder {
    fn create_or_open(&self, _path: &Path<'_>, disposition: Disposition) -> OpsResult<OpenOutcome> {
        self.seen.lock().expect("recorder lock").push(disposition);
        if disposition != Disposition::Open && !self.can_create {
            return Err(OpsError::ReadOnly);
        }
        let info = node();
        Ok(OpenOutcome {
            file_id: info.file_id,
            kind: info.kind,
            action: fs_semantics::disposition::Action::Open,
            size: info.size,
            times: info.times,
            attributes: info.attributes,
            binding: (info.file_id, Vec::new()),
            replaced: None,
        })
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
    fn set_size(&self, _file_id: u64, _len: u64) -> OpsResult<()> {
        Err(OpsError::ReadOnly)
    }
    fn read_dir(&self, _dir_id: u64) -> OpsResult<Vec<DirEntry>> {
        Ok(Vec::new())
    }
    fn get_info(&self, file_id: u64) -> OpsResult<NodeInfo> {
        if file_id == 5 {
            Ok(node())
        } else {
            Err(OpsError::NotFound)
        }
    }
    fn set_times(
        &self,
        _file_id: u64,
        _accessed: Option<i64>,
        _written: Option<i64>,
        _changed: Option<i64>,
        _created: Option<i64>,
    ) -> OpsResult<()> {
        Err(OpsError::ReadOnly)
    }
    fn set_attributes(&self, _file_id: u64, _attributes: Attributes) -> OpsResult<()> {
        Err(OpsError::ReadOnly)
    }
    fn rename(&self, _from: &Path<'_>, _to: &Path<'_>, _replace: bool) -> OpsResult<RenameOutcome> {
        Err(OpsError::ReadOnly)
    }
    fn remove_at(&self, _parent: u64, _name: &[u8], _expected_id: u64) -> OpsResult<()> {
        Err(OpsError::ReadOnly)
    }
    fn flush(&self) -> OpsResult<()> {
        Ok(())
    }
}

/// A shareable handle to the recorder plus the adapter the shell will be given.
///
/// The recorder is reached through [`Arc`] because the adapter **owns** its backend: without a second handle the test could not ask
/// what happened, and asserting the count through a fresh backend would prove nothing.
fn fixture(recorder: &Arc<Recorder>) -> DynAdapter {
    DynAdapter::new(Box::new(Shared(Arc::clone(recorder))) as DynOps)
}

/// Forwards every call to the shared recorder.
struct Shared(Arc<Recorder>);

impl WorkspaceOps for Shared {
    fn create_or_open(&self, path: &Path<'_>, disposition: Disposition) -> OpsResult<OpenOutcome> {
        self.0.create_or_open(path, disposition)
    }
    fn mkdir(&self, path: &Path<'_>) -> OpsResult<u64> {
        self.0.mkdir(path)
    }
    fn read(&self, file_id: u64, offset: u64, len: usize) -> OpsResult<Vec<u8>> {
        self.0.read(file_id, offset, len)
    }
    fn write(&self, file_id: u64, offset: u64, bytes: &[u8]) -> OpsResult<usize> {
        self.0.write(file_id, offset, bytes)
    }
    fn set_size(&self, file_id: u64, len: u64) -> OpsResult<()> {
        self.0.set_size(file_id, len)
    }
    fn read_dir(&self, dir_id: u64) -> OpsResult<Vec<DirEntry>> {
        self.0.read_dir(dir_id)
    }
    fn get_info(&self, file_id: u64) -> OpsResult<NodeInfo> {
        self.0.get_info(file_id)
    }
    fn set_times(
        &self,
        file_id: u64,
        accessed: Option<i64>,
        written: Option<i64>,
        changed: Option<i64>,
        created: Option<i64>,
    ) -> OpsResult<()> {
        self.0
            .set_times(file_id, accessed, written, changed, created)
    }
    fn set_attributes(&self, file_id: u64, attributes: Attributes) -> OpsResult<()> {
        self.0.set_attributes(file_id, attributes)
    }
    fn rename(&self, from: &Path<'_>, to: &Path<'_>, replace: bool) -> OpsResult<RenameOutcome> {
        self.0.rename(from, to, replace)
    }
    fn remove_at(&self, parent: u64, name: &[u8], expected_id: u64) -> OpsResult<()> {
        self.0.remove_at(parent, name, expected_id)
    }
    fn flush(&self) -> OpsResult<()> {
        self.0.flush()
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Runs one `Create`; returns `(status, whether a FileContext was handed out)`.
fn create(adapter: &DynAdapter, options: u32) -> (i32, bool) {
    let name = wide("/new.txt");
    let mut record = FileInfo::zeroed();
    let mut context: *mut c_void = std::ptr::null_mut();
    let status = unsafe {
        create_shell(
            Some(adapter),
            name.as_ptr(),
            options,
            std::ptr::addr_of_mut!(context),
            std::ptr::addr_of_mut!(record),
        )
    };
    let gave_context = !context.is_null();
    if gave_context {
        // Release it the way `close` does: the value is an **id**, so it goes back through the table rather than to `Box::from_raw`.
        fs_winfsp::callbacks::release_context_for_test(context);
    }
    (status, gave_context)
}

#[test]
fn file_create_asks_the_backend_to_create_exactly_once() {
    let recorder = Recorder::writable();
    let adapter = fixture(&recorder);
    let (status, gave_context) = create(&adapter, pack(FILE_CREATE, 0));

    assert_eq!(
        status, 0,
        "cell create: a backend that can create satisfies FILE_CREATE"
    );
    assert!(gave_context, "cell create: and the caller gets a handle");
    assert_eq!(
        recorder.seen(),
        vec![Disposition::Create],
        "cell create: **one** call, with Create — not zero (silently ignored) and not two"
    );
}

#[test]
fn file_open_still_means_open() {
    let recorder = Recorder::writable();
    let adapter = fixture(&recorder);
    let (status, _) = create(&adapter, pack(FILE_OPEN, 0));

    assert_eq!(status, 0, "cell open: FILE_OPEN succeeds on the fixture");
    assert_eq!(
        recorder.seen(),
        vec![Disposition::Open],
        "cell open: the translation did not move the other member of the class"
    );
}

#[test]
fn a_backend_that_cannot_create_refuses_instead_of_pretending() {
    let recorder = Recorder::read_only();
    let adapter = fixture(&recorder);
    let (status, gave_context) = create(&adapter, pack(FILE_CREATE, 0));

    assert_ne!(status, 0, "cell read-only: the create is refused");
    assert!(
        !gave_context,
        "cell read-only: **no FileContext** — a handle would tell the caller an entry exists that does not"
    );
    assert_eq!(
        recorder.seen(),
        vec![Disposition::Create],
        "cell read-only: the ask was made once and refused — not skipped, not retried"
    );
}

#[test]
fn an_undefined_disposition_value_is_refused_before_anything_is_called() {
    // `0x07` is not a disposition NT defines. The refusal must happen **before** the backend, so "not implemented" and "tried and
    // failed" stay distinguishable — a refusal that still asks once leaves a phantom attempt in every future diagnosis.
    let recorder = Recorder::writable();
    let adapter = fixture(&recorder);
    let (status, gave_context) = create(&adapter, pack(0x07, 0));

    assert_eq!(status, NOT_IMPLEMENTED, "cell undefined: refused");
    assert!(!gave_context, "cell undefined: and no handle is handed out");
    assert!(
        recorder.seen().is_empty(),
        "cell undefined: **the backend was not asked at all**"
    );
}

#[test]
fn the_disposition_lives_in_the_high_byte_and_the_flags_in_the_low_24_bits() {
    // The packing is `fsctl.h:351`'s one-line comment, and this is it as an assertion: a bare `0x21` from the FSD log is
    // `FILE_DIRECTORY_FILE` with **no** disposition, which is why reading the low bits never could have worked.
    let parts = split_create_options(pack(FILE_OPEN, FILE_DIRECTORY_FILE));
    assert_eq!(
        parts.disposition,
        Some(Disposition::Open),
        "cell split: the disposition comes from the high byte"
    );
    assert_eq!(
        parts.options, FILE_DIRECTORY_FILE,
        "cell split: and the flags from the low 24 bits"
    );

    // The bare value from the real log: flags only, disposition zero (= `FILE_SUPERSEDE`, which is why the old reading "worked" for
    // a directory open and would have been wrong for anything else).
    let bare = split_create_options(FILE_DIRECTORY_FILE | 0x20);
    assert_eq!(
        bare.options,
        FILE_DIRECTORY_FILE | 0x20,
        "cell bare: flags survive"
    );
    assert_eq!(
        bare.disposition,
        Some(Disposition::Supersede),
        "cell bare: with nothing in the high byte, the disposition is whatever zero means — **not** FILE_OPEN"
    );

    assert_eq!(
        split_create_options(pack(0x07, 0)).disposition,
        None,
        "cell undefined: an out-of-range disposition has no mapping"
    );
}

/// Packs a disposition and options the way the FSD does (`fsctl.h:351`).
fn pack(disposition: u32, options: u32) -> u32 {
    ((disposition & 0xff) << 24) | (options & 0x00FF_FFFF)
}

/// `FILE_CREATE | FILE_DIRECTORY_FILE` ⇒ **`mkdir`**, and the file door is never used.
///
/// The two halves of the packed word have to be read to get this right: the disposition (high byte) says "create", the option
/// (low bits) says "a directory". Reading either one alone gives the wrong verb — which is what the previous cut did.
#[test]
fn creating_a_directory_goes_through_mkdir_and_not_the_file_door() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts which door was used: `mkdir` or `create_or_open`.
    struct Doors {
        made_dirs: AtomicUsize,
        opened: AtomicUsize,
    }

    impl WorkspaceOps for Doors {
        fn create_or_open(
            &self,
            _path: &Path<'_>,
            _disposition: Disposition,
        ) -> OpsResult<OpenOutcome> {
            self.opened.fetch_add(1, Ordering::Relaxed);
            Err(OpsError::NotFound)
        }
        fn mkdir(&self, _path: &Path<'_>) -> OpsResult<u64> {
            let made = self.made_dirs.fetch_add(1, Ordering::Relaxed) + 1;
            Ok(100 + made as u64)
        }
        fn read(&self, _id: u64, _o: u64, _l: usize) -> OpsResult<Vec<u8>> {
            Err(OpsError::ReadOnly)
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
                kind: Kind::Directory,
                size: 0,
                times: fs_winfsp::file_info::times(1, 1, 1, 1),
                attributes: Attributes(0x10),
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
            Err(OpsError::ReadOnly)
        }
        fn set_attributes(&self, _id: u64, _a: Attributes) -> OpsResult<()> {
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

    let doors = Arc::new(Doors {
        made_dirs: AtomicUsize::new(0),
        opened: AtomicUsize::new(0),
    });

    struct SharedDoors(Arc<Doors>);
    impl WorkspaceOps for SharedDoors {
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

    let adapter = DynAdapter::new(Box::new(SharedDoors(Arc::clone(&doors))) as DynOps);
    let (status, gave_context) = create(&adapter, pack(FILE_CREATE, FILE_DIRECTORY_FILE));

    assert_eq!(status, 0, "cell dir: the directory was made");
    assert!(gave_context, "cell dir: and the caller gets a handle");
    assert_eq!(
        doors.made_dirs.load(Ordering::Relaxed),
        1,
        "cell dir: **mkdir was the door** — exactly once"
    );
    assert_eq!(
        doors.opened.load(Ordering::Relaxed),
        0,
        "cell dir: and the file door was never used (it has no kind argument, so it cannot make a directory)"
    );
}
