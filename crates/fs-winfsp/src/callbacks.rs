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

/// Configures the callbacks for this process. Returns `false` if they were already set (a second mount is not supported at
/// this stage, and silently overwriting would make two mounts share one answer).
pub fn configure(volume: &VolumeInfo, lookup: fn(u64) -> Option<NodeInfo>) -> bool {
    if VOLUME.set(*volume).is_err() {
        return false;
    }
    if LOOKUP.set(lookup).is_err() {
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
    if out.is_null() || (capacity as usize) < std::mem::size_of::<VolumeInfo>() {
        return NOT_IMPLEMENTED;
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
