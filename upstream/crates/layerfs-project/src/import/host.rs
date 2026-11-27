//! Cross-platform host path and metadata helpers for `layerfs-project` Init.
//!
//! Keeps Unix byte-exact behavior unchanged while enabling safe (`#![forbid(unsafe_code)]`)
//! Windows directory import via `layerfs-win32-host`.

use crate::backing::Stamp;
#[cfg(windows)]
use crate::error::io_path;
use crate::ProjectError;
use std::{
    ffi::OsStr,
    fs::{File, Metadata},
    path::{Path, PathBuf},
};

/// Normalizes a validated source root directory for host traversal.
///
/// On Windows, `std::fs::canonicalize` returns a verbatim `\\?\...` path so
/// descendant traversal, metadata queries, and file opens bypass Win32 DOS
/// device name mangling (e.g. literal files named `nul`, `con`, `aux`) and the
/// legacy 260-character `MAX_PATH` limit.
#[cfg(windows)]
pub(crate) fn canonical_source_root(source: &Path) -> Result<PathBuf, ProjectError> {
    std::fs::canonicalize(source).map_err(|error| io_path("canonicalize", source, error))
}

#[cfg(unix)]
pub(crate) fn canonical_source_root(source: &Path) -> Result<PathBuf, ProjectError> {
    Ok(source.to_path_buf())
}

#[cfg(unix)]
pub(crate) fn os_str_to_bytes(os: &OsStr) -> Result<Vec<u8>, ProjectError> {
    use std::os::unix::ffi::OsStrExt;
    Ok(os.as_bytes().to_vec())
}

#[cfg(windows)]
pub(crate) fn os_str_to_bytes(os: &OsStr) -> Result<Vec<u8>, ProjectError> {
    os.to_str()
        .map(|s| s.as_bytes().to_vec())
        .ok_or(ProjectError::InvalidInput)
}

#[cfg(unix)]
pub(crate) fn bytes_to_path(bytes: Vec<u8>) -> Result<PathBuf, ProjectError> {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(windows)]
pub(crate) fn bytes_to_path(bytes: Vec<u8>) -> Result<PathBuf, ProjectError> {
    String::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|_| ProjectError::InvalidInput)
}

#[cfg(unix)]
pub(crate) fn join_name(directory: &Path, name: &[u8]) -> Result<PathBuf, ProjectError> {
    use std::os::unix::ffi::OsStrExt;
    Ok(directory.join(OsStr::from_bytes(name)))
}

#[cfg(windows)]
pub(crate) fn join_name(directory: &Path, name: &[u8]) -> Result<PathBuf, ProjectError> {
    let text = std::str::from_utf8(name).map_err(|_| ProjectError::InvalidInput)?;
    Ok(directory.join(text))
}

pub(crate) fn child_path_bytes(directory: &Path, name: &[u8]) -> Result<Vec<u8>, ProjectError> {
    let joined = join_name(directory, name)?;
    os_str_to_bytes(joined.as_os_str())
}

#[cfg(unix)]
pub(crate) fn dev_ino_of_path(path: &Path, metadata: &Metadata) -> Result<(u64, u64), ProjectError> {
    use std::os::unix::fs::MetadataExt;
    let _ = path;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
pub(crate) fn dev_ino_of_path(
    path: &Path,
    _metadata: &Metadata,
) -> Result<(u64, u64), ProjectError> {
    let id = layerfs_win32_host::path_identity(path)
        .map_err(|error| io_path("path_identity", path, error))?;
    Ok((id.device, id.inode))
}

#[cfg(unix)]
pub(crate) fn stamp_of_path(_path: &Path, metadata: &Metadata) -> Result<Stamp, ProjectError> {
    use std::os::unix::fs::MetadataExt;
    Ok(Stamp {
        dev: metadata.dev(),
        ino: metadata.ino(),
        len: metadata.len(),
        mode: metadata.mode(),
        mtime: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec(),
        ctime: metadata.ctime(),
        ctime_nsec: metadata.ctime_nsec(),
    })
}

#[cfg(unix)]
pub(crate) fn stamp_of_file(_file: &File, metadata: &Metadata) -> Result<Stamp, ProjectError> {
    stamp_of_path(Path::new(""), metadata)
}

#[cfg(windows)]
fn system_time_parts(time: std::io::Result<std::time::SystemTime>) -> (i64, i64) {
    use std::time::UNIX_EPOCH;
    let Ok(time) = time else {
        return (0, 0);
    };
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, i64::from(d.subsec_nanos())),
        Err(err) => {
            let d = err.duration();
            let secs = d.as_secs() as i64;
            let nanos = i64::from(d.subsec_nanos());
            if nanos == 0 {
                (-secs, 0)
            } else {
                (-secs - 1, 1_000_000_000 - nanos)
            }
        }
    }
}

#[cfg(windows)]
fn windows_portable_mode(metadata: &Metadata) -> u32 {
    if metadata.file_type().is_symlink() {
        0o777
    } else if metadata.is_dir() {
        0o755
    } else if metadata.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

#[cfg(windows)]
pub(crate) fn stamp_of_path(path: &Path, metadata: &Metadata) -> Result<Stamp, ProjectError> {
    let id = layerfs_win32_host::path_identity(path)
        .map_err(|error| io_path("path_identity", path, error))?;
    let (mtime, mtime_nsec) = system_time_parts(metadata.modified());
    let (ctime, ctime_nsec) = system_time_parts(metadata.created());
    Ok(Stamp {
        dev: id.device,
        ino: id.inode,
        len: metadata.len(),
        mode: windows_portable_mode(metadata),
        mtime,
        mtime_nsec,
        ctime,
        ctime_nsec,
    })
}

#[cfg(windows)]
pub(crate) fn stamp_of_file(file: &File, metadata: &Metadata) -> Result<Stamp, ProjectError> {
    let id = layerfs_win32_host::file_identity(file)?;
    let (mtime, mtime_nsec) = system_time_parts(metadata.modified());
    let (ctime, ctime_nsec) = system_time_parts(metadata.created());
    Ok(Stamp {
        dev: id.device,
        ino: id.inode,
        len: metadata.len(),
        mode: windows_portable_mode(metadata),
        mtime,
        mtime_nsec,
        ctime,
        ctime_nsec,
    })
}
