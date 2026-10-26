//! The first real handshake with WinFsp (P3-4): load the DLL, fetch **one** symbol, read its version.
//!
//! Deliberately nothing more. Three things have to be true before any filesystem work is meaningful — the library loads,
//! a symbol can be fetched from it, and the version can be read — and each of them fails for a different reason. Wiring the
//! callback table in the same step would put four unknowns behind one symptom.
//!
//! No `WinFsp` types cross this boundary yet: `FspVersion` is a plain `fn() -> u32`, so the handshake can be verified
//! without any of the structures being right.

use std::path::Path;

use crate::loader::{resolve_library, LoadError};

/// The version reported by the loaded library, e.g. `0x0002_0001` for 2.1.
///
/// # Errors
///
/// [`LoadError::NotFound`] when the library is absent, [`LoadError::LoadFailed`] when the loader refuses it, and
/// [`LoadError::MissingExport`] when the symbol is not there (a version problem, not an installation one).
pub fn read_version(library: &Path) -> Result<u32, LoadError> {
    resolve_library(library)?;
    platform::read_version(library)
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use crate::loader::LoadError;

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *const c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    pub(super) fn read_version(library: &Path) -> Result<u32, LoadError> {
        let mut wide: Vec<u16> = library.as_os_str().encode_wide().collect();
        wide.push(0);

        // SAFETY: `wide` is NUL-terminated and lives until the call returns; `LoadLibraryW` does not retain the pointer.
        let module = unsafe { LoadLibraryW(wide.as_ptr()) };
        if module.is_null() {
            let last_error = unsafe { GetLastError() };
            return Err(LoadError::LoadFailed {
                path: library.to_path_buf(),
                last_error,
            });
        }

        // `GetProcAddress` takes a NUL-terminated **narrow** name.
        let symbol = b"FspVersion\0";
        // SAFETY: both pointers are valid for the call; the returned pointer is only used as a function address below.
        let address = unsafe { GetProcAddress(module, symbol.as_ptr()) };
        if address.is_null() {
            // SAFETY: `module` came from a successful `LoadLibraryW` and is not used afterwards.
            unsafe { FreeLibrary(module) };
            return Err(LoadError::MissingExport {
                name: "FspVersion".to_owned(),
            });
        }

        // SAFETY: `FspVersion` is documented as `ULONG FspVersion(VOID)`; the address came from that module's export table.
        let version: u32 = unsafe {
            let function: extern "system" fn() -> u32 = std::mem::transmute(address);
            function()
        };
        // SAFETY: as above — paired with the successful load.
        unsafe { FreeLibrary(module) };
        Ok(version)
    }
}

#[cfg(not(windows))]
mod platform {
    use std::path::Path;

    use crate::loader::LoadError;

    pub(super) fn read_version(library: &Path) -> Result<u32, LoadError> {
        // Not a failure of the environment we test on: the handshake is a Windows-only step, and saying so explicitly beats
        // a compile-time absence that would make the caller's error handling untested everywhere else.
        Err(LoadError::LoadFailed {
            path: library.to_path_buf(),
            last_error: 0,
        })
    }
}
