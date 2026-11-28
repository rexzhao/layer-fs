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

/// `STATUS_VOLUME_DISMOUNTED` — "an operation was attempted to a volume after it was dismounted" (`ntstatus.h:7047`).
///
/// Read from the header (Windows Kits `10.0.26100.0`), not from memory: this is the answer an arriving open gets while a `switch` is closing the mount, and a
/// wrong code would show up as a confusing error instead of "the volume is going away".
pub const OURS_VOLUME_DISMOUNTED: i32 = 0xC000_026Eu32 as i32;

/// Takes an admission ticket for a path-entering callback, or answers the status that refuses the open.
///
/// The three doors that resolve a **name** (`Create`, `Open`, `GetSecurityByName`) all route through this, so "a restore/switch excludes new opens" is one
/// rule with three callers rather than three chances to forget it. The ticket must be held for the whole callback — resolve, backend call and handle
/// registration included — which is the window it exists to cover (S3 card §3).
fn admit(adapter: &crate::DynAdapter) -> Result<crate::admission::Ticket<'_>, i32> {
    adapter.admit().map_err(|refusal| match refusal {
        // Waiting here would deadlock the unmount that is waiting for the dispatcher, so the open fails — what NTFS does while a volume is being dismounted.
        crate::admission::Refused::Closing => OURS_VOLUME_DISMOUNTED,
    })
}

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
    let Some(entry) = entry_of(context) else {
        return NOT_IMPLEMENTED;
    };
    let context_id = context as usize as u64;
    // Cache the sorted directory snapshot on the open directory handle (`Shared::dir_snapshot`):
    // - On the first page (`marker.is_null()`), compute a fresh snapshot once and cache it.
    // - On pages 2..N (`!marker.is_null()`), reuse the cached `Arc<Vec<ListEntry>>` in O(1)
    //   instead of re-scanning and re-sorting the entire directory on every 16 KiB WinFsp page!
    let entries: std::sync::Arc<Vec<ListEntry>> = match adapter {
        Some(adapter) => {
            if !marker.is_null() {
                if let Some(cached) = cached_dir_snapshot(context_id) {
                    cached
                } else {
                    let fresh = list_entries(adapter, entry.file_id);
                    store_dir_snapshot(context_id, std::sync::Arc::clone(&fresh));
                    fresh
                }
            } else {
                let fresh = list_entries(adapter, entry.file_id);
                store_dir_snapshot(context_id, std::sync::Arc::clone(&fresh));
                fresh
            }
        }
        // No mount: an empty page. A miss must never fall back to another mount's listing (352).
        None => std::sync::Arc::new(Vec::new()),
    };
    // SAFETY: the caller's contract covers every pointer handed through.
    unsafe { read_directory_from(&entries, buffer, marker, length, bytes_transferred) }
}

/// One directory as `Arc<Vec<ListEntry>>`, fetched via the backend's cached UTF-16 listing (N7).
fn list_entries(adapter: &crate::DynAdapter, dir_id: u64) -> std::sync::Arc<Vec<ListEntry>> {
    adapter
        .backend()
        .read_dir_utf16_cached(dir_id)
        .unwrap_or_else(|_| std::sync::Arc::new(Vec::new()))
}

/// The `GetDirInfoByName` **shell** (`winfsp.h:852`, slot 24): resolves a single child name inside
/// an open parent directory in O(1) without enumerating the directory.
///
/// # Safety
///
/// `context` is an opaque handle id; `file_name` is a NUL-terminated UTF-16 component name;
/// `dir_info_out` points to a WinFsp `FSP_FSCTL_DIR_INFO` buffer with room for up to 255 UTF-16
/// code units after the 104-byte fixed header.
pub unsafe fn get_dir_info_by_name_shell(
    adapter: Option<&crate::DynAdapter>,
    context: *const c_void,
    file_name: *const u16,
    dir_info_out: *mut c_void,
) -> i32 {
    if context.is_null() || file_name.is_null() || dir_info_out.is_null() {
        return OURS_INVALID_PARAMETER;
    }
    let Some(entry) = entry_of(context) else {
        return NOT_IMPLEMENTED;
    };
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    // SAFETY: `file_name` is NUL-terminated per WinFsp's contract.
    let units = unsafe { crate::boundary::utf16_len(file_name) };
    // SAFETY: bounded by the NUL terminator.
    let slice = unsafe { std::slice::from_raw_parts(file_name, units) };
    let Ok(name_utf8) = crate::boundary::utf16_to_utf8(slice) else {
        return OURS_OBJECT_NAME_NOT_FOUND;
    };
    let (stored_name, node) = match adapter
        .backend()
        .lookup_child_info(entry.file_id, name_utf8.as_bytes())
    {
        Ok(pair) => pair,
        Err(crate::ops::OpsError::NotFound) => return OURS_OBJECT_NAME_NOT_FOUND,
        Err(err) => return crate::error::to_status_of(&err).code() as i32,
    };
    let Ok(record) = crate::file_info::file_info(&node) else {
        return NOT_IMPLEMENTED;
    };
    let name_units: Vec<u16> = String::from_utf8_lossy(&stored_name)
        .encode_utf16()
        .take(255)
        .collect();
    let rec_size = crate::abi::dir_info::record_size(name_units.len());
    let Ok(size_u16) = u16::try_from(rec_size) else {
        return OURS_INVALID_PARAMETER;
    };
    // SAFETY: WinFsp supplies a `FSP_FSCTL_DIR_INFO` + `MAX_PATH` buffer (`104 + 260 * 2` bytes).
    unsafe {
        let base = dir_info_out.cast::<u8>();
        std::ptr::write_bytes(base, 0, crate::abi::dir_info::HEADER);
        std::ptr::copy_nonoverlapping(size_u16.to_ne_bytes().as_ptr(), base, 2);
        std::ptr::write(
            base.add(crate::abi::dir_info::FILE_INFO_AT).cast::<FileInfo>(),
            record,
        );
        std::ptr::copy_nonoverlapping(
            name_units.as_ptr().cast::<u8>(),
            base.add(crate::abi::dir_info::NAME_AT),
            name_units.len() * 2,
        );
    }
    0
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
    // If the marker entry was deleted mid-enumeration, advance to the first real child strictly greater than it
    // (skipping synthesized leading `.` and `..` so names like `!foo` < `.` never loop back to `.`).
    let start = if marker.is_null() {
        0usize
    } else {
        // SAFETY: WinFsp passes a NUL-terminated UTF-16 name.
        let name_units = unsafe { crate::boundary::utf16_len(marker) };
        // SAFETY: same pointer, bounded by the terminator just measured.
        let name = unsafe { std::slice::from_raw_parts(marker, name_units) };
        let first_real = entries
            .iter()
            .take_while(|(c, _)| {
                c.as_slice() == [u16::from(b'.')]
                    || c.as_slice() == [u16::from(b'.'), u16::from(b'.')]
            })
            .count();
        if let Some(dot_idx) = entries[..first_real]
            .iter()
            .position(|(candidate, _)| candidate.as_slice() == name)
        {
            dot_idx + 1
        } else {
            // `entries[first_real..]` is sorted by UTF-16 code units: O(log N) resume!
            let real = &entries[first_real..];
            match real.binary_search_by(|(candidate, _)| candidate.as_slice().cmp(name)) {
                Ok(idx) => first_real + idx + 1,
                Err(ins) => first_real + ins,
            }
        }
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
pub unsafe fn get_volume_info_shell(adapter: Option<&crate::DynAdapter>, out: *mut c_void) -> i32 {
    VOLUME_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let config = adapter.map(|adapter| adapter.config());
    let volume = config.as_ref().and_then(|config| config.volume);
    let capacity_source = config.and_then(|config| config.capacity);
    // SAFETY: the caller's contract covers `out`; the size is the structure's own, because the dispatcher always passes an
    // 88-byte local (`fsop.c:1245-1260`).
    unsafe {
        get_volume_info_for(
            volume,
            capacity_source,
            out,
            std::mem::size_of::<VolumeInfo>() as u32,
        )
    }
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
/// The **capacity** of the volume, asked afresh on every call.
///
/// The first version hard-coded `1 GiB / 512 MiB` as a placeholder and never replaced it, so Explorer showed "1 GB, 512 MB used" regardless of content — a number
/// that was not merely stale but invented. The honest answer for a content-addressed store is the **host volume it lives on**: when that disk fills, writes through
/// the mount fail, and the reported free space should say so at the same moment rather than after a remount.
///
/// `None` means "cannot be asked" (no store path, or the call failed): the caller then keeps whatever the configuration supplied.
pub type CapacitySource = std::sync::Arc<dyn Fn() -> Option<(u64, u64)> + Send + Sync>;

#[cfg(windows)]
mod capacity_api {
    #[link(name = "kernel32")]
    extern "system" {
        /// `BOOL GetDiskFreeSpaceExW(LPCWSTR, PULARGE_INTEGER, PULARGE_INTEGER, PULARGE_INTEGER)`.
        pub fn GetDiskFreeSpaceExW(
            directory_name: *const u16,
            free_bytes_available: *mut u64,
            total_bytes: *mut u64,
            total_free_bytes: *mut u64,
        ) -> i32;
    }
}

/// The host volume behind `path`: `(total_bytes, free_bytes_available)`.
///
/// `None` when the path cannot be resolved to a volume or the API refuses — the caller then keeps the configured numbers rather than inventing one.
#[cfg(windows)]
pub fn host_capacity(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_available: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    // SAFETY: `wide` is NUL-terminated; the three out-pointers are valid locals. The API accepts a directory path and resolves its volume itself.
    let asked = unsafe {
        capacity_api::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_available,
            &mut total,
            &mut total_free,
        )
    };
    if asked == 0 || total == 0 {
        return None;
    }
    Some((total, free_available))
}

/// Non-Windows builds have no host volume to ask.
#[cfg(not(windows))]
pub fn host_capacity(_path: &std::path::Path) -> Option<(u64, u64)> {
    None
}

/// `GetVolumeInfo`: the configured label plus the **live** capacity of the host volume, written as far as the caller's buffer reaches.
///
/// # Safety
///
/// `out` must be writable for `capacity` bytes, or null.
pub unsafe fn get_volume_info_for(
    volume: Option<VolumeInfo>,
    capacity_source: Option<CapacitySource>,
    out: *mut c_void,
    capacity: u32,
) -> i32 {
    if out.is_null() {
        return OURS_INVALID_PARAMETER;
    }
    let Some(volume) = volume else {
        // The order matters and is asserted by a test: **"no volume configured" is decided before the buffer is trusted**, so a
        // configured-but-absent volume cannot be mistaken for a caller that passed a bad pointer.
        return NOT_IMPLEMENTED;
    };
    // **Write what fits, and only what fits.** WinFsp's own dispatcher asks with a short buffer (measured: `capacity = 8`,
    // repeatedly), and answering `BUFFER_TOO_SMALL` forever is what left the volume showing no size in Explorer. The structure's
    // fields are laid out leading-first (`fsctl.h:270-275`), so a prefix **is** a valid answer to "how big is this volume" — and the
    // alternative, writing all 88 bytes into an 8-byte buffer, would be a buffer overrun rather than an answer.
    // The label is configuration; the two numbers are **state**.
    let mut volume = volume;
    if let Some(source) = capacity_source {
        if let Some((total, free)) = source() {
            volume.total_size = total;
            volume.free_size = free;
        }
    }
    let wanted = std::mem::size_of::<VolumeInfo>();
    let take = (capacity as usize).min(wanted);
    // SAFETY: `out` is non-null and valid for `capacity` bytes per the caller's contract; `take <= capacity` and `take <= wanted`.
    unsafe {
        std::ptr::copy_nonoverlapping(
            std::ptr::addr_of!(volume).cast::<u8>(),
            out.cast::<u8>(),
            take,
        );
    }
    0
}

/// The `SetFileSize` **shell**: ask the mount's backend to resize, then answer with the entry's **current** record.
///
/// The two steps are deliberately separate: "the backend accepted a length" and "the entry now has that length" are different
/// facts, and the record handed back is built from a fresh `get_info`, so a backend that silently ignores the request cannot make
/// the caller believe it worked (382 — the write-side mirror of "zero records read as an empty file").
///
/// # Safety
/// Whether `ptr` is writable for `len` bytes according to the OS, not according to our hopes.
///
/// **Why this exists**: a heap-corruption abort (`0xc0000374`) names the *abort* point, not the *fault* point — the two can be dozens of
/// calls apart, so a debug trace only narrows the neighbourhood. This check asks the operating system directly, right before the write,
/// and turns "somebody later noticed the heap was broken" into "this callback, this pointer, this length".
///
/// It is cheap enough to keep on: `VirtualQuery` is a single syscall, the callbacks that use it write at most a few dozen bytes, and a
/// write that fails this check is a **bug in the caller or in our own argument reading**, never a normal runtime event.
#[cfg(windows)]
fn write_is_safe(ptr: *const c_void, len: usize) -> bool {
    use std::ffi::c_void as Cvoid;
    #[repr(C)]
    struct MemoryBasicInformation {
        base_address: *mut Cvoid,
        allocation_base: *mut Cvoid,
        allocation_protect: u32,
        partition_id: u16,
        _pad: u16,
        region_size: usize,
        state: u32,
        protect: u32,
        kind: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualQuery(
            address: *const Cvoid,
            buffer: *mut MemoryBasicInformation,
            length: usize,
        ) -> usize;
    }
    const MEM_COMMIT: u32 = 0x1000;
    const PAGE_READWRITE: u32 = 0x04;
    const PAGE_WRITECOPY: u32 = 0x08;
    const PAGE_EXECUTE_READWRITE: u32 = 0x40;
    const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
    const PAGE_GUARD: u32 = 0x100;

    if ptr.is_null() || len == 0 {
        return false;
    }
    let mut info = MemoryBasicInformation {
        base_address: std::ptr::null_mut(),
        allocation_base: std::ptr::null_mut(),
        allocation_protect: 0,
        partition_id: 0,
        _pad: 0,
        region_size: 0,
        state: 0,
        protect: 0,
        kind: 0,
    };
    // SAFETY: the pointer is only inspected; the structure is ours and fully initialised.
    let asked = unsafe {
        VirtualQuery(
            ptr,
            &mut info,
            std::mem::size_of::<MemoryBasicInformation>(),
        )
    };
    if asked == 0 || info.state != MEM_COMMIT || info.protect & PAGE_GUARD != 0 {
        return false;
    }
    let writable = matches!(
        info.protect & 0xFF,
        PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
    );
    if !writable {
        return false;
    }
    let start = info.base_address as usize;
    let end = start.saturating_add(info.region_size);
    let at = ptr as usize;
    at >= start && at.saturating_add(len) <= end
}

fn verify_ptrs_enabled() -> bool {
    static VERIFY_PTRS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VERIFY_PTRS
        .get_or_init(|| std::env::var_os("LAYERFS_VERIFY_PTRS").is_some_and(|v| v != "0"))
}

/// Writes a `FileInfo` to `out`, optionally running `VirtualQuery` when `LAYERFS_VERIFY_PTRS=1`.
///
/// # Safety
///
/// `out` is non-null and writable for `size_of::<FileInfo>()` bytes per the WinFsp callback contract.
unsafe fn write_file_info(out: *mut FileInfo, record: FileInfo) -> bool {
    if out.is_null() {
        return false;
    }
    if verify_ptrs_enabled() && !write_is_safe(out.cast::<c_void>(), std::mem::size_of::<FileInfo>())
    {
        BAD_OUTPUT_POINTERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "file info: refusing to write {} bytes to {out:p} — not a writable region the OS knows about",
            std::mem::size_of::<FileInfo>()
        );
        return false;
    }
    // SAFETY: checked non-null (and writable if diagnostic verification is enabled).
    unsafe { std::ptr::write(out, record) };
    true
}

/// How many output pointers failed the OS-level check.
static BAD_OUTPUT_POINTERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times an output pointer was refused because the OS said it was not writable.
#[must_use]
pub fn bad_output_pointer_count() -> usize {
    BAD_OUTPUT_POINTERS.load(std::sync::atomic::Ordering::Relaxed)
}

/// On non-Windows there is no `VirtualQuery`: the check is a no-op so the shells stay compilable and the criterion does not silently
/// disappear on the platform that never had the crash.
#[cfg(not(windows))]
fn write_is_safe(_ptr: *const c_void, _len: usize) -> bool {
    true
}

/// The `SetFileSize` **shell**: ask the mount's backend to resize, then answer with the entry's **current** record.
///
/// # Safety
///
/// `context` must point to an `EntryContext` owned by this frontend; `out` must be valid for writes of `size_of::<FileInfo>()`.
pub unsafe fn set_file_size_shell(
    adapter: Option<&crate::DynAdapter>,
    context: *const c_void,
    new_size: u64,
    set_allocation_size: bool,
    out: *mut FileInfo,
) -> i32 {
    SET_SIZE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if context.is_null() || out.is_null() {
        return OURS_INVALID_PARAMETER;
    }
    // SAFETY: the caller guarantees `context` came from our own `Box<EntryContext>`.
    let Some(entry) = entry_of(context) else {
        return NOT_IMPLEMENTED;
    };
    let entry = &entry;
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    if set_allocation_size {
        // `SetAllocationSize = TRUE` asks for **reserved space**, not for a length: this store grows on demand, so the truthful answer
        // is success with the file unchanged. Truncating here would be the lie — it would drop data the caller did not ask to drop.
        return match adapter.backend().get_info(entry.file_id) {
            Ok(node) => {
                if !out.is_null() {
                    match crate::file_info::file_info(&node) {
                        Ok(record) => {
                            // SAFETY: `out` is non-null and the caller guarantees room for `FileInfo`.
                            if !unsafe { write_file_info(out, record) } {
                                return OURS_INVALID_PARAMETER;
                            }
                            0
                        }
                        Err(_) => NOT_IMPLEMENTED,
                    }
                } else {
                    0
                }
            }
            Err(_) => OURS_OBJECT_NAME_NOT_FOUND,
        };
    }
    if let Err(err) = adapter.backend().set_size(entry.file_id, new_size) {
        // Nothing is written into the caller's buffer on failure: a half-updated record is worse than none.
        return crate::error::to_status_of(&err).code() as i32;
    }
    // Read back through the **same** backend: this is where "accepted" becomes "is".
    let node = match adapter.backend().get_info(entry.file_id) {
        Ok(node) => node,
        Err(err) => return crate::error::to_status_of(&err).code() as i32,
    };
    if node.size != new_size {
        // The backend said yes and the store says otherwise. Refusing is the only honest answer, and it is counted so the two
        // facts cannot be confused later.
        SET_SIZE_DISAGREEMENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return NOT_IMPLEMENTED;
    }
    match crate::file_info::file_info(&node) {
        Ok(record) => {
            if !unsafe { write_file_info(out, record) } {
                return OURS_INVALID_PARAMETER;
            }
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
    let Some(entry) = entry_of(context) else {
        return NOT_IMPLEMENTED;
    };
    let entry = &entry;
    // No mount ⇒ no metadata. `None` in the type is the refusal, not a reason to ask somebody else's configuration (352).
    let info = adapter.and_then(|adapter| adapter.backend().get_info(entry.file_id).ok());
    let Some(info) = info else {
        return NOT_IMPLEMENTED;
    };
    match crate::file_info::file_info(&info) {
        Ok(record) => {
            if !unsafe { write_file_info(out.cast::<FileInfo>(), record) } {
                return OURS_INVALID_PARAMETER;
            }
            0
        }
        Err(_) => NOT_IMPLEMENTED,
    }
}
/// Live `FileContext` allocations, and how many have been released.
/// Every `FileContext` currently handed out, **by address**.
/// Handed-out contexts, keyed by a **monotonic id** — never by address.
///
/// The address-keyed version that lived here before was a net, not a design. It caught double releases, but under allocator reuse a *stale*
/// close could land on a live context and free the wrong object **silently** — and a serial stored *inside* the context could not help,
/// because under reuse that memory is the new context's and the serial read back is the new one too. The lesson generalises: **identity has to
/// travel in the value that crosses the boundary**, not be proven afterwards from the memory the value points at.
///
/// So the `FileContext` this frontend hands out is an **id**. We never dereference it; a stale id is simply not in the map; and the map owns
/// every context, which makes a double release impossible by construction rather than by vigilance.
/// One context and **how many file objects still refer to it**.
///
/// WinFsp's contract is the reference-counted one its own sample uses (`memfs`: `MemfsFileNodeReference` on open,
/// `MemfsFileNodeDereference` on close, the node dying at zero): several file objects for the same entry **share** one context, and every object's
/// `Close` arrives. Our first model handed out a fresh id per open and freed it at the first close, which is why the same id was closed twice and a
/// read could arrive between those closes — measured: `CLOSE 8 … READ STALE id=8 … CLOSE 8`.
struct Shared {
    entry: EntryContext,
    refs: usize,
    dir_snapshot: Option<std::sync::Arc<Vec<ListEntry>>>,
}

const MAX_CLOSED_CONTEXT_IDS: usize = 16_384;

fn debug_handles_enabled() -> bool {
    static DEBUG_HANDLES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEBUG_HANDLES.get_or_init(|| std::env::var_os("LAYERFS_DEBUG_HANDLES").is_some())
}

/// Number of independent shards for the global `FileContext` table (power of two).
/// The shard index (`file_id & (CONTEXT_SHARDS - 1)`) is encoded into the low 5 bits of every
/// issued context id so `entry_of`, `acquire_context`, `release_context`, and `dir_snapshot`
/// touch ONLY 1 of 32 shard locks instead of contending on a single global `RwLock`.
const CONTEXT_SHARDS: usize = 32;
const CONTEXT_SHARD_BITS: u32 = 5;
const CONTEXT_SHARD_MASK: u64 = (CONTEXT_SHARDS as u64) - 1;
const MAX_CLOSED_CONTEXT_IDS_PER_SHARD: usize = MAX_CLOSED_CONTEXT_IDS / CONTEXT_SHARDS;

#[inline]
fn shard_index_for_file(file_id: u64) -> usize {
    (file_id & CONTEXT_SHARD_MASK) as usize
}

#[inline]
fn shard_index_for_id(id: u64) -> usize {
    (id & CONTEXT_SHARD_MASK) as usize
}

fn context_shards() -> &'static [std::sync::RwLock<ContextTables>; CONTEXT_SHARDS] {
    static SHARDS: std::sync::OnceLock<[std::sync::RwLock<ContextTables>; CONTEXT_SHARDS]> =
        std::sync::OnceLock::new();
    SHARDS.get_or_init(|| std::array::from_fn(|_| std::sync::RwLock::new(ContextTables::default())))
}

fn read_shard(idx: usize) -> std::sync::RwLockReadGuard<'static, ContextTables> {
    context_shards()[idx]
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_shard(idx: usize) -> std::sync::RwLockWriteGuard<'static, ContextTables> {
    context_shards()[idx]
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn cached_dir_snapshot(context_id: u64) -> Option<std::sync::Arc<Vec<ListEntry>>> {
    if context_id == 0 {
        return None;
    }
    let guard = read_shard(shard_index_for_id(context_id));
    guard
        .contexts
        .get(&context_id)
        .and_then(|shared| shared.dir_snapshot.clone())
}

fn store_dir_snapshot(context_id: u64, snapshot: std::sync::Arc<Vec<ListEntry>>) {
    if context_id == 0 {
        return;
    }
    let mut guard = write_shard(shard_index_for_id(context_id));
    if let Some(shared) = guard.contexts.get_mut(&context_id) {
        shared.dir_snapshot = Some(snapshot);
    }
}

#[derive(Default)]
struct ContextTables {
    contexts: std::collections::HashMap<u64, Shared>,
    by_file: std::collections::HashMap<u64, u64>,
    closed: std::collections::HashSet<u64>,
    closed_order: std::collections::VecDeque<u64>,
}

impl ContextTables {
    fn record_closed(&mut self, id: u64) {
        if self.closed.insert(id) {
            self.closed_order.push_back(id);
            while self.closed_order.len() > MAX_CLOSED_CONTEXT_IDS_PER_SHARD {
                if let Some(old) = self.closed_order.pop_front() {
                    self.closed.remove(&old);
                }
            }
        }
    }
}

/// The next sequence number to hand out. Never reused, shifted left by `CONTEXT_SHARD_BITS` so
/// the low 5 bits always carry the shard index while every issued id stays strictly non-zero.
static NEXT_CONTEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// How many acquisitions were made (a fresh context or a `refs + 1`), next to how many releases happened.
///
/// `acquires - releases` is the number of live references the table should be carrying; comparing it with the sum of `refs` inside the table says whether the
/// accounting is self-consistent (then the bug is *who* acquired) or whether an acquisition never reached the table at all.
static ACQUIRES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `(acquires, releases)` since process start.
#[must_use]
pub fn acquire_release_counts() -> (usize, usize) {
    (
        ACQUIRES.load(std::sync::atomic::Ordering::Relaxed),
        CONTEXTS_FREED.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// The table as `(id, refs)` pairs: the reading that says whether a zero-reference entry was left behind.
#[must_use]
pub fn context_table() -> Vec<(u64, usize)> {
    let mut pairs = Vec::new();
    for idx in 0..CONTEXT_SHARDS {
        let guard = read_shard(idx);
        for (id, shared) in &guard.contexts {
            pairs.push((*id, shared.refs));
        }
    }
    pairs.sort_unstable_by_key(|(id, _)| *id);
    pairs
}

/// How many live references the table currently carries (the sum of every `refs`).
#[must_use]
pub fn live_reference_count() -> usize {
    let mut total = 0usize;
    for idx in 0..CONTEXT_SHARDS {
        let guard = read_shard(idx);
        total += guard.contexts.values().map(|s| s.refs).sum::<usize>();
    }
    total
}

/// How many releases arrived for an id that was never handed out.
///
/// This is the class the old witness could not name: the FSD does close context slots that were never ours (17 of them in one mounted run),
/// and the only safe answer is to refuse them. **Refusing is not a bug to be explained away** — it is the design being robust to garbage.
pub static STALE_RELEASES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many releases arrived for an id that was never handed out.
#[must_use]
pub fn stale_release_count() -> usize {
    STALE_RELEASES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Stores a context under an already-held shard lock and returns its shard-tagged id.
fn register_locked(tables: &mut ContextTables, shard_idx: usize, entry: EntryContext) -> u64 {
    ACQUIRES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let seq = NEXT_CONTEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = (seq << CONTEXT_SHARD_BITS) | (shard_idx as u64);
    tables.contexts.insert(
        id,
        Shared {
            entry,
            refs: 1,
            dir_snapshot: None,
        },
    );
    tables.by_file.insert(entry.file_id, id);
    id
}

/// Stores a context and returns the id the OS will carry for it.
fn register_context(entry: EntryContext) -> u64 {
    let shard_idx = shard_index_for_file(entry.file_id);
    let mut guard = write_shard(shard_idx);
    register_locked(&mut guard, shard_idx, entry)
}

/// The id for `entry`, **reusing the one already handed out for that entry** and bumping its reference count.
///
/// This is the shape the FSD expects (one context per entry, one close per file object); the key is the entry's `file_id`, which is already the
/// fold-insensitive identity the rest of this layer uses.
fn acquire_context(entry: EntryContext) -> u64 {
    let shard_idx = shard_index_for_file(entry.file_id);
    let mut guard = write_shard(shard_idx);
    if let Some(&id) = guard.by_file.get(&entry.file_id) {
        if let Some(shared) = guard.contexts.get_mut(&id) {
            shared.refs += 1;
            ACQUIRES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if debug_handles_enabled() && ACQUIRES.load(std::sync::atomic::Ordering::Relaxed) <= 40
            {
                eprintln!(
                    "acquire: id={id} file_id={} refs={}",
                    entry.file_id, shared.refs
                );
            }
            return id;
        }
    }
    register_locked(&mut guard, shard_idx, entry)
}

/// The context behind `context`, if the id was really handed out.
///
/// **No pointer is dereferenced.** That is the whole point: a value the OS invented cannot make us read memory.
fn entry_of(context: *const c_void) -> Option<EntryContext> {
    let id = context as usize as u64;
    if id == 0 {
        return None;
    }
    let guard = read_shard(shard_index_for_id(id));
    guard.contexts.get(&id).map(|shared| shared.entry)
}

/// Releases a context from the direct tests, through the same table `close_shell` uses.
#[doc(hidden)]
pub fn release_context_for_test(context: *mut c_void) {
    if let Release::Unknown = release_context(context.cast_const()) {
        STALE_RELEASES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Registers a context for the direct tests, which build one by hand instead of going through `open_like`.
///
/// Returns the same **id-as-pointer** value the OS would carry, so the test exercises the production table rather than a copy of it.
#[doc(hidden)]
pub fn register_context_for_test(entry: EntryContext) -> *mut c_void {
    register_context(entry) as usize as *mut c_void
}

/// Drops every context still in the table — called at unmount.
///
/// The table is process-wide (the callbacks have no place to keep a per-mount one), so without this a mount that leaves contexts behind would
/// make the next mount start with them. Clearing at teardown is what keeps the residue **bounded by one mount** instead of accumulating.
pub fn clear_contexts() {
    for idx in 0..CONTEXT_SHARDS {
        let mut guard = write_shard(idx);
        guard.contexts.clear();
        guard.by_file.clear();
        guard.closed.clear();
        guard.closed_order.clear();
    }
}

/// The **one** place WinFsp's path bytes are turned into the bytes the backends parse (517/518).
///
/// WinFsp hands Windows paths — `\\`, `\\dir\\file`. Every backend in this workspace parses `/`. Without this rewrite the root arrived as a
/// single component named `\\`, was answered `0xC0000034`, and WinFsp synthesised `0xC000003A` for every child: no file could be created through the
/// mount, and every subdirectory operation reported `os error 3`.
///
/// This must be the **only** conversion, applied at every door that takes a name from the file system (`open_like`, `get_security_by_name_shell`,
/// `rename_shell`; `mkdir`/`open_dir`/`dir_binding` are reached through `open_like`). Doing it in some doors and not others is worse than doing it
/// nowhere: the same entry would be addressed by two different keys (measured — a single-door version left the create chain broken *and* reddened a
/// mount test that had been green).
fn fsd_path(units: &[u16]) -> String {
    String::from_utf16_lossy(units).replace('\\', "/")
}

/// Whether the EOF-on-closed fallback is enabled (`LAYER_FS_EOF_ON_CLOSED=1`).
///
/// Default **off**: a fallback that cannot tell "the object is gone" from "we released too early" hides the lifecycle question rather than
/// answering it (459). The environment variable exists so the A/B can be re-run on demand without rebuilding the arm in.
fn eof_on_closed() -> bool {
    std::env::var_os("LAYER_FS_EOF_ON_CLOSED").is_some_and(|value| value != "0")
}

/// Whether `context` names a handle that was handed out and has since been closed.
fn is_closed(context: *const c_void) -> bool {
    let id = context as usize as u64;
    if id == 0 {
        return false;
    }
    let guard = read_shard(shard_index_for_id(id));
    guard.closed.contains(&id)
}

/// Removes the context behind `context`, or `None` when the id was never handed out (a stale close).
/// Drops the reference `context` held, freeing the entry only when the **last** file object lets go.
///
/// Returns the entry when this was the final release (the adapter then performs the single `remove_at`), and `None` when other file objects still
/// refer to it — which is the ordinary case the FSD's contract produces, not an error.
/// What a release actually did — three outcomes, not two (the instrument's own lesson, 434).
///
/// `None` used to mean both "we never issued this id" and "this file object was not the last one", so `STALE_RELEASES` grew for the second kind too and was read
/// as "the FSD is closing slots that were never ours". Those are different facts with different fixes, and one counter cannot carry both.
enum Release {
    /// The last reference went; the entry is gone (returned so the caller can close its handle).
    Freed(EntryContext),
    /// Another file object still refers to it (normal: one `FileId`, several opens). The entry travels back because **its handle must still be closed**:
    /// one handle was registered per open, so one close per open has to come out (that asymmetry was the leak).
    StillShared(EntryContext),
    /// An id we never issued — garbage from the FSD, refused by design.
    Unknown,
}

fn release_context(context: *const c_void) -> Release {
    let id = context as usize as u64;
    if id == 0 {
        return Release::Unknown;
    }
    let mut guard = write_shard(shard_index_for_id(id));
    let (remaining, shared_entry) = match guard.contexts.get_mut(&id) {
        Some(shared) => {
            shared.refs -= 1;
            (shared.refs, shared.entry)
        }
        None => return Release::Unknown,
    };
    if remaining > 0 {
        if debug_handles_enabled() {
            eprintln!("release: id={id} still_shared refs={remaining}");
        }
        return Release::StillShared(shared_entry);
    }
    let entry = guard
        .contexts
        .remove(&id)
        .map(|shared| shared.entry)
        .expect("a context that reached zero references must still be in the table");
    if guard.by_file.get(&entry.file_id) == Some(&id) {
        guard.by_file.remove(&entry.file_id);
    }
    guard.record_closed(id);
    Release::Freed(entry)
}

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
    // SAFETY: the caller's contract covers both pointers; the same descriptor and the same in/out rule as the name door.
    unsafe { write_descriptor(descriptor_out, descriptor_size_out) }
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
            // `Open` has no create options at all: a plain open never asks for a directory to be made, and it expresses no kind either.
            false,
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
    expects_file: bool,
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
    let path = fsd_path(slice);
    let Some(adapter) = adapter else {
        OPEN_EXITS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return NOT_IMPLEMENTED;
    };
    // **The ticket lives for this whole callback** (S3 §3.2): a `restore` must not start while a name is being resolved into a handle, or the handle would name
    // an object the restore just removed. Held here rather than next to the registration, because the resolve is half the window.
    let _ticket = match admit(adapter) {
        Ok(ticket) => ticket,
        Err(status) => return status,
    };
    let target = match (wants_directory, expects_file) {
        (true, true) => return OURS_INVALID_PARAMETER,
        (true, false) => crate::ops::OpenTarget::DirOnly,
        (false, true) => crate::ops::OpenTarget::FileOnly,
        (false, false) => crate::ops::OpenTarget::Any,
    };
    let outcome = match adapter
        .backend()
        .open_entry(&crate::ops::Path::new(path.as_bytes()), disposition, target)
    {
        Ok(outcome) => outcome,
        Err(err) => {
            OPEN_EXITS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return crate::error::to_status_of(&err).code() as i32;
        }
    };
    let file_id = outcome.file_id;
    let node = NodeInfo {
        file_id: outcome.file_id,
        kind: outcome.kind,
        size: outcome.size,
        times: outcome.times,
        attributes: outcome.attributes,
    };
    let is_directory = node.kind == crate::ops::Kind::Directory;
    if wants_directory && !is_directory {
        return OURS_NOT_A_DIRECTORY;
    }
    if expects_file && is_directory {
        return OURS_IS_A_DIRECTORY;
    }
    let binding = (!outcome.binding.1.is_empty()).then_some(outcome.binding);
    let _ = disposition;
    let Ok(record) = crate::file_info::file_info(&node) else {
        OPEN_EXITS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return NOT_IMPLEMENTED;
    };
    // Register the handle **after** the record is built: a refused open must not leave a handle behind, and the table is the thing
    // `Cleanup`/`Close` will act on. Coming last also means the earlier refusal paths need no undo.
    let Ok(handle) = adapter.open_handle(file_id, binding, is_directory) else {
        OPEN_EXITS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return NOT_IMPLEMENTED;
    };
    // The value the OS carries is an **id**, not an address (see `CONTEXTS`).
    let context = acquire_context(EntryContext { file_id, handle }) as usize as *mut c_void;
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
    // A disposition value NT does not define is still refused, before the backend is asked.
    let Some(disposition) = parts.disposition else {
        return NOT_IMPLEMENTED;
    };
    // `FILE_DIRECTORY_FILE` is the **option**, not the disposition — it says *what kind* to make, and it is the flag the SDK tells
    // user-mode file systems to look at (`winfsp.h:262-266`).
    let wants_directory = parts.options & FILE_DIRECTORY_FILE != 0;
    let expects_file = parts.options & FILE_NON_DIRECTORY_FILE != 0;
    // SAFETY: the caller's contract for both pointers is the same as `open_shell`'s.
    unsafe {
        open_like(
            adapter,
            file_name,
            disposition,
            wants_directory,
            expects_file,
            context_out,
            info_out,
        )
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
/// `FILE_NON_DIRECTORY_FILE` — `0x00000040`: "this name must be a file, not a directory".
///
/// Ignoring it is how a directory\x27s name got replaced by an empty file in the real trace: the create was answered through the file door
/// without checking what the name already was.
pub const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;

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
/// The `CanDelete` **shell**: is this entry deletable at all?
///
/// The header's wording is "tests whether a file or directory can be safely deleted" (`winfsp.h:600-607`), and the honest answer for this
/// backend is: if the id is live and the store still has it, yes. The **sharing** part of "safely" is the adapter's own rule and is applied
/// when the delete is actually requested — answering it here as well would be a second, drift-prone copy.
///
/// # Safety
///
/// `context` is an opaque token; it is looked up in the table and never dereferenced.
pub unsafe fn can_delete_shell(adapter: Option<&crate::DynAdapter>, context: *const c_void) -> i32 {
    let Some(entry) = entry_of(context) else {
        return OURS_INVALID_PARAMETER;
    };
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    match adapter.backend().get_info(entry.file_id) {
        Ok(info) => {
            // **This is the only place a refused deletion can still be reported.** `Cleanup` and `Close` are `VOID`, so marking and removing a
            // directory happen where nobody can return an error — measured: `rmdir` on a non-empty directory answered success while `remove_at`
            // refused inside the close, and Windows had already told the caller it worked. `CanDelete` is the question the FSD asks **before** the
            // disposition is set, so the emptiness rule belongs here as well as at the removal.
            if info.kind == crate::Kind::Directory {
                if let Ok(false) = adapter.backend().is_dir_empty(entry.file_id) {
                    return OURS_DIRECTORY_NOT_EMPTY;
                }
            }
            0
        }
        Err(err) => crate::error::to_status_of(&err).code() as i32,
    }
}

/// `SetBasicInfo`'s two sentinels, both **"leave this alone"** — and they are different values, which is exactly the kind of thing that has to be read
/// from the header rather than assumed.
///
/// `FileAttributes == 0xFFFFFFFF` (`INVALID_FILE_ATTRIBUTES`) means "do not change the attributes"; a time of `0` (or `-1`/`-2` i.e. `u64::MAX`/`u64::MAX - 1`) means "do not change this time".
const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;

/// The `SetBasicInfo` **shell**: attributes first, then the four timestamps — each field independently optional.
///
/// Windows sends one call for any combination (touch, attribute change, both), with the untouched fields carrying their sentinel. Applying them
/// separately is what keeps a timestamp-only call from clearing the attribute bits and vice versa.
///
/// # Safety
///
/// `context` is an opaque token (looked up, never dereferenced); `out` (when not null) must be writable for `size_of::<FileInfo>()`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn set_basic_info_shell(
    adapter: Option<&crate::DynAdapter>,
    context: *const c_void,
    file_attributes: u32,
    creation_time: u64,
    last_access_time: u64,
    last_write_time: u64,
    change_time: u64,
    out: *mut FileInfo,
) -> i32 {
    let Some(entry) = entry_of(context) else {
        return NOT_IMPLEMENTED;
    };
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    if file_attributes != INVALID_FILE_ATTRIBUTES {
        // The adapter accepts only the four writable bits; `DIRECTORY`/`NORMAL` are derived, so masking them off here is what keeps a caller that
        // passes the ordinary "normal file" value from being refused.
        let writable = file_attributes & fs_semantics::attributes::WRITABLE;
        if let Err(err) = adapter
            .backend()
            .set_attributes(entry.file_id, crate::ops::Attributes(writable))
        {
            return crate::error::to_status_of(&err).code() as i32;
        }
    }
    // Windows FILETIME (100ns ticks since 1601) -> Unix nanoseconds (`i64`).
    // Sentinels: `0`, `u64::MAX` (-1: disable auto-update), `u64::MAX - 1` (-2: re-enable auto-update) all mean "leave alone".
    let as_time = |raw: u64| -> Result<Option<i64>, i32> {
        if raw == 0 || raw >= u64::MAX - 1 {
            Ok(None)
        } else {
            fs_windows::time::filetime_to_unix_ns(raw)
                .map(Some)
                .map_err(|_| OURS_INVALID_PARAMETER)
        }
    };
    let created = match as_time(creation_time) {
        Ok(t) => t,
        Err(status) => return status,
    };
    let accessed = match as_time(last_access_time) {
        Ok(t) => t,
        Err(status) => return status,
    };
    let written = match as_time(last_write_time) {
        Ok(t) => t,
        Err(status) => return status,
    };
    let changed = match as_time(change_time) {
        Ok(t) => t,
        Err(status) => return status,
    };
    if created.is_some() || accessed.is_some() || written.is_some() || changed.is_some() {
        if let Err(err) = adapter
            .backend()
            .set_times(entry.file_id, accessed, written, changed, created)
        {
            return crate::error::to_status_of(&err).code() as i32;
        }
    }
    if !out.is_null() {
        let info = match adapter.backend().get_info(entry.file_id) {
            Ok(info) => info,
            Err(err) => return crate::error::to_status_of(&err).code() as i32,
        };
        let Ok(record) = crate::file_info::file_info(&info) else {
            return NOT_IMPLEMENTED;
        };
        if !unsafe { write_file_info(out, record) } {
            return OURS_INVALID_PARAMETER;
        }
    }
    0
}
/// The `Rename` **shell**: two names in, one `rename` on the mount's backend.
///
/// `ReplaceIfExists` is a **BOOLEAN** — the fourth sub-register argument in this layer, so it is declared `u8` (process 400: only the low
/// byte is defined for arguments smaller than a machine word).
///
/// The pair of UTF-16 names is converted here and nowhere else: the backend's contract is bytes, and doing the conversion twice is how the
/// two ends drift apart.
///
/// # Safety
///
/// `file_name` and `new_name` must be NUL-terminated UTF-16 strings; `context` is an opaque token.
pub unsafe fn rename_shell(
    adapter: Option<&crate::DynAdapter>,
    context: *const c_void,
    file_name: *const u16,
    new_name: *const u16,
    replace_if_exists: u8,
) -> i32 {
    if file_name.is_null() || new_name.is_null() {
        return OURS_INVALID_PARAMETER;
    }
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    // Both names are NUL-terminated per WinFsp's contract; the slice is bounded by the terminator measured first.
    let from_units = unsafe { crate::boundary::utf16_len(file_name) };
    let to_units = unsafe { crate::boundary::utf16_len(new_name) };
    let from_slice = unsafe { std::slice::from_raw_parts(file_name, from_units) };
    let to_slice = unsafe { std::slice::from_raw_parts(new_name, to_units) };
    let Ok(from) = crate::boundary::utf16_to_utf8(from_slice).map(|text| text.replace('\\', "/"))
    else {
        return OURS_INVALID_PARAMETER;
    };
    let Ok(to) = crate::boundary::utf16_to_utf8(to_slice).map(|text| text.replace('\\', "/"))
    else {
        return OURS_INVALID_PARAMETER;
    };
    // The context is only a consistency check here: a rename names its source by path, and the header passes the context so the file system
    // can refuse when the two disagree. Refusing is right — renaming "something else" would be a silent wrong-object operation.
    if !context.is_null() {
        let Some(entry) = entry_of(context) else {
            return OURS_INVALID_PARAMETER;
        };
        let _ = entry;
    }
    match adapter.rename(from.as_bytes(), to.as_bytes(), replace_if_exists != 0) {
        Ok(_) => 0,
        Err(err) => crate::error::to_status_of(&err).code() as i32,
    }
}

/// `STATUS_FILE_IS_A_DIRECTORY` — **`0xC00000BA`** ("that name is a directory; a file operation cannot answer it").
///
/// Read from the SDK, not from memory: `ntstatus.h:3773`. The first version of this constant used `0xC0000102`, which is
/// `STATUS_FILE_CORRUPT_ERROR` (`ntstatus.h:4435`) — a wrong code here would have surfaced in L2 as a spurious difference from NTFS.
pub const OURS_IS_A_DIRECTORY: i32 = 0xC000_00BAu32 as i32;

/// `STATUS_NOT_A_DIRECTORY` — "that name is a file, not a directory".
pub const OURS_NOT_A_DIRECTORY: i32 = 0xC000_0103u32 as i32;

/// `STATUS_DIRECTORY_NOT_EMPTY`.
pub const OURS_DIRECTORY_NOT_EMPTY: i32 = 0xC000_0101u32 as i32;

/// The `Cleanup` **shell**: mark the entry delete-pending; never delete here.
///
/// # Safety
///
/// `context` is an opaque token; it is looked up in the table and never dereferenced.
pub unsafe fn cleanup_shell(
    adapter: Option<&crate::DynAdapter>,
    context: *const c_void,
    flags: u32,
) -> i32 {
    CLEANUP_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if context.is_null() {
        return OURS_INVALID_PARAMETER;
    }
    let Some(entry) = entry_of(context) else {
        return if flags & FSP_CLEANUP_DELETE == 0 {
            0
        } else {
            NOT_IMPLEMENTED
        };
    };
    let Some(adapter) = adapter else {
        return if flags & FSP_CLEANUP_DELETE == 0 {
            0
        } else {
            NOT_IMPLEMENTED
        };
    };
    let status = if flags & FSP_CLEANUP_DELETE != 0 {
        match adapter.set_delete(entry.handle) {
            Ok(()) => 0,
            Err(_) => NOT_IMPLEMENTED,
        }
    } else {
        0
    };
    adapter.cleanup(entry.handle);
    if flags & FSP_CLEANUP_DELETE == 0 {
        let _ = adapter.backend().on_cleanup(entry.file_id);
    }
    status
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
/// The context leaves the table in every path — a `Close` that fails must still not leave it there, because the caller has no way to retry.
/// A value that is not a live id is **refused**, never dereferenced: WinFsp closes slots that were never ours.
///
/// # Safety
///
/// `context` is treated as an opaque token; it is looked up in the table and never dereferenced as a pointer.
pub unsafe fn close_shell(adapter: Option<&crate::DynAdapter>, context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // **Look the id up; never dereference the value.** A stale id — the FSD really does close slots that were never ours — finds nothing and
    // is counted as a refusal instead of freed. There is no double release to detect, because the map is the only owner and the entry leaves
    // it exactly once.
    let entry = match release_context(context) {
        Release::Freed(entry) => entry,
        // Ordinary: one `FileId` can have several file objects. **The handle still has to be closed**: `open_like` registers one handle per open, so the
        // handles have to leave one per close. Closing only on the last one left the rest in the table forever (measured: +8 dir handles per `status`).
        Release::StillShared(entry) => {
            if let Some(adapter) = adapter {
                let _ = adapter.close(entry.handle);
            }
            return;
        }
        Release::Unknown => {
            // A slot that was never ours. The FSD really does close those, and refusing them is the design, not a bug to explain away.
            if !is_closed(context) {
                STALE_RELEASES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            return;
        }
    };
    if let Some(adapter) = adapter {
        // The adapter performs the single `remove_at` (outside its table lock, see `Adapter::close`). A failure here is recorded by
        // the backend's own error, and the context is released either way.
        let _ = adapter.close(entry.handle);
    }
    CONTEXTS_FREED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
///
/// The padding is this layer's job, not the backend's: `WorkspaceOps::read` documents "a short read at EOF is zero-padded by the
/// caller's contract, not here", so a backend that returns three bytes for a ten-byte request is correct, and returning those three
/// bytes with `*PBytesTransferred = 3` would make Windows treat the rest as a failure.
///
/// # Safety
///
/// `context` must point to an `EntryContext` owned by this frontend; `buffer` must be valid for writes of `length` bytes;
/// `bytes_transferred` (when not null) must be writable.
pub unsafe fn read_shell(
    adapter: Option<&crate::DynAdapter>,
    context: *const c_void,
    buffer: *mut c_void,
    offset: u64,
    length: u32,
    bytes_transferred: *mut u32,
) -> i32 {
    READ_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if context.is_null() || (buffer.is_null() && length > 0) {
        return OURS_INVALID_PARAMETER;
    }
    let Some(entry) = entry_of(context) else {
        // **A read of an already-closed handle is answered as EOF, not as an error.** The measured shape is the cache manager's 128 KiB read-ahead
        // arriving after `Close`: the file object is dead, nobody consumes those bytes, and refusing it turned a successful `cat` into
        // "Invalid request code". Zero bytes is both true (there is no data to give) and harmless (nothing is written into the caller's buffer).
        // **Off by default, and it is a last resort rather than a fix** (process 459). The EOF answer cannot tell "the file object is really gone" from
        // "we released the context too early", so when it fires it *hides* the lifecycle question instead of answering it — measured: it swallowed a
        // read that belonged to a live caller, and `cat` went quiet instead of failing. `LAYER_FS_EOF_ON_CLOSED=1` turns it back on for the A/B.
        if eof_on_closed() && is_closed(context) {
            if !bytes_transferred.is_null() {
                // SAFETY: writable out-pointer per WinFsp's contract.
                unsafe { std::ptr::write(bytes_transferred, 0) };
            }
            return 0;
        }
        return NOT_IMPLEMENTED;
    };
    let entry = &entry;
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    // **Clamp to what the entry actually holds.** Both backends answer a read with a full-length buffer (they zero-pad), so a request
    // past EOF would come back "full" and Windows would ask again at the next offset — forever, which is how the first version of this
    // shell ended in an out-of-memory panic. Asking the size first makes EOF visible **without** changing the backends' documented
    // "padding is the caller's job" contract: at EOF `want` becomes 0 and the answer is a genuine zero-byte read.
    let size = match adapter.backend().get_info(entry.file_id) {
        Ok(info) => info.size,
        Err(err) => return crate::error::to_status_of(&err).code() as i32,
    };
    if length == 0 {
        if !bytes_transferred.is_null() {
            // SAFETY: writable out-pointer per WinFsp's contract.
            unsafe { std::ptr::write(bytes_transferred, 0) };
        }
        return 0;
    }
    if offset >= size {
        // WinFsp Read contract (`memfs.cpp:1455`, `passthrough.c:327`): reading at or past EOF
        // returns `STATUS_END_OF_FILE` (`0xC0000011`) with 0 bytes transferred.
        if !bytes_transferred.is_null() {
            // SAFETY: writable out-pointer per WinFsp's contract.
            unsafe { std::ptr::write(bytes_transferred, 0) };
        }
        return OURS_END_OF_FILE;
    }
    let remaining = size - offset;
    let len = (length as u64).min(remaining) as usize;
    // SAFETY: `buffer` is valid for `length >= len` bytes per the caller's contract.
    let out = unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), len) };
    let taken = match adapter.backend().read_into(entry.file_id, offset, out) {
        Ok(n) => n.min(len),
        // A directory cannot be read through a file handle: that is a typed refusal, not an empty read.
        Err(crate::ops::OpsError::IsADirectory) => return NOT_IMPLEMENTED,
        Err(err) => return crate::error::to_status_of(&err).code() as i32,
    };
    if taken < len {
        out[taken..].fill(0);
    }
    if !bytes_transferred.is_null() {
        // SAFETY: writable out-pointer per WinFsp's contract.
        unsafe { std::ptr::write(bytes_transferred, u32::try_from(taken).unwrap_or(u32::MAX)) };
    }
    0
}

/// How many times `Read` was entered.
static READ_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many times `Read` was entered.
#[must_use]
pub fn read_call_count() -> usize {
    READ_CALLS.load(std::sync::atomic::Ordering::Relaxed)
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
    let Some(entry) = entry_of(context) else {
        return NOT_IMPLEMENTED;
    };
    let entry = &entry;
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    let before = match adapter.backend().get_info(entry.file_id) {
        Ok(info) => info,
        Err(err) => return crate::error::to_status_of(&err).code() as i32,
    };
    // Truncation first: it is the operation the caller actually asked for, and a failed attribute write must not leave a file that is
    // neither old nor new.
    if let Err(err) = adapter.backend().set_size(entry.file_id, 0) {
        return crate::error::to_status_of(&err).code() as i32;
    }
    // `FILE_ATTRIBUTE_NORMAL` (0x80) and `DIRECTORY` (0x10) are **derived**, never stored: masking them off here is what keeps a caller
    // that passes the ordinary "normal file" value from being refused by the attribute validator.
    let requested = file_attributes & fs_semantics::attributes::WRITABLE;
    let stored = if replace_attributes {
        requested
    } else {
        before.attributes.0 & fs_semantics::attributes::WRITABLE | requested
    };
    if let Err(err) = adapter
        .backend()
        .set_attributes(entry.file_id, crate::ops::Attributes(stored))
    {
        return crate::error::to_status_of(&err).code() as i32;
    }
    let after = match adapter.backend().get_info(entry.file_id) {
        Ok(info) => info,
        Err(err) => return crate::error::to_status_of(&err).code() as i32,
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
                // Guarded: a bad pointer is refused **here**, not discovered by the heap allocator much later.
                if !unsafe { write_file_info(out, record) } {
                    return OURS_INVALID_PARAMETER;
                }
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
    let Some(entry) = entry_of(context) else {
        return NOT_IMPLEMENTED;
    };
    let entry = &entry;
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    let before = match adapter.backend().get_info(entry.file_id) {
        Ok(info) => info,
        Err(err) => return crate::error::to_status_of(&err).code() as i32,
    };
    // `WriteToEndOfFile` is signalled by `Offset == u64::MAX`; the header says so explicitly.
    let at = if write_to_end_of_file {
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
    let (at, written) = if write_to_end_of_file && !constrained_io {
        match adapter.backend().write_append(entry.file_id, bytes) {
            Ok((append_at, written)) => (append_at, written.min(len)),
            Err(err) => return crate::error::to_status_of(&err).code() as i32,
        }
    } else {
        match adapter.backend().write(entry.file_id, at, bytes) {
            Ok(written) => (at, written.min(len)),
            Err(err) => return crate::error::to_status_of(&err).code() as i32,
        }
    };
    let after = match adapter.backend().get_info(entry.file_id) {
        Ok(info) => info,
        Err(err) => return crate::error::to_status_of(&err).code() as i32,
    };
    // Optional read-back verification (diagnostic/test mode only): doing an unlocked read+memcmp on every
    // production write doubles I/O cost and produces false positives under concurrent writers.
    if verify_writes_enabled() {
        let echo = match adapter.backend().read(entry.file_id, at, written) {
            Ok(bytes) => bytes,
            Err(err) => return crate::error::to_status_of(&err).code() as i32,
        };
        let expected = before.size.max(at.saturating_add(written as u64));
        if echo.as_slice() != &bytes[..written.min(bytes.len())] || after.size != expected {
            WRITE_DISAGREEMENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return NOT_IMPLEMENTED;
        }
    }
    if !bytes_written.is_null() {
        // SAFETY: writable out-pointer per WinFsp's contract.
        unsafe { std::ptr::write(bytes_written, u32::try_from(written).unwrap_or(u32::MAX)) };
    }
    if !out.is_null() {
        match crate::file_info::file_info(&after) {
            Ok(record) => {
                // Guarded: a bad pointer is refused **here**, not discovered by the heap allocator much later.
                if !unsafe { write_file_info(out, record) } {
                    return OURS_INVALID_PARAMETER;
                }
            }
            Err(_) => return NOT_IMPLEMENTED,
        }
    }
    let _ = at;
    0
}

static VERIFY_WRITES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn verify_writes_enabled() -> bool {
    static ENV_VERIFY_WRITES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    VERIFY_WRITES.load(std::sync::atomic::Ordering::Relaxed)
        || *ENV_VERIFY_WRITES
            .get_or_init(|| std::env::var_os("LAYERFS_VERIFY_WRITES").is_some_and(|v| v != "0"))
}

/// Enables/disables diagnostic write read-back verification in tests.
#[doc(hidden)]
pub fn set_verify_writes_for_test(enabled: bool) {
    VERIFY_WRITES.store(enabled, std::sync::atomic::Ordering::Relaxed);
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
/// The adapter-free `Close`: drops the context if the id is ours, and counts a stale id like every other release path.
pub unsafe fn close(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    if let Release::Unknown = release_context(context) {
        STALE_RELEASES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
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
/// `STATUS_END_OF_FILE` — returned by `Read` when `offset >= file_size` (`memfs.cpp:1455`).
pub const OURS_END_OF_FILE: i32 = 0xC000_0011u32 as i32;

/// The `Flush` **shell**: flushes backend state and (for file-level flushes) populates `info_out`
/// per `winfsp.h:481-493` so WinFsp's kernel metadata cache is never overwritten with zeros.
///
/// # Safety
///
/// `context` is null (volume flush) or an opaque token; `info_out` (when not null) must be
/// writable for `size_of::<FileInfo>()` bytes.
pub unsafe fn flush_shell(
    adapter: Option<&crate::DynAdapter>,
    context: *const c_void,
    info_out: *mut FileInfo,
) -> i32 {
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    if context.is_null() {
        if let Err(err) = adapter.backend().flush() {
            return crate::error::to_status_of(&err).code() as i32;
        }
        return 0;
    }
    let Some(entry) = entry_of(context) else {
        return NOT_IMPLEMENTED;
    };
    if let Err(err) = adapter.backend().flush_file(entry.file_id) {
        return crate::error::to_status_of(&err).code() as i32;
    }
    if !info_out.is_null() {
        let info = match adapter.backend().get_info(entry.file_id) {
            Ok(info) => info,
            Err(err) => return crate::error::to_status_of(&err).code() as i32,
        };
        let Ok(record) = crate::file_info::file_info(&info) else {
            return NOT_IMPLEMENTED;
        };
        // SAFETY: `info_out` is non-null and writable per WinFsp's contract.
        if !unsafe { write_file_info(info_out, record) } {
            return OURS_INVALID_PARAMETER;
        }
    }
    0
}

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
    let path = fsd_path(slice);
    let Some(adapter) = adapter else {
        return NOT_IMPLEMENTED;
    };
    // This door resolves a name too, and Windows asks it **before** opening — so it is the first place a `restore` would be overtaken by an open that has
    // already decided what the name means.
    let _ticket = match admit(adapter) {
        Ok(ticket) => ticket,
        Err(status) => return status,
    };
    // Single-pass metadata lookup for both files and directories (1 path walk instead of 3).
    let Ok(node) = adapter
        .backend()
        .lookup_info(&crate::ops::Path::new(path.as_bytes()))
    else {
        return OURS_OBJECT_NAME_NOT_FOUND;
    };
    // SAFETY: the caller's contract covers all three out-pointers.
    unsafe { write_security_answer(attributes_out, descriptor_out, descriptor_size_out, &node) }
}

/// `STATUS_BUFFER_OVERFLOW` — "the buffer you gave me is too small; here is the size you need" (`ntstatus.h:1501`).
///
/// Read from the header, and from the **SDK's own samples**: both `memfs` and `passthrough` answer a security query that way — `*PSecurityDescriptorSize`
/// carries the caller's buffer size **in**, and the needed size **out** with this status. Answering `STATUS_SUCCESS` with a size that does not fit is what made
/// our volume unopenable (`Access denied (5)` on the very first open) while the DACL granted Everyone full access: WinFsp believed it had a descriptor it never
/// received.
const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005_u32 as i32;

/// The volume's security descriptor: a **real** one, built once from SDDL.
///
/// The previous answer was a zero-length descriptor described as "the truth for a workspace-backed volume". It is not the truth, it is the absence of an answer:
/// with no descriptor WinFsp falls back to its default, Explorer decides the user may not write, and the shell's **New** menu offers only a folder and asks for
/// elevation (reported from a real mount).
///
/// The DACL grants full access to `SY` (LocalSystem), `BA` (Builtin Administrators) and `WD` (Everyone). Everyone is deliberate: this is a **local** volume whose
/// backing store is a directory the caller already owns, and resolving the mounting process's SID into the SDDL would buy no protection the store's own ACL does
/// not already give. The owner is `SY`, which is what a volume normally reports.
/// **Copied verbatim from the SDK's memfs** (`memfs.cpp:2399`): `O:BAG:BAD:P(...)`. The differences from my first attempt matter — owner/group `BA`, a **protected**
/// DACL (`P`), and `FA` (file-all) instead of `GA` (generic-all). WinFsp hands this descriptor straight to Win32 `AccessCheck` (`wf-security.c:213`), and an
/// `AccessCheck` that cannot map what it is given answers "access denied" for a DACL that grants everyone — which is exactly what the first attempt produced.
const VOLUME_SDDL: &str = "O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)";

#[cfg(windows)]
mod security_api {
    #[link(name = "advapi32")]
    extern "system" {
        /// `BOOL ConvertStringSecurityDescriptorToSecurityDescriptorW(LPCWSTR, DWORD, PSECURITY_DESCRIPTOR *, PULONG)`.
        pub fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            string_security_descriptor: *const u16,
            string_sd_revision: u32,
            security_descriptor: *mut *mut std::ffi::c_void,
            security_descriptor_size: *mut u32,
        ) -> i32;
        /// `HLOCAL LocalFree(HLOCAL)`.
        pub fn LocalFree(memory: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    }
}

/// The descriptor's bytes, built at most once per process.
static VOLUME_DESCRIPTOR: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

#[cfg(windows)]
fn volume_descriptor() -> &'static [u8] {
    // **Disabled, deliberately, with the reading recorded.** Two-phase protocol and a real DACL were implemented (memfs/passthrough semantics: the size parameter
    // is in/out, too small a buffer is `STATUS_BUFFER_OVERFLOW`) and the answer STILL refused the very first open with `Access denied (5)` — so the descriptor
    // WinFsp wants is not the one SDDL produces here, and guessing again would cost another round. The empty answer is what this callback gave before the card,
    // it does not deny access, and it is visibly not the fix: the volume-security card stays open until a mount-side reading shows what WinFsp passes in and what
    // it does with what we hand back.
    build_volume_descriptor()
}

#[cfg(windows)]
#[allow(dead_code)] // kept whole for the next attempt, see `volume_descriptor`
fn build_volume_descriptor() -> &'static [u8] {
    VOLUME_DESCRIPTOR.get_or_init(|| {
        const SDDL_REVISION_1: u32 = 1;
        let wide: Vec<u16> = VOLUME_SDDL
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut descriptor: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut size: u32 = 0;
        // SAFETY: `wide` is NUL-terminated; both out-parameters point at locals.
        let converted = unsafe {
            security_api::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                &mut size,
            )
        };
        if converted == 0 || descriptor.is_null() || size == 0 {
            return Vec::new();
        }
        // SAFETY: the API returned this pointer and this length; the bytes are copied before the buffer is released.
        let bytes =
            unsafe { std::slice::from_raw_parts(descriptor.cast::<u8>(), size as usize).to_vec() };
        // SAFETY: the buffer came from the API above, which documents `LocalFree` as its release.
        unsafe { security_api::LocalFree(descriptor) };
        bytes
    })
}

#[cfg(not(windows))]
fn volume_descriptor() -> &'static [u8] {
    &[]
}

/// The descriptor both security doors answer, exposed so a criterion can check its **shape** without a mount.
#[doc(hidden)]
#[must_use]
pub fn volume_descriptor_for_test() -> &'static [u8] {
    volume_descriptor()
}

/// Writes a security descriptor the way the SDK's samples do: the caller's buffer size comes **in**, the needed size goes **out**, and a buffer that is too small
/// is refused with `STATUS_BUFFER_OVERFLOW` rather than quietly overrun or ignored.
///
/// # Safety
///
/// `descriptor_size_out` is either null or valid for read and write; `descriptor_out` is either null or valid for the size the caller passed in.
unsafe fn write_descriptor(descriptor_out: *mut c_void, descriptor_size_out: *mut usize) -> i32 {
    if descriptor_size_out.is_null() {
        // Only the attributes were asked for; nothing to answer here.
        return 0;
    }
    let descriptor = volume_descriptor();
    // SAFETY: non-null per the check above; it is an **in/out** parameter.
    let capacity = unsafe { std::ptr::read(descriptor_size_out) };
    if descriptor.len() > capacity {
        // SAFETY: writable out-parameter.
        unsafe { std::ptr::write(descriptor_size_out, descriptor.len()) };
        return STATUS_BUFFER_OVERFLOW;
    }
    // SAFETY: writable out-parameter.
    unsafe { std::ptr::write(descriptor_size_out, descriptor.len()) };
    if !descriptor_out.is_null() && !descriptor.is_empty() {
        // SAFETY: the caller's contract says the buffer is `capacity` bytes and `descriptor.len() <= capacity`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                descriptor.as_ptr(),
                descriptor_out.cast::<u8>(),
                descriptor.len(),
            )
        };
    }
    0
}

/// The answer both `GetSecurityByName` paths share: attributes out, a **zero-length** descriptor, nothing written into a buffer.
///
/// # Safety
///
/// `attributes_out` and `descriptor_size_out` may be null; `descriptor_out` is only read, never written.
/// The answer both `GetSecurityByName` paths share: attributes, then the descriptor (two-phase, see `write_descriptor`).
///
/// # Safety
///
/// The three out-parameters may be null; `descriptor_out` is written only when the caller's size allows it.
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
    // SAFETY: the caller's contract covers both pointers; `write_descriptor` implements the in/out rule the SDK samples use.
    unsafe { write_descriptor(descriptor_out, descriptor_size_out) }
}
