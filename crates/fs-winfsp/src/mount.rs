//! Creating, starting, stopping and deleting a WinFsp file system **without mounting it** (P3-4 step ③).
//!
//! Four calls, no mount point: that is the smallest experiment that exercises the library's lifecycle without a drive letter
//! and without asserting anything about callbacks. `FspFileSystemCreate` takes the device path, the (opaque) volume
//! parameters and the 64-slot interface table; `StartDispatcher` spawns the threads that will later serve callbacks;
//! `StopDispatcher` ends them; `Delete` frees the object.
//!
//! Mounting (`FspFileSystemSetMountPoint`) is deliberately **not** here: it needs a directory and can leave a stuck drive
//! behind if something goes wrong, and it would mix "the library's lifecycle works" with "our callbacks are right".

use std::path::Path;

use crate::loader::{resolve_library, LoadError};

/// The device name this frontend creates (`fsctl.h:52`: `winfsp` + `.Disk`).
///
/// The disk device is the one used by user-mode file systems that appear as a drive; the network device would be for a UNC
/// provider. Named here rather than inlined so the choice is visible.
pub const DISK_DEVICE: &str = "winfsp.Disk";

/// What the four lifecycle calls reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lifecycle {
    /// `FspFileSystemCreate` status.
    pub create: i32,
    /// `FspFileSystemStartDispatcher` status.
    pub start: i32,
    /// Whether the library offers a way to observe the dispatcher threads ending. WinFsp exposes no such query at this
    /// level, so this is `false` and the report says "not observable" rather than substituting "we can start again".
    pub stop_is_observable: bool,
}

/// Runs create → start → stop → delete against `library`.
///
/// Nothing is mounted, so a failure here is about the library or our structures, never about a callback.
///
/// # Errors
///
/// [`LoadError`] from the loader or a missing export.
pub fn lifecycle(library: &Path) -> Result<Lifecycle, LoadError> {
    resolve_library(library)?;
    platform::lifecycle(library)
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use crate::abi::{FileSystemInterface, VolumeParams};
    use crate::loader::LoadError;

    use super::Lifecycle;

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *const c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    type CreateFn = extern "system" fn(
        *const u16,
        *const VolumeParams,
        *const FileSystemInterface,
        *mut *mut c_void,
    ) -> i32;
    type StartFn = extern "system" fn(*mut c_void, u32) -> i32;
    type StopFn = extern "system" fn(*mut c_void);
    type DeleteFn = extern "system" fn(*mut c_void);

    unsafe fn symbol(module: *mut c_void, name: &str) -> Option<*const c_void> {
        let mut narrow: Vec<u8> = name.as_bytes().to_vec();
        narrow.push(0);
        let address = unsafe { GetProcAddress(module, narrow.as_ptr()) };
        (!address.is_null()).then_some(address)
    }

    pub(super) fn lifecycle(library: &Path) -> Result<Lifecycle, LoadError> {
        let mut wide: Vec<u16> = library.as_os_str().encode_wide().collect();
        wide.push(0);
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let module = unsafe { LoadLibraryW(wide.as_ptr()) };
        if module.is_null() {
            let last_error = unsafe { GetLastError() };
            return Err(LoadError::LoadFailed {
                path: library.to_path_buf(),
                last_error,
            });
        }

        let missing = |name: &str| LoadError::MissingExport {
            name: name.to_owned(),
        };
        // SAFETY: each address comes from this module's export table; the signatures are the SDK's.
        // `symbol` already returns an `Option` of raw addresses; only the transmutes are unsafe, and each is annotated.
        let (create, start, stop, delete) = {
            let address = |name: &str| match unsafe { symbol(module, name) } {
                Some(address) => Ok(address),
                None => Err(missing(name)),
            };
            let (create, start, stop, delete) = match (
                address("FspFileSystemCreate"),
                address("FspFileSystemStartDispatcher"),
                address("FspFileSystemStopDispatcher"),
                address("FspFileSystemDelete"),
            ) {
                (Ok(create), Ok(start), Ok(stop), Ok(delete)) => (create, start, stop, delete),
                (Err(error), ..)
                | (_, Err(error), ..)
                | (_, _, Err(error), _)
                | (_, _, _, Err(error)) => {
                    unsafe { FreeLibrary(module) };
                    return Err(error);
                }
            };
            // SAFETY: the four addresses came from this module's export table, and the signatures are the SDK's.
            unsafe {
                (
                    std::mem::transmute::<*const c_void, CreateFn>(create),
                    std::mem::transmute::<*const c_void, StartFn>(start),
                    std::mem::transmute::<*const c_void, StopFn>(stop),
                    std::mem::transmute::<*const c_void, DeleteFn>(delete),
                )
            }
        };

        let mut params = VolumeParams::zeroed();
        params.set_version_size();
        params.set_sector_size(4096);
        params.set_sectors_per_allocation_unit(1);
        params.set_max_component_length(255);
        params.set_file_info_timeout(0);
        let interface = FileSystemInterface::empty();

        let mut device: Vec<u16> = super::DISK_DEVICE.encode_utf16().collect();
        device.push(0);

        let mut file_system: *mut c_void = std::ptr::null_mut();
        // SAFETY: all pointers outlive the call; `file_system` is a valid out-parameter.
        let create_status = create(device.as_ptr(), &params, &interface, &mut file_system);
        if create_status != 0 || file_system.is_null() {
            unsafe { FreeLibrary(module) };
            return Ok(Lifecycle {
                create: create_status,
                start: 0,
                stop_is_observable: false,
            });
        }

        // `file_system` came from a successful create; 0 lets WinFsp choose the thread count.
        let start_status = start(file_system, 0);
        // Stopping a non-started dispatcher is a no-op per the SDK.
        stop(file_system);
        // After stop, delete is the documented teardown.
        delete(file_system);
        // SAFETY: paired with the successful load.
        unsafe { FreeLibrary(module) };

        Ok(Lifecycle {
            create: create_status,
            start: start_status,
            // WinFsp 2.x has no query for "are the dispatcher threads gone"; see the docs on this field.
            stop_is_observable: false,
        })
    }
}

#[cfg(not(windows))]
mod platform {
    use std::path::Path;

    use crate::loader::LoadError;

    use super::Lifecycle;

    pub(super) fn lifecycle(library: &Path) -> Result<Lifecycle, LoadError> {
        Err(LoadError::LoadFailed {
            path: library.to_path_buf(),
            last_error: 0,
        })
    }
}
