//! WinFsp's own FSD trace (`FspDebugLogSetHandle` + `FspFileSystemSetDebugLogF`), attached per mount.
//!
//! This is the FSD-side observer: every IRP the driver sees, and every decision it makes, is written to one file. It is the
//! only way to tell "the driver rejected the request" from "the driver asked us and we answered wrongly" — an NTSTATUS alone
//! cannot, which is what a long chain of zeroed counters has shown.
//!
//! The ordering rules come from memfs and from the API shapes:
//!
//! - `FspDebugLogSetHandle` is **process-wide** (it takes no file system), so it must be set **before** `FspFileSystemCreate`
//!   or the first traces are lost;
//! - `FspFileSystemSetDebugLogF(fs, 0xffffffff)` is per file system and goes right after `create`;
//! - the handle must be **closed before the file is read**, or the tail is still buffered.
//!
//! The `…F` (function) exports are used on purpose: the SDK's inline versions write into private structure fields we do not
//! mirror.

use std::path::Path;

/// An open log file, identified by its raw handle.
#[derive(Clone, Copy, Debug)]
pub struct LogHandle(usize);

/// Opens (or creates) the log file for appending.
///
/// Returns `None` when the file cannot be opened — logging is an observation, never a reason for the mount to fail.
#[must_use]
pub fn open(path: &Path) -> Option<LogHandle> {
    platform::open(path).map(LogHandle)
}

/// Points WinFsp's trace at the log and turns on **all** trace flags for `file_system`.
///
/// `module` is the loaded WinFsp module; the exports are looked up here so `mounted.rs` does not have to grow two more symbol
/// tables.
pub fn attach(log: LogHandle, module: *mut std::ffi::c_void, file_system: *mut std::ffi::c_void) {
    platform::attach(log.0, module, file_system);
}

/// Flushes and closes the log. Call **before** reading it.
pub fn close(log: LogHandle) {
    platform::close(log.0);
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security: *const c_void,
            creation: u32,
            flags: u32,
            template: *mut c_void,
        ) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *const c_void;
    }

    pub(super) fn open(path: &Path) -> Option<usize> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        // GENERIC_WRITE = 0x4000_0000, FILE_SHARE_READ|FILE_SHARE_WRITE = 0x3, OPEN_ALWAYS = 4, FILE_ATTRIBUTE_NORMAL = 0x80.
        // SAFETY: `wide` is NUL-terminated and outlives the call; no security descriptor and no template are wanted.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0x4000_0000,
                0x3,
                std::ptr::null(),
                4,
                0x80,
                std::ptr::null_mut(),
            )
        };
        if handle.is_null() || handle == usize::MAX as *mut c_void {
            return None;
        }
        Some(handle as usize)
    }

    pub(super) fn attach(log: usize, module: *mut c_void, file_system: *mut c_void) {
        let handle = log as *mut c_void;
        let resolve = |name: &str| -> Option<*const c_void> {
            let mut narrow: Vec<u8> = name.as_bytes().to_vec();
            narrow.push(0);
            // SAFETY: both pointers are valid for the call.
            let address = unsafe { GetProcAddress(module, narrow.as_ptr()) };
            (!address.is_null()).then_some(address)
        };

        if let Some(address) = resolve("FspDebugLogSetHandle") {
            // SAFETY: `FspDebugLogSetHandle(HANDLE)` — the module's own export, called with a handle from `CreateFileW`.
            let set: extern "system" fn(*mut c_void) = unsafe { std::mem::transmute(address) };
            set(handle);
        }
        if let Some(address) = resolve("FspFileSystemSetDebugLogF") {
            // SAFETY: `FspFileSystemSetDebugLogF(FSP_FILE_SYSTEM*, UINT32)` — all trace flags.
            let on: extern "system" fn(*mut c_void, u32) = unsafe { std::mem::transmute(address) };
            on(file_system, 0xffff_ffff);
        }
    }

    pub(super) fn close(log: usize) {
        // SAFETY: the handle came from `CreateFileW` in `open` and is closed exactly once.
        unsafe { CloseHandle(log as *mut c_void) };
    }
}

#[cfg(not(windows))]
mod platform {
    use std::ffi::c_void;
    use std::path::Path;

    pub(super) fn open(_path: &Path) -> Option<usize> {
        None
    }

    pub(super) fn attach(_log: usize, _module: *mut c_void, _file_system: *mut c_void) {}

    pub(super) fn close(_log: usize) {}
}
