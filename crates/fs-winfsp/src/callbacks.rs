//! The two callbacks of step ④, written so they can be **called directly in a test** before WinFsp ever sees them.
//!
//! `GetVolumeInfo` and `GetFileInfo` are the only ones wired at this stage, and neither needs mount-level mutable state
//! (ruling (c)):
//!
//! - the volume answer is **configuration**, and it now belongs to the mount (an `AdapterConfig`), not to the process;
//!   before `start`;
//! - the file answer needs only the entry being asked about, which WinFsp hands back to us as the `FileContext` we gave it
//!   at `Open`. `EntryContext { file_id }` is the **minimal** set: anything else here would be our state machine parked in
//!   somebody else's memory.
//!
//! Every callback body is safe Rust apart from reading the raw pointers it is given; the `unsafe` blocks are the pointer
//! dereferences and nothing else.

use std::ffi::c_void;

use crate::abi::{FileInfo, VolumeInfo};
use crate::ops::NodeInfo;

/// Status code used for "this build cannot answer": `STATUS_INVALID_DEVICE_REQUEST` is what a caller sees when an
/// operation is not implemented, and it is distinct from any of our own failures.
pub const NOT_IMPLEMENTED: i32 = 0xC000_0010u32 as i32;

/// The `FileContext` this frontend hands to WinFsp for an open entry.
///
/// Minimal on purpose: `file_id` is enough to answer `GetFileInfo`, and every extra field would be state kept in memory
/// WinFsp owns and frees on its own schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryContext {
    /// The entry the handle names.
    pub file_id: u64,
    /// The adapter-side handle: `Cleanup`/`Close` act on the **entry that was opened**, not on whatever the name points at when
    /// they arrive (a rename between open and close is the case that makes the difference visible).
    pub handle: crate::handles::HandleId,
}

/// Where the volume answer comes from: configuration set before the mount starts.
/// One directory entry as the callbacks see it: the name in UTF-16 (what WinFsp wants) and its metadata.
pub type ListEntry = (Vec<u16>, NodeInfo);

/// A directory source: `file_id -> entries`.
pub type Lister = fn(u64) -> Vec<ListEntry>;

/// The `ReadDirectory` **shell**: the adapter comes from the mount registry, or `None` when the pointer is not a live mount.
///
/// Same shape as [`get_volume_info_shell`]: the miss is in the type, and the entries come from **this mount's** adapter rather
/// than from a process-wide hook (352). `None` lists nothing — it does not fall back to whoever was configured last.
///
/// # Safety
///
/// `context` must point to `EntryContext`, `buffer` must be valid for writes of `length` bytes, `marker` (when not null) must
/// point to a NUL-terminated UTF-16 string, and `bytes_transferred` (when not null) must be writable.
pub unsafe fn read_directory_shell(
    adapter: Option<&crate::DynAdapter>,
    context: *const c_void,
    buffer: *mut c_void,
    marker: *const u16,
    length: u32,
    bytes_transferred: *mut u32,
) -> i32 {
    READ_DIR_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if context.is_null() || buffer.is_null() {
        eprintln!(
            "read_directory: null argument (context={:?} buffer={:?} length={})",
            context, buffer, length
        );
        return 0;
    }
    // SAFETY: the caller guarantees `context` came from our own `Box<EntryContext>`.
    let entry = unsafe { &*context.cast::<EntryContext>() };
    // The listing comes from **this mount's backend**, not from an injected hook: `DirEntry` now carries the entry's id, so its
    // metadata is one `get_info` away (367 — the field exists exactly so this derivation stops being impossible).
    let entries = match adapter {
        Some(adapter) => list_entries(adapter, entry.file_id),
        // No mount: an empty page. A miss must never fall back to another mount's listing (352).
        None => Vec::new(),
    };
    // SAFETY: the caller's contract covers every pointer handed through.
    unsafe { read_directory_from(&entries, buffer, marker, length, bytes_transferred) }
}

/// One directory as `ListEntry`s, taken from the mount's own backend.
///
/// A name that the backend cannot answer metadata for is **left out** rather than listed with invented numbers: a dirent record
/// with a made-up size/timestamp is worse than a missing row, because nothing downstream can tell the difference.
fn list_entries(adapter: &crate::DynAdapter, dir_id: u64) -> Vec<ListEntry> {
    let Ok(items) = adapter.backend().read_dir(dir_id) else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|item| {
            let info = adapter.backend().get_info(item.file_id).ok()?;
            Some((
                String::from_utf8_lossy(&item.name)
                    .encode_utf16()
                    .collect::<Vec<u16>>(),
                info,
            ))
        })
        .collect()
}

/// The `ReadDirectory` body with the entries handed in — the resume, the framing and the count are all here.
///
/// # Safety
///
/// `buffer` must be valid for writes of `length` bytes, `marker` (when not null) must point to a NUL-terminated UTF-16 string,
/// and `bytes_transferred` (when not null) must be writable.
pub unsafe fn read_directory_from(
    entries: &[ListEntry],
    buffer: *mut c_void,
    marker: *const u16,
    length: u32,
    bytes_transferred: *mut u32,
) -> i32 {
    // A non-null marker means the caller wants to resume after that name; we index by position, so translate it.
    let start = if marker.is_null() {
        0usize
    } else {
        // SAFETY: WinFsp passes a NUL-terminated UTF-16 name.
        let name_units = unsafe { crate::boundary::utf16_len(marker) };
        // SAFETY: same pointer, bounded by the terminator just measured.
        let name = unsafe { std::slice::from_raw_parts(marker, name_units) };
        entries
            .iter()
            .position(|(candidate, _)| candidate.as_slice() == name)
            .map_or(0, |index| index + 1)
    };

    // SAFETY: `buffer` is valid for `length` bytes per the caller's contract.
    let bytes = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), length as usize) };
    let mut out = crate::dir_buffer::DirBuffer::new(bytes);
    for (name, info) in entries.iter().skip(start) {
        match crate::file_info::file_info(info) {
            Ok(record) => {
                if !out.push(name, &record) {
                    // The next record does not fit: stop with the complete ones, which is what WinFsp expects.
                    break;
                }
            }
            // A record that cannot be built (a timestamp outside FILETIME) is skipped rather than truncating the listing.
            Err(_) => continue,
        }
    }
    // The count is the **only** way the OS learns that entries were produced: without it a perfectly filled buffer reads as an
    // empty directory (which is exactly what `dir Z:` showed while the chain itself was green).
    if !bytes_transferred.is_null() {
        // SAFETY: writable out-pointer per WinFsp's contract.
        unsafe {
            std::ptr::write(
                bytes_transferred,
                u32::try_from(out.written()).unwrap_or(u32::MAX),
            )
        };
    }
    0
}
/// The `GetVolumeInfo` body: writes the configured `VolumeInfo` into the caller's buffer.
///
/// `capacity` is the size in bytes of the buffer WinFsp passed; a smaller buffer than our structure is refused rather than
/// partially filled — a half-written structure would be read as garbage.
///
/// # Safety
///
/// `out` must be valid for writes of `capacity` bytes for the duration of the call.
/// The `GetVolumeInfo` **shell**: the adapter comes from the mount registry, or `None` when the pointer is not a live mount.
///
/// `Option<&DynAdapter>` puts the miss **in the type**: the refusal path exists in the signature rather than in a fallback branch,
/// so "no adapter" cannot quietly become "the last configuration" (352).
///
/// # Safety
///
/// `out` must be valid for writes of `capacity` bytes for the duration of the call.
pub unsafe fn get_volume_info_shell(
    adapter: Option<&crate::DynAdapter>,
    out: *mut c_void,
    capacity: u32,
) -> i32 {
    VOLUME_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let volume = adapter.and_then(|adapter| adapter.config().volume);
    // SAFETY: the caller's contract covers `out`.
    unsafe { get_volume_info_for(volume, out, capacity) }
}

/// The `GetVolumeInfo` body with the volume handed in: **the logic lives here, the process-wide lookup lives above** (361).
///
/// Splitting it this way is what makes the two-phase contract testable without a mount: the size query, the "too small" answer,
/// and the "no volume configured" answer are all reachable by passing `None` or a small buffer — the `unsafe` shell keeps only
/// the parts that genuinely need an FFI call.
///
/// # Safety
///
/// `out` must be valid for writes of `capacity` bytes for the duration of the call.
pub unsafe fn get_volume_info_for(
    volume: Option<VolumeInfo>,
    out: *mut c_void,
    capacity: u32,
) -> i32 {
    if out.is_null() {
        return OURS_INVALID_PARAMETER;
    }
    if (capacity as usize) < std::mem::size_of::<VolumeInfo>() {
        // Two-phase, like `GetSecurityByName`: a buffer that is too small gets a **"too small"**, not "unsupported" — WinFsp
        // retries with a bigger one, whereas `NOT_IMPLEMENTED` is terminal and is what the log showed
        // (`<<QueryVolumeInformation IoStatus=c0000010`).
        return OURS_BUFFER_TOO_SMALL;
    }
    let Some(volume) = volume else {
        // The order matters and is asserted by a test: **"no volume configured" is decided before the buffer is trusted**, so a
        // configured-but-absent volume cannot be mistaken for a caller that passed a bad pointer.
        return NOT_IMPLEMENTED;
    };
    // SAFETY: `out` is non-null and large enough for `VolumeInfo`, per the check above.
    unsafe { std::ptr::write(out.cast::<VolumeInfo>(), volume) };
    0
}

/// The `SetFileSize` **shell**: ask the mount's backend to resize, then answer with the entry's **current** record.
///
/// The two steps are deliberately separate: "the backend accepted a length" and "the entry now has that length" are different
/// facts, and the record handed back is built from a fresh `get_info`, so a backend that silently ignores the request cannot make
/// the caller believe it worked (382 — the write-side mirror of "zero records read as an empty file").
///
/// # Safety
///
/// `context` must point to an `EntryContext` owned by this frontend; `out` must be valid for writes of `size_of::<FileInfo>()`.
pub unsafe fn set_file_size_shell(
    adapter: Option<&crate::DynAdapter>,
    context: *const c_void,
    new_size: u64,
    out: *mut FileInfo,
) -> i32 {
    SET_SIZE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if context.is_null() || out.is_null() {
        return OURS_INVALID_PARAMETER;
    }
    // SAFETY: the caller guarantees `context` came from our own `Box<EntryContext>`.
    let entry = unsafe { &*context.cast::<EntryContext>() };
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    if adapter.backend().set_size(entry.file_id, new_size).is_err() {
        // Nothing is written into the caller's buffer on failure: a half-updated record is worse than none.
        return NOT_IMPLEMENTED;
    }
    // Read back through the **same** backend: this is where "accepted" becomes "is".
    let Some(node) = adapter.backend().get_info(entry.file_id).ok() else {
        return NOT_IMPLEMENTED;
    };
    if node.size != new_size {
        // The backend said yes and the store says otherwise. Refusing is the only honest answer, and it is counted so the two
        // facts cannot be confused later.
        SET_SIZE_DISAGREEMENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return NOT_IMPLEMENTED;
    }
    match crate::file_info::file_info(&node) {
        Ok(record) => {
            // SAFETY: `out` is non-null; the caller guarantees room for `FileInfo`.
            unsafe { std::ptr::write(out.cast::<FileInfo>(), record) };
            0
        }
        Err(_) => NOT_IMPLEMENTED,
    }
}

/// How many times `SetFileSize` was entered.
static SET_SIZE_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times a backend accepted a size and then reported a different one.
///
/// Zero is the invariant: a non-zero value means the store disagrees with the last successful call, which is a defect in one of the
/// two, not a transient.
static SET_SIZE_DISAGREEMENTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// How many times `SetFileSize` was entered.
#[must_use]
pub fn set_size_call_count() -> usize {
    SET_SIZE_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many times a backend accepted a length and then answered with a different one.
#[must_use]
pub fn set_size_disagreement_count() -> usize {
    SET_SIZE_DISAGREEMENTS.load(std::sync::atomic::Ordering::Relaxed)
}

/// The `GetFileInfo` **shell**: the metadata comes from **this mount's backend**, not from a process-wide lookup hook.
///
/// # Safety
///
/// `context` must point to an `EntryContext` owned by this frontend, and `out` must be valid for writes of
/// `size_of::<FileInfo>()` bytes.
pub unsafe fn get_file_info_shell(
    adapter: Option<&crate::DynAdapter>,
    context: *const c_void,
    out: *mut c_void,
) -> i32 {
    if context.is_null() || out.is_null() {
        return NOT_IMPLEMENTED;
    }
    // SAFETY: the caller guarantees `context` came from our own `Box<EntryContext>`.
    let entry = unsafe { &*context.cast::<EntryContext>() };
    // No mount ⇒ no metadata. `None` in the type is the refusal, not a reason to ask somebody else's configuration (352).
    let info = adapter.and_then(|adapter| adapter.backend().get_info(entry.file_id).ok());
    let Some(info) = info else {
        return NOT_IMPLEMENTED;
    };
    match crate::file_info::file_info(&info) {
        Ok(record) => {
            // SAFETY: `out` is non-null; the caller guarantees room for `FileInfo`.
            unsafe { std::ptr::write(out.cast::<FileInfo>(), record) };
            0
        }
        Err(_) => NOT_IMPLEMENTED,
    }
}
/// Live `FileContext` allocations, and how many have been released.
///
/// Counted rather than merely asserted: when a mount crashes, the log's last numbers are the only evidence of whether contexts
/// leaked or were freed twice — and a crash is exactly when `debug_assert` is too late.
static CONTEXTS_ALLOCATED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static CONTEXTS_FREED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `(allocated, freed)`: equal at a clean shutdown, and printed by the mount test before it tears down.
#[must_use]
pub fn context_counts() -> (usize, usize) {
    (
        CONTEXTS_ALLOCATED.load(std::sync::atomic::Ordering::Relaxed),
        CONTEXTS_FREED.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Which early exit `open` took, counted per branch.
///
/// The three exits all return the same status code, so a status cannot say which one fired — the counters can. Order:
/// `(no_resolver, no_entry, bad_record)`.
static OPEN_EXITS: [std::sync::atomic::AtomicUsize; 3] = [
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
];

/// How many times each early exit of `open` was taken.
#[must_use]
pub fn open_exit_counts() -> (usize, usize, usize) {
    (
        OPEN_EXITS[0].load(std::sync::atomic::Ordering::Relaxed),
        OPEN_EXITS[1].load(std::sync::atomic::Ordering::Relaxed),
        OPEN_EXITS[2].load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// The `GetSecurity` body (slot 16, `QuerySecurity`'s first half): the same two-phase answer as `GetSecurityByName`, without a
/// name — the entry is already known by `FileContext`.
///
/// # Safety
///
/// `context` must be our `EntryContext`; `descriptor_out`/`descriptor_size_out` follow the two-phase contract.
pub unsafe fn get_security(
    context: *const c_void,
    descriptor_out: *mut c_void,
    descriptor_size_out: *mut usize,
) -> i32 {
    if context.is_null() {
        return OURS_INVALID_PARAMETER;
    }
    if !descriptor_size_out.is_null() {
        // SAFETY: writable out-parameter.
        unsafe { std::ptr::write(descriptor_size_out, 0) };
    }
    let _ = descriptor_out;
    0
}

/// How many times `ReadDirectory` was entered — the counter that separates "our callback ran and answered empty" from "the
/// slot was never called".
static READ_DIR_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times `ReadDirectory` was entered.
#[must_use]
pub fn read_dir_call_count() -> usize {
    READ_DIR_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many times `open` was actually entered (independent of how it exited).
static OPEN_ENTRIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times `open` was entered.
#[must_use]
pub fn open_entry_count() -> usize {
    OPEN_ENTRIES.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many times `Create` was called.
static CREATE_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times the create path has been asked.
#[must_use]
pub fn create_call_count() -> usize {
    CREATE_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// The `Open` **shell**: the name is resolved by **this mount's backend**, not by a process-wide resolver.
///
/// Behaviour is the same two steps the old body took (resolve, then fetch metadata) — but the steps now happen inside the backend,
/// so `fs-winfsp` no longer needs to know how a name becomes an id (367's route, cashed in).
///
/// # Safety
///
/// `file_name` must be NUL-terminated; `context_out` and `info_out` must be valid for writes of a pointer and a `FileInfo`.
pub unsafe fn open_shell(
    adapter: Option<&crate::DynAdapter>,
    file_name: *const u16,
    context_out: *mut *mut c_void,
    info_out: *mut FileInfo,
) -> i32 {
    // SAFETY: the caller's contract is `open_like`'s.
    unsafe {
        open_like(
            adapter,
            file_name,
            fs_semantics::disposition::Disposition::Open,
            // `Open` has no create options at all: a plain open never asks for a directory to be made.
            false,
            context_out,
            info_out,
        )
    }
}

/// The one body behind `Open` and `Create`: resolve the name **with a disposition**, then hand back a `FileContext`.
///
/// The disposition is the whole difference between the two callbacks: `Open` says "must exist", `Create` says what the create
/// options asked for. Writing it once is what keeps them from drifting apart — the read-only stage answered `Create` by calling
/// `open`, so any disposition other than `Open` was silently ignored rather than refused.
///
/// # Safety
///
/// `file_name` must be NUL-terminated; `context_out` and `info_out` must be valid for writes of a pointer and a `FileInfo`.
unsafe fn open_like(
    adapter: Option<&crate::DynAdapter>,
    file_name: *const u16,
    disposition: fs_semantics::disposition::Disposition,
    wants_directory: bool,
    context_out: *mut *mut c_void,
    info_out: *mut FileInfo,
) -> i32 {
    OPEN_ENTRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if file_name.is_null() || context_out.is_null() || info_out.is_null() {
        eprintln!(
            "open: null argument (file_name={:?} context_out={:?} info_out={:?})",
            file_name, context_out, info_out
        );
        return OURS_INVALID_PARAMETER;
    }
    // SAFETY: `file_name` is NUL-terminated per WinFsp's contract.
    let units = unsafe { crate::boundary::utf16_len(file_name) };
    // SAFETY: same pointer, bounded by the terminator.
    let slice = unsafe { std::slice::from_raw_parts(file_name, units) };
    let path = String::from_utf16_lossy(slice);
    let Some(adapter) = adapter else {
        OPEN_EXITS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return NOT_IMPLEMENTED;
    };
    // The disposition is passed through **unchanged** for a file: whether it creates or opens is the backend's decision, and the
    // promise that `Open` does not create lives there (asserted in `open_shell_criterion`).
    // A **directory** is different: `mkdir` is the only verb that can make one, so the decision is taken here and said out loud.
    let outcome = match make_verb(disposition, wants_directory) {
        MakeVerb::Directory => match adapter.backend().mkdir(path.as_bytes()) {
            // A directory made by `mkdir` has **no binding from the backend** (the trait answers only an id), so a pending deletion of
            // a directory would have no name to remove. Recorded as a gap: opening a directory by path is the missing trait capability.
            Ok(file_id) => adapter
                .backend()
                .get_info(file_id)
                .map(|_| (file_id, None))
                .map_err(|_| OURS_OBJECT_NAME_NOT_FOUND),
            // The name is taken: `FILE_CREATE` means **fail if it exists**, and that is a collision, not a silent open.
            Err(crate::ops::OpsError::NameCollision) => Err(OURS_OBJECT_NAME_COLLISION),
            Err(_) => Err(OURS_OBJECT_NAME_NOT_FOUND),
        },
        MakeVerb::File => adapter
            .backend()
            .create_or_open(path.as_bytes(), disposition)
            // The backend's own answer carries where the name lives — that is the binding `remove_at` will guard with.
            .map(|outcome| (outcome.file_id, Some(outcome.binding)))
            .map_err(|_| OURS_OBJECT_NAME_NOT_FOUND),
    };
    let (file_id, binding) = match outcome {
        Ok(pair) => pair,
        Err(status) => {
            OPEN_EXITS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return status;
        }
    };
    let _ = disposition;
    // The record is built from **current** metadata, read at return time — not from whatever the outcome remembered.
    let Some(node) = adapter.backend().get_info(file_id).ok() else {
        OPEN_EXITS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return OURS_OBJECT_NAME_NOT_FOUND;
    };
    let Ok(record) = crate::file_info::file_info(&node) else {
        OPEN_EXITS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return NOT_IMPLEMENTED;
    };
    // Register the handle **after** the record is built: a refused open must not leave a handle behind, and the table is the thing
    // `Cleanup`/`Close` will act on. Coming last also means the earlier refusal paths need no undo.
    let Ok(handle) = adapter.open_handle(file_id, binding) else {
        OPEN_EXITS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return NOT_IMPLEMENTED;
    };
    let context = Box::into_raw(Box::new(EntryContext { file_id, handle }));
    CONTEXTS_ALLOCATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // SAFETY: both out-pointers were checked non-null and are valid for their writes per the caller's contract.
    unsafe {
        std::ptr::write(context_out, context.cast::<c_void>());
        std::ptr::write(info_out, record);
    }
    0
}

/// The `Create` **shell**: WinFsp's create options decide the disposition, and the backend does the rest.
///
/// Only the two options the read-only stage can honour are translated:
///
/// - `FILE_OPEN` ("must exist") → `Disposition::Open`;
/// - `FILE_CREATE` ("create, fail if it exists") → `Disposition::Create`.
///
/// Anything else is **refused**, not approximated. Answering `FILE_OVERWRITE` by opening would be the "looks successful, did
/// nothing" failure the write batch exists to avoid — and a mount that silently ignores a truncation request is worse than one
/// that reports it cannot.
///
/// # Safety
///
/// Same contract as [`open_shell`].
pub unsafe fn create_shell(
    adapter: Option<&crate::DynAdapter>,
    file_name: *const u16,
    create_options: u32,
    context_out: *mut *mut c_void,
    info_out: *mut FileInfo,
) -> i32 {
    CREATE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let parts = split_create_options(create_options);
    let Some(disposition) = parts.disposition else {
        // A disposition value NT does not define: refuse, and do not ask the backend anything.
        return NOT_IMPLEMENTED;
    };
    // `FILE_DIRECTORY_FILE` is the **option**, not the disposition — it says *what kind* to make, and it is the flag the SDK tells
    // user-mode file systems to look at (`winfsp.h:262-266`).
    let wants_directory = parts.options & FILE_DIRECTORY_FILE != 0;
    // SAFETY: the caller's contract for both pointers is the same as `open_shell`'s.
    unsafe {
        open_like(
            adapter,
            file_name,
            disposition,
            wants_directory,
            context_out,
            info_out,
        )
    }
}

/// Which verb to use for a create request.
///
/// A directory is made with `mkdir` and nothing else — `create_or_open` has **no kind argument**, because the file door is the only
/// place a name becomes an id. That split is the reason `FILE_DIRECTORY_FILE` had to be extracted from the low 24 bits at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MakeVerb {
    /// `create_or_open(path, disposition)` — a file, opened or created by the disposition.
    File,
    /// `mkdir(path)` — a **new** directory (the disposition asked for creation).
    Directory,
}

/// The decision, as a function: `FILE_DIRECTORY_FILE` + a creating disposition ⇒ `mkdir`.
///
/// Opening an existing directory keeps going through `create_or_open(Open)`, because that is the only path→id door the trait has;
///
/// **a directory's id is not reachable any other way** (finding recorded when this was written).
#[must_use]
fn make_verb(
    disposition: fs_semantics::disposition::Disposition,
    wants_directory: bool,
) -> MakeVerb {
    use fs_semantics::disposition::Disposition;
    match (wants_directory, disposition) {
        (true, Disposition::Create) => MakeVerb::Directory,
        _ => MakeVerb::File,
    }
}

/// The two halves of a packed `create_options` word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateParts {
    /// The disposition (`FILE_OPEN`, `FILE_CREATE`, …), or `None` when the value is not one NT defines.
    pub disposition: Option<fs_semantics::disposition::Disposition>,
    /// The low 24 bits: `FILE_DIRECTORY_FILE`, `FILE_NON_DIRECTORY_FILE`, `FILE_WRITE_THROUGH`, …
    pub options: u32,
}

/// Splits the packed `CreateOptions` word — **`Disposition: high 8 bits; Options: low 24 bits`** (`fsctl.h:351`).
///
/// The packing is documented in one comment line in the transact structure, and the dispatcher itself does exactly this
/// (`fsop.c:915`: `switch ((CreateOptions >> 24) & 0xff)`), passing the **packed** value through to the interface (`fsop.c:455`).
/// Reading the low bits as the disposition is the mistake this function exists to make impossible: the FSD's own trace prints the
/// two halves separately, so a bare `0x21` in a log is `FILE_DIRECTORY_FILE` and says nothing about the disposition.
#[must_use]
pub fn split_create_options(create_options: u32) -> CreateParts {
    use fs_semantics::disposition::Disposition;
    let disposition = match (create_options >> 24) & 0xff {
        FILE_SUPERSEDE => Some(Disposition::Supersede),
        FILE_OPEN => Some(Disposition::Open),
        FILE_CREATE => Some(Disposition::Create),
        FILE_OPEN_IF => Some(Disposition::OpenIf),
        FILE_OVERWRITE => Some(Disposition::Overwrite),
        FILE_OVERWRITE_IF => Some(Disposition::OverwriteIf),
        _ => None,
    };
    CreateParts {
        disposition,
        options: create_options & 0x00FF_FFFF,
    }
}

/// `FILE_SUPERSEDE` — `winfsp.h` (`CreateOptions`), the first member of the create-disposition class.
pub const FILE_SUPERSEDE: u32 = 0x00;
/// `FILE_OPEN` — "must exist".
pub const FILE_OPEN: u32 = 0x01;
/// `FILE_CREATE` — "create; fail if it exists".
pub const FILE_CREATE: u32 = 0x02;
/// `FILE_OPEN_IF` — "open if it exists, otherwise create".
pub const FILE_OPEN_IF: u32 = 0x03;
/// `FILE_OVERWRITE` — "open and truncate; fail if it does not exist".
pub const FILE_OVERWRITE: u32 = 0x04;
/// `FILE_OVERWRITE_IF` — "open and truncate, creating if needed".
pub const FILE_OVERWRITE_IF: u32 = 0x05;

/// `FILE_DIRECTORY_FILE` — an **option** bit (`0x00000001`), not a disposition: "create a directory rather than a file".
///
/// The collision with `FILE_OPEN`'s value is exactly what made the low-bit reading look plausible and be wrong: the two live in
/// different halves of the same word.
pub const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;

/// The `Close` body: frees the `FileContext` **once**.
///
/// # Safety
///
/// `context` must have come from [`open`] and must not be used afterwards.
/// `FSP_CLEANUP_DELETE` — `winfsp.h` (`Cleanup` flags): "delete the file when the last handle is closed".
pub const FSP_CLEANUP_DELETE: u32 = 0x0000_0001;

/// The `Cleanup` **shell** — phase one of two-phase deletion: **mark**, do not delete.
///
/// Windows calls `Cleanup` when the **last user handle** to the entry goes away, and calls `Close` afterwards. The deletion itself
/// therefore belongs to `Close`, because only then is the adapter's own handle count the last word — and that ordering is what makes
/// "delete on the last close" true for the OS rather than for us.
///
/// # Safety
///
/// `context` must point to an `EntryContext` owned by this frontend.
pub unsafe fn cleanup_shell(
    adapter: Option<&crate::DynAdapter>,
    context: *const c_void,
    flags: u32,
) -> i32 {
    CLEANUP_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if context.is_null() {
        return OURS_INVALID_PARAMETER;
    }
    if flags & FSP_CLEANUP_DELETE == 0 {
        // A cleanup for any other reason (a handle simply closing) has nothing to mark.
        return 0;
    }
    // SAFETY: the caller guarantees `context` came from our own `Box<EntryContext>`.
    let entry = unsafe { &*context.cast::<EntryContext>() };
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    match adapter.set_delete(entry.handle) {
        Ok(()) => 0,
        // No `DELETE` access on this handle, or the handle is unknown: a typed refusal, not a silent success that would leave the
        // caller believing the entry is going away.
        Err(_) => NOT_IMPLEMENTED,
    }
}

/// How many times `Cleanup` was entered.
static CLEANUP_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times `Cleanup` was entered.
#[must_use]
pub fn cleanup_call_count() -> usize {
    CLEANUP_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// The `Close` **shell**: releases the handle, and performs the entry's removal **once** if it was marked.
///
/// The `Box<EntryContext>` is freed in every path — a `Close` that fails must still not leak the context, because the caller has no
/// way to retry it.
///
/// # Safety
///
/// Same contract as [`close`]: `context` came from `Box::into_raw` in `open_like`, and `Close` happens once per open.
pub unsafe fn close_shell(adapter: Option<&crate::DynAdapter>, context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: the caller guarantees this pointer came from `Box::into_raw` in `open_like`.
    let entry = unsafe { Box::from_raw(context.cast::<EntryContext>()) };
    if let Some(adapter) = adapter {
        // The adapter performs the single `remove_at` (outside its table lock, see `Adapter::close`). A failure here is recorded by
        // the backend's own error, and the context is freed either way.
        let _ = adapter.close(entry.handle);
    }
    CONTEXTS_FREED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The `Overwrite` **shell**: truncate in place, apply the attribute policy, then **read both back**.
///
/// The two header facts this is built on (`winfsp.h:326-346`):
///
/// - the entry keeps its **identity** — this is a truncation of the same file, not a new one (the same decision ruling (a) made for
///   `SUPERSEDE` in the core, now at the mount layer);
/// - `ReplaceFileAttributes` decides between **replace** and **merge (or)**: `TRUE` stores the caller's bits, `FALSE` ors them into what
///   is stored. Both directions are asserted, because a shell that always replaced would pass a replace-only test.
///
/// `AllocationSize` is accepted and **ignored**: this backend has no allocation concept, and pretending to honour it would be a
/// promise nobody keeps.
///
/// # Safety
///
/// `context` must point to an `EntryContext` owned by this frontend; `out` (when not null) must be writable.
pub unsafe fn overwrite_shell(
    adapter: Option<&crate::DynAdapter>,
    context: *const c_void,
    file_attributes: u32,
    replace_attributes: bool,
    allocation_size: u64,
    out: *mut FileInfo,
) -> i32 {
    OVERWRITE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if context.is_null() {
        return OURS_INVALID_PARAMETER;
    }
    // SAFETY: the caller guarantees `context` came from our own `Box<EntryContext>`.
    let entry = unsafe { &*context.cast::<EntryContext>() };
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    let before = match adapter.backend().get_info(entry.file_id) {
        Ok(info) => info,
        Err(_) => return OURS_OBJECT_NAME_NOT_FOUND,
    };
    // Truncation first: it is the operation the caller actually asked for, and a failed attribute write must not leave a file that is
    // neither old nor new.
    if adapter.backend().set_size(entry.file_id, 0).is_err() {
        return NOT_IMPLEMENTED;
    }
    // `FILE_ATTRIBUTE_NORMAL` (0x80) and `DIRECTORY` (0x10) are **derived**, never stored: masking them off here is what keeps a caller
    // that passes the ordinary "normal file" value from being refused by the attribute validator.
    let requested = file_attributes & fs_semantics::attributes::WRITABLE;
    let stored = if replace_attributes {
        requested
    } else {
        before.attributes.0 & fs_semantics::attributes::WRITABLE | requested
    };
    if adapter
        .backend()
        .set_attributes(entry.file_id, crate::ops::Attributes(stored))
        .is_err()
    {
        return NOT_IMPLEMENTED;
    }
    let after = match adapter.backend().get_info(entry.file_id) {
        Ok(info) => info,
        Err(_) => return NOT_IMPLEMENTED,
    };
    // Read both back: the size **and** the attribute bits, since "replace vs merge" is the whole question this callback asks.
    if after.size != 0 || after.attributes.0 & fs_semantics::attributes::WRITABLE != stored {
        OVERWRITE_DISAGREEMENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return NOT_IMPLEMENTED;
    }
    let _ = allocation_size;
    if !out.is_null() {
        match crate::file_info::file_info(&after) {
            Ok(record) => {
                // SAFETY: `out` is non-null and the caller guarantees room for `FileInfo`.
                unsafe { std::ptr::write(out.cast::<FileInfo>(), record) };
            }
            Err(_) => return NOT_IMPLEMENTED,
        }
    }
    0
}

/// How many times `Overwrite` was entered.
static OVERWRITE_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times an overwrite was accepted and then contradicted by the store (size or attributes).
static OVERWRITE_DISAGREEMENTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// How many times `Overwrite` was entered.
#[must_use]
pub fn overwrite_call_count() -> usize {
    OVERWRITE_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many times an overwrite left the store in a state that does not match what was asked.
#[must_use]
pub fn overwrite_disagreement_count() -> usize {
    OVERWRITE_DISAGREEMENTS.load(std::sync::atomic::Ordering::Relaxed)
}
/// The `Write` **shell**: write, then **read back** — "the backend accepted it" is not "the bytes are there" (021ed97's rule, applied
/// to the operation where it matters most).
///
/// Three details come straight from the header (`winfsp.h:452-466`) and each is a way to be silently wrong:
///
/// - `WriteToEndOfFile` ⇒ `Offset` is **-1** and the write goes to the current end of file;
/// - `ConstrainedIo` ⇒ the file **must not grow**: a write past the end is truncated, not extended;
/// - `*PBytesTransferred` is the caller's only evidence that anything happened.
///
/// # Safety
///
/// `context` must point to an `EntryContext` owned by this frontend; `buffer` must be valid for reads of `length` bytes;
/// `bytes_written` (when not null) and `out` (when not null) must be writable.
/// The argument count is the callback's own shape (`winfsp.h:471-475`): flattening it into a struct here would only move the same
/// nine things somewhere else, and the FFI adapter would have to unpack them again.
#[allow(clippy::too_many_arguments)]
pub unsafe fn write_shell(
    adapter: Option<&crate::DynAdapter>,
    context: *const c_void,
    buffer: *const c_void,
    offset: u64,
    length: u32,
    write_to_end_of_file: bool,
    constrained_io: bool,
    bytes_written: *mut u32,
    out: *mut FileInfo,
) -> i32 {
    WRITE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if context.is_null() || (buffer.is_null() && length > 0) {
        return OURS_INVALID_PARAMETER;
    }
    // SAFETY: the caller guarantees `context` came from our own `Box<EntryContext>`.
    let entry = unsafe { &*context.cast::<EntryContext>() };
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    let before = match adapter.backend().get_info(entry.file_id) {
        Ok(info) => info,
        Err(_) => return OURS_OBJECT_NAME_NOT_FOUND,
    };
    // `WriteToEndOfFile` is signalled by `Offset == u64::MAX`; the header says so explicitly.
    let mut at = if write_to_end_of_file {
        before.size
    } else {
        offset
    };
    let mut len = length as usize;
    if constrained_io {
        // "Must not extend the file": keep whatever fits and report how much was taken. Reporting success while growing the file
        // would break the one promise this flag makes.
        let room = before.size.saturating_sub(at);
        len = len.min(usize::try_from(room).unwrap_or(usize::MAX));
    }
    // SAFETY: `buffer` is valid for `length >= len` bytes per the caller's contract.
    let bytes = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), len) };
    let written = match adapter.backend().write(entry.file_id, at, bytes) {
        Ok(written) => written.min(len),
        Err(_) => return NOT_IMPLEMENTED,
    };
    // Read the bytes **back** and compare: the size alone cannot catch a dropped write that stayed inside the file, and that is
    // exactly the silent case this check exists for (`before.size` and `after.size` are equal whether the bytes landed or not).
    let echo = match adapter.backend().read(entry.file_id, at, written) {
        Ok(bytes) => bytes,
        Err(_) => return NOT_IMPLEMENTED,
    };
    let after = match adapter.backend().get_info(entry.file_id) {
        Ok(info) => info,
        Err(_) => return NOT_IMPLEMENTED,
    };
    let expected = before.size.max(at.saturating_add(written as u64));
    if echo.as_slice() != &bytes[..written.min(bytes.len())] || after.size != expected {
        WRITE_DISAGREEMENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return NOT_IMPLEMENTED;
    }
    if !bytes_written.is_null() {
        // SAFETY: writable out-pointer per WinFsp's contract.
        unsafe { std::ptr::write(bytes_written, u32::try_from(written).unwrap_or(u32::MAX)) };
    }
    if !out.is_null() {
        match crate::file_info::file_info(&after) {
            Ok(record) => {
                // SAFETY: `out` is non-null and the caller guarantees room for `FileInfo`.
                unsafe { std::ptr::write(out.cast::<FileInfo>(), record) };
            }
            Err(_) => return NOT_IMPLEMENTED,
        }
    }
    let _ = &mut at;
    0
}

/// How many times `Write` was entered.
static WRITE_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times a backend accepted bytes and then reported a size that does not follow from them.
static WRITE_DISAGREEMENTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times `Write` was entered.
#[must_use]
pub fn write_call_count() -> usize {
    WRITE_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many times a write was accepted and then contradicted by the store.
#[must_use]
pub fn write_disagreement_count() -> usize {
    WRITE_DISAGREEMENTS.load(std::sync::atomic::Ordering::Relaxed)
}
/// The `Close` body **without** an adapter: frees the `FileContext` and nothing else.
///
/// Kept for the paths that have no mount to ask (and for the direct tests that exercise the allocation contract alone); production
/// goes through [`close_shell`], which also releases the handle and performs the removal.
///
/// # Safety
///
/// `context` must have come from `open_like` and must not be used afterwards.
pub unsafe fn close(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: the caller guarantees this pointer came from `Box::into_raw` in `open`, and that `Close` happens once per open.
    drop(unsafe { Box::from_raw(context.cast::<EntryContext>()) });
    CONTEXTS_FREED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `STATUS_OBJECT_NAME_NOT_FOUND`.
pub const OURS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
/// `STATUS_OBJECT_NAME_COLLISION` — "that name is already taken" (`FILE_CREATE` on an existing entry).
pub const OURS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035u32 as i32;
/// `STATUS_BUFFER_TOO_SMALL`.
pub const OURS_BUFFER_TOO_SMALL: i32 = 0xC000_0023u32 as i32;
/// `STATUS_INVALID_PARAMETER`.
pub const OURS_INVALID_PARAMETER: i32 = 0xC000_000Du32 as i32;

/// How many times `GetVolumeInfo` was called — the cheapest "did the OS talk to us at all" probe.
static VOLUME_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times the volume question has been answered.
#[must_use]
pub fn volume_call_count() -> usize {
    VOLUME_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many times `GetSecurityByName` was called — the counter that makes "we were never asked" visible.
static SECURITY_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times the security query has been answered.
#[must_use]
pub fn security_call_count() -> usize {
    SECURITY_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// The `GetSecurityByName` **shell**: the name is resolved by **this mount's backend**, like `open` (367's route).
///
/// Windows asks this **before** opening anything, so it is the first place a mount dies — and the first place a leftover
/// process-wide resolver would show up as `ERROR_INVALID_FUNCTION` for every operation that follows.
///
/// # Safety
///
/// Same contract as [`get_security_by_name`].
pub unsafe fn get_security_by_name_shell(
    adapter: Option<&crate::DynAdapter>,
    file_name: *const u16,
    attributes_out: *mut u32,
    descriptor_out: *mut c_void,
    descriptor_size_out: *mut usize,
) -> i32 {
    SECURITY_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if file_name.is_null() {
        return OURS_INVALID_PARAMETER;
    }
    // SAFETY: `file_name` is NUL-terminated per WinFsp's contract.
    let units = unsafe { crate::boundary::utf16_len(file_name) };
    // SAFETY: same pointer, bounded by the terminator.
    let slice = unsafe { std::slice::from_raw_parts(file_name, units) };
    let path = String::from_utf16_lossy(slice);
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    // `Open` is the read-only disposition that **does not create** — the right question for "does this name exist".
    let Ok(outcome) = adapter.backend().create_or_open(
        path.as_bytes(),
        fs_semantics::disposition::Disposition::Open,
    ) else {
        return OURS_OBJECT_NAME_NOT_FOUND;
    };
    let Ok(node) = adapter.backend().get_info(outcome.file_id) else {
        return OURS_OBJECT_NAME_NOT_FOUND;
    };
    // SAFETY: the caller's contract covers all three out-pointers.
    unsafe { write_security_answer(attributes_out, descriptor_out, descriptor_size_out, &node) }
}

/// The answer both `GetSecurityByName` paths share: attributes out, a **zero-length** descriptor, nothing written into a buffer.
///
/// # Safety
///
/// `attributes_out` and `descriptor_size_out` may be null; `descriptor_out` is only read, never written.
unsafe fn write_security_answer(
    attributes_out: *mut u32,
    descriptor_out: *mut c_void,
    descriptor_size_out: *mut usize,
    node: &NodeInfo,
) -> i32 {
    if !attributes_out.is_null() {
        // SAFETY: the caller passes either a valid pointer or null (fsop.c:797 passes null when it only wants the size).
        unsafe { std::ptr::write(attributes_out, node.attributes.0) };
    }
    if !descriptor_size_out.is_null() {
        // **Two-phase contract**: on a size query the caller wants only the required size and must not get a byte written into
        // its buffer. `0` means "no explicit security information", which is the truth for a workspace-backed volume and the
        // one answer that needs no buffer at all.
        // SAFETY: writable out-parameter, checked non-null above.
        unsafe { std::ptr::write(descriptor_size_out, 0) };
    }
    let _ = descriptor_out;
    0
}
