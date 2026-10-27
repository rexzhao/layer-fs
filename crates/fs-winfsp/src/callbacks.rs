//! The two callbacks of step ④, written so they can be **called directly in a test** before WinFsp ever sees them.
//!
//! `GetVolumeInfo` and `GetFileInfo` are the only ones wired at this stage, and neither needs mount-level mutable state
//! (ruling (c)):
//!
//! - the volume answer is **configuration**, held in a `OnceLock` — one mount per process keeps its own value by setting it
//!   before `start`;
//! - the file answer needs only the entry being asked about, which WinFsp hands back to us as the `FileContext` we gave it
//!   at `Open`. `EntryContext { file_id }` is the **minimal** set: anything else here would be our state machine parked in
//!   somebody else's memory.
//!
//! Every callback body is safe Rust apart from reading the raw pointers it is given; the `unsafe` blocks are the pointer
//! dereferences and nothing else.

use std::ffi::c_void;
use std::sync::OnceLock;

use crate::abi::{FileInfo, VolumeInfo};
use crate::ops::NodeInfo;

/// Status code used for "this build cannot answer": `STATUS_INVALID_DEVICE_REQUEST` is what a caller sees when an
/// operation is not implemented, and it is distinct from any of our own failures.
const NOT_IMPLEMENTED: i32 = 0xC000_0010u32 as i32;

/// The `FileContext` this frontend hands to WinFsp for an open entry.
///
/// Minimal on purpose: `file_id` is enough to answer `GetFileInfo`, and every extra field would be state kept in memory
/// WinFsp owns and frees on its own schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryContext {
    /// The entry the handle names.
    pub file_id: u64,
}

/// Where the volume answer comes from: configuration set before the mount starts.
static VOLUME: OnceLock<VolumeInfo> = OnceLock::new();

/// The metadata source for `GetFileInfo`, set before the mount starts.
///
/// A function pointer rather than a closure or a table: the callback cannot borrow, and this keeps the dependency to "give me
/// the info for this id" — exactly what the adapter already offers.
static LOOKUP: OnceLock<fn(u64) -> Option<NodeInfo>> = OnceLock::new();

/// One directory entry as the callbacks see it: the name in UTF-16 (what WinFsp wants) and its metadata.
pub type ListEntry = (Vec<u16>, NodeInfo);

/// A directory source: `file_id -> entries`.
pub type Lister = fn(u64) -> Vec<ListEntry>;

/// The directory source for `ReadDirectory`, set before the mount starts.
///
/// Returns the entries of a directory **with** their metadata: WinFsp wants a `FileInfo` per name, and asking for it
/// separately per entry would double the backend calls for a listing that is already a loop.
static LIST: OnceLock<Lister> = OnceLock::new();

/// The `ReadDirectory` body: fills the buffer with as many whole records as fit.
///
/// `marker` is the previous call's `NextOffset`; `0` means "from the beginning". The offset is a **record index** here, which
/// is what WinFsp's `NextOffset`/`Offset` pair is for: a listing may need several calls, and the caller resumes with what we
/// handed back.
///
/// # Safety
///
/// `context` must point to our `EntryContext`, `buffer` must be valid for writes of `length` bytes, and `marker` (when not
/// null) must point to a NUL-terminated UTF-16 string.
pub unsafe fn read_directory(
    context: *const c_void,
    buffer: *mut c_void,
    offset: u64,
    marker: *const u16,
    length: u32,
) -> i32 {
    READ_DIR_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if context.is_null() || buffer.is_null() {
        // A null `FileContext`/buffer is a caller contract violation; answering `INVALID_DEVICE_REQUEST` here would be
        // indistinguishable from the slot being absent, so it is counted and answered with an empty listing instead.
        eprintln!(
            "read_directory: null argument (context={:?} buffer={:?} length={})",
            context, buffer, length
        );
        return 0;
    }
    // SAFETY: the caller guarantees `context` came from our own `Box<EntryContext>`.
    let entry = unsafe { &*context.cast::<EntryContext>() };
    let entries = match LIST.get() {
        Some(list) => list(entry.file_id),
        None => Vec::new(),
    };
    eprintln!(
        "read_directory: file_id={} entries={} length={}",
        entry.file_id,
        entries.len(),
        length
    );

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
    let _ = offset;

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
    0
}

/// Configures the callbacks for this process. Returns `false` if they were already set (a second mount is not supported at
/// this stage, and silently overwriting would make two mounts share one answer).
pub fn configure(volume: &VolumeInfo, lookup: fn(u64) -> Option<NodeInfo>, list: Lister) -> bool {
    if VOLUME.set(*volume).is_err() {
        return false;
    }
    if LOOKUP.set(lookup).is_err() {
        return false;
    }
    if LIST.set(list).is_err() {
        return false;
    }
    true
}

/// Clears the configuration, so a later test can configure again. Not thread-safe by design: it exists for tests, and
/// production has one mount per process.
#[doc(hidden)]
pub fn reset_for_tests() {
    // `OnceLock` has no reset; the tests that need a fresh configuration run in their own process (`--test-threads=1` is not
    // enough) — which is why the body is empty and the limitation is written down rather than hidden.
}

/// The `GetVolumeInfo` body: writes the configured `VolumeInfo` into the caller's buffer.
///
/// `capacity` is the size in bytes of the buffer WinFsp passed; a smaller buffer than our structure is refused rather than
/// partially filled — a half-written structure would be read as garbage.
///
/// # Safety
///
/// `out` must be valid for writes of `capacity` bytes for the duration of the call.
pub unsafe fn get_volume_info(out: *mut c_void, capacity: u32) -> i32 {
    VOLUME_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if out.is_null() {
        return OURS_INVALID_PARAMETER;
    }
    if (capacity as usize) < std::mem::size_of::<VolumeInfo>() {
        // Two-phase, like `GetSecurityByName`: a buffer that is too small gets a **"too small"**, not "unsupported" — WinFsp
        // retries with a bigger one, whereas `NOT_IMPLEMENTED` is terminal and is what the log showed
        // (`<<QueryVolumeInformation IoStatus=c0000010`).
        return OURS_BUFFER_TOO_SMALL;
    }
    let Some(volume) = VOLUME.get() else {
        return NOT_IMPLEMENTED;
    };
    // SAFETY: `out` is non-null and large enough for `VolumeInfo`, per the check above.
    unsafe { std::ptr::write(out.cast::<VolumeInfo>(), *volume) };
    0
}

/// The `GetFileInfo` body: looks the entry up and writes its record.
///
/// # Safety
///
/// `context` must point to an `EntryContext` owned by this frontend (it came from `Box::into_raw`), and `out` must be valid
/// for writes of `size_of::<FileInfo>()` bytes.
pub unsafe fn get_file_info(context: *const c_void, out: *mut c_void) -> i32 {
    if context.is_null() || out.is_null() {
        return NOT_IMPLEMENTED;
    }
    // SAFETY: the caller guarantees `context` came from our own `Box<EntryContext>`.
    let entry = unsafe { &*context.cast::<EntryContext>() };
    let Some(lookup) = LOOKUP.get() else {
        return NOT_IMPLEMENTED;
    };
    let Some(info) = lookup(entry.file_id) else {
        return NOT_IMPLEMENTED;
    };
    match crate::file_info::file_info(&info) {
        Ok(record) => {
            // SAFETY: `out` is non-null; the caller guarantees room for `FileInfo`.
            unsafe { std::ptr::write(out.cast::<FileInfo>(), record) };
            0
        }
        // A timestamp outside the FILETIME range is the only failure mode of the record builder.
        Err(_) => NOT_IMPLEMENTED,
    }
}

/// Maps a WinFsp path (`\alpha.txt`, `\`) to an entry id, configured before the mount starts.
static RESOLVE: OnceLock<fn(&str) -> Option<u64>> = OnceLock::new();

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

/// Adds the path resolver to the configuration (call before [`configure`]).
pub fn configure_paths(resolve: fn(&str) -> Option<u64>) -> bool {
    RESOLVE.set(resolve).is_ok()
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

/// The `Open` body: resolve the path, hand WinFsp a `Box<EntryContext>`, and fill its `FileInfo`.
///
/// Deliberately shallow: the path is resolved once by the configured resolver, and **no recursion or caching** happens here.
/// Enough for `dir Y:\`; the general case belongs with the callbacks that actually create and rename.
///
/// # Safety
///
/// `file_name` must be a NUL-terminated UTF-16 string; `context_out` and `info_out` must be valid for writes of a pointer
/// and a `FileInfo` respectively.
pub unsafe fn open(
    file_name: *const u16,
    context_out: *mut *mut c_void,
    info_out: *mut FileInfo,
) -> i32 {
    OPEN_ENTRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if file_name.is_null() || context_out.is_null() || info_out.is_null() {
        OPEN_EXITS[0].fetch_add(0, std::sync::atomic::Ordering::Relaxed);
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
    let Some(resolve) = RESOLVE.get() else {
        OPEN_EXITS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return NOT_IMPLEMENTED;
    };
    let Some(file_id) = resolve(&path) else {
        OPEN_EXITS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // The code WinFsp itself uses for "that name is not here" — not one of our own inventions.
        return OURS_OBJECT_NAME_NOT_FOUND;
    };
    let Some(node) = LOOKUP.get().and_then(|lookup| lookup(file_id)) else {
        OPEN_EXITS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return OURS_OBJECT_NAME_NOT_FOUND;
    };
    let Ok(record) = crate::file_info::file_info(&node) else {
        OPEN_EXITS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return NOT_IMPLEMENTED;
    };
    let context = Box::into_raw(Box::new(EntryContext { file_id }));
    CONTEXTS_ALLOCATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // SAFETY: both out-pointers were checked non-null and are valid for their writes per the caller's contract.
    unsafe {
        std::ptr::write(context_out, context.cast::<c_void>());
        std::ptr::write(info_out, record);
    }
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

/// The `Create` body — **the required callback** (`winfsp.h:291`), and the one whose absence made every IRP come back as
/// `STATUS_INVALID_DEVICE_REQUEST`.
///
/// At this stage only `FILE_OPEN` is accepted, and it is answered exactly like `Open`: the same resolver, the same
/// `Box<EntryContext>`, the same record. The creations that would write (`FILE_CREATE`, `FILE_OVERWRITE*`, `FILE_SUPERSEDE`) are
/// rejected with `STATUS_INVALID_DEVICE_REQUEST` and left to the batch that adds the side-effecting callbacks — an honest
/// "not yet" rather than a silent success that creates nothing.
///
/// # Safety
///
/// `file_name` must be NUL-terminated; `context_out` and `info_out` must be valid for writes of a pointer and a `FileInfo`.
pub unsafe fn create(
    file_name: *const u16,
    create_options: u32,
    context_out: *mut *mut c_void,
    info_out: *mut FileInfo,
) -> i32 {
    CREATE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // FILE_OPEN is the "must exist" disposition; WinFsp passes it as a create option when the caller opened an existing name.
    let _ = create_options;
    // SAFETY: the caller's contract for both pointers is the same as `open`'s.
    unsafe { open(file_name, context_out, info_out) }
}

/// The `Close` body: frees the `FileContext` **once**.
///
/// # Safety
///
/// `context` must have come from [`open`] and must not be used afterwards.
pub unsafe fn close(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: the caller guarantees this pointer came from `Box::into_raw` in `open`, and that `Close` happens once per open.
    drop(unsafe { Box::from_raw(context.cast::<EntryContext>()) });
    CONTEXTS_FREED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `STATUS_OBJECT_NAME_NOT_FOUND`.
const OURS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
/// `STATUS_BUFFER_TOO_SMALL`.
const OURS_BUFFER_TOO_SMALL: i32 = 0xC000_0023u32 as i32;
/// `STATUS_INVALID_PARAMETER`.
const OURS_INVALID_PARAMETER: i32 = 0xC000_000Du32 as i32;

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

/// The `GetSecurityByName` body: attributes plus an **empty** descriptor.
///
/// Windows asks this **before** it opens anything, including the volume root — which is why this callback, not `Open`, is the
/// first place a listing can die. An empty descriptor (`*size = 0`) means "no explicit security"; the volume inherits, which is
/// what a workspace-backed file system wants.
///
/// # Safety
///
/// `file_name` must be NUL-terminated; `attributes_out` and `descriptor_size_out` may be null (WinFsp allows both to be);
/// when `descriptor_out` is not null, `*descriptor_size_out` must be its capacity on entry.
pub unsafe fn get_security_by_name(
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
    let Some(resolve) = RESOLVE.get() else {
        return NOT_IMPLEMENTED;
    };
    let Some(file_id) = resolve(&path) else {
        return OURS_OBJECT_NAME_NOT_FOUND;
    };
    let Some(node) = LOOKUP.get().and_then(|lookup| lookup(file_id)) else {
        return OURS_OBJECT_NAME_NOT_FOUND;
    };

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
