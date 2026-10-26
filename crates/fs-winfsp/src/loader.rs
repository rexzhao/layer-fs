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

/// The default location of the SDK's dynamic library on this machine, if the environment says otherwise use
/// [`resolve_library`].
///
/// **Not** the `SxS/DYNAMIC/bin` copy: loading that one and calling `FspVersion` through `GetProcAddress` **crashes**
/// (`0xC0000005`), because it is the dynamic-loader flavour of the library and expects to be entered through its own API
/// rather than have its exports called directly. Recorded here so the next person does not re-try it: the crash was found
/// by this very handshake, which is what it is for.
pub const DEFAULT_LIBRARY: &str = "F:/work/winfsp-sdk/DYN/x64/winfsp-x64.dll";

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
