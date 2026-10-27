//! The `Create` criterion for `FILE_CREATE` (write-chain batch, first cut).
//!
//! The read-only stage answered **every** create option by opening, which meant `FILE_CREATE` silently did nothing: the caller
//! believed it had created a file and the store disagreed. The criterion has to be able to tell those apart:
//!
//! - `FILE_CREATE` ⇒ **exactly one** `create_or_open` with `Disposition::Create`;
//! - `FILE_OPEN` ⇒ `Disposition::Open` (the translation must not have moved the other member);
//! - a backend that cannot create ⇒ **refusal**, never a `FileContext` that pretends the entry exists;
//! - an unimplemented member (`FILE_OVERWRITE`) ⇒ refused, and **nothing is called**.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use fs_semantics::disposition::Disposition;
use fs_winfsp::abi::FileInfo;
use fs_winfsp::callbacks::{
    create_shell, EntryContext, FILE_CREATE, FILE_OPEN, FILE_OVERWRITE, NOT_IMPLEMENTED,
};
use fs_winfsp::ops::DynOps;
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
    fn create_or_open(&self, _path: &[u8], disposition: Disposition) -> OpsResult<OpenOutcome> {
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

    fn mkdir(&self, _path: &[u8]) -> OpsResult<u64> {
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
    fn rename(&self, _from: &[u8], _to: &[u8], _replace: bool) -> OpsResult<RenameOutcome> {
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
    fn create_or_open(&self, path: &[u8], disposition: Disposition) -> OpsResult<OpenOutcome> {
        self.0.create_or_open(path, disposition)
    }
    fn mkdir(&self, path: &[u8]) -> OpsResult<u64> {
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
    fn rename(&self, from: &[u8], to: &[u8], replace: bool) -> OpsResult<RenameOutcome> {
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
        // Release it the way `close` would: the shell allocated it.
        drop(unsafe { Box::from_raw(context.cast::<EntryContext>()) });
    }
    (status, gave_context)
}

#[test]
fn file_create_asks_the_backend_to_create_exactly_once() {
    let recorder = Recorder::writable();
    let adapter = fixture(&recorder);
    let (status, gave_context) = create(&adapter, FILE_CREATE);

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
    let (status, _) = create(&adapter, FILE_OPEN);

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
    let (status, gave_context) = create(&adapter, FILE_CREATE);

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
fn an_unimplemented_member_is_refused_before_anything_is_called() {
    let recorder = Recorder::writable();
    let adapter = fixture(&recorder);
    let (status, gave_context) = create(&adapter, FILE_OVERWRITE);

    assert_eq!(
        status, NOT_IMPLEMENTED,
        "cell overwrite: refused, because \"open and truncate\" is not implemented yet"
    );
    assert!(!gave_context, "cell overwrite: and no handle is handed out");
    assert!(
        recorder.seen().is_empty(),
        "cell overwrite: **the backend was not asked at all** — a refusal must not be a failed attempt"
    );
}
