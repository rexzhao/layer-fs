//! Diagnosing and loading the WinFsp DLL (P3-4), with failures that say **which** problem happened.
//!
//! "load failed" is the least useful error a mount can produce: it covers "the SDK is not installed", "the path is wrong",
//! "the DLL is the wrong architecture" and "the DLL is there but too old to export what we need", and each of those has a
//! different fix. This module splits them before any `LoadLibraryW` call is attempted, so a first-handshake failure points
//! at one of them.

use std::path::{Path, PathBuf};

/// Why the library could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// The file is not there. Almost always "the SDK is not installed" or "the path points at another machine's layout".
    NotFound {
        /// The path that was tried.
        path: PathBuf,
    },
    /// The file exists but an expected export is missing: a **version** problem, not an installation one.
    MissingExport {
        /// The export name that was not found.
        name: String,
    },
    /// `LoadLibraryW` itself failed, with the OS error that says why (wrong architecture, dependencies missing, …).
    LoadFailed {
        /// The path that was tried.
        path: PathBuf,
        /// `GetLastError` from the loader.
        last_error: u32,
    },
}

impl LoadError {
    /// A one-line description for the caller's log, naming the *kind* of problem before the detail.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            LoadError::NotFound { path } => {
                format!(
                    "winfsp library not found at {} (is the SDK installed?)",
                    path.display()
                )
            }
            LoadError::MissingExport { name } => format!(
                "winfsp library is present but does not export `{name}` (wrong or too-old version)"
            ),
            LoadError::LoadFailed { path, last_error } => format!(
                "winfsp library at {} failed to load (GetLastError = {last_error})",
                path.display()
            ),
        }
    }
}

/// Where the real WinFsp runtime lives on a normal install (WinFsp 2.x, 64-bit).
///
/// Verified on this machine: `LoadLibraryW` + `GetProcAddress("FspVersion")` → **2.0.1**. This is the only copy of the three
/// same-named DLLs on this box that may be called through its export table.
pub const PROGRAM_FILES_X86: &str = "C:/Program Files (x86)/WinFsp/bin/winfsp-x64.dll";

/// Environment override, so a test can point at another build without editing code.
pub const ENV_OVERRIDE: &str = "WINFSP_DLL";

/// The library a caller should try by default: the environment override if set, else [`PROGRAM_FILES_X86`].
///
/// **Do not** use `F:/work/winfsp-sdk/.../DYNAMIC/SxS/.../winfsp-x64.dll`: that is the dynamic-loader variant, and calling
/// its exports through `GetProcAddress` crashes with `0xC0000005`. Three same-named DLLs on one machine, only one callable
/// that way — which is why the handshake exists.
#[must_use]
pub fn default_library() -> PathBuf {
    match std::env::var_os(ENV_OVERRIDE) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(PROGRAM_FILES_X86),
    }
}

/// Checks that `path` exists before a loader call.
///
/// Separated from the `LoadLibraryW` call so the "not installed" case can be tested without Windows in the picture, and so
/// the error carries the **path that was tried** rather than a bare failure.
///
/// # Errors
///
/// [`LoadError::NotFound`] with the path.
pub fn resolve_library(path: &Path) -> Result<(), LoadError> {
    if path.is_file() {
        return Ok(());
    }
    Err(LoadError::NotFound {
        path: path.to_path_buf(),
    })
}
