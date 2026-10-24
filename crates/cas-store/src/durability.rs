//! The durability primitives the publication order needs (ADR-002 §5).
//!
//! This is the only module in the crate that touches a platform API. Everything
//! above it works in terms of "make this file durable" and "make this directory
//! entry durable", so a port to another platform changes one file.
//!
//! # What is actually guaranteed
//!
//! - `sync_file`: the file's bytes reach stable storage before it returns
//!   (`File::sync_all`: `fsync` on Unix, `FlushFileBuffers` on Windows).
//! - `sync_directory`: the directory's entries reach stable storage.
//!   - Unix: `fsync` on a directory is a documented, meaningful operation.
//!   - Windows: there is no documented Win32 API for this. A directory handle
//!     opened with `FILE_FLAG_BACKUP_SEMANTICS` can be passed to
//!     `FlushFileBuffers`, and it succeeds on NTFS, but the combination is not
//!     a documented durability contract (MS-FSA §2.1.5.7 describes the
//!     abstract object store, and the behaviour is NTFS-specific).
//!
//!   So on Windows this call is **best effort**: it does not fail the caller
//!   when the flush fails, and it does not license a claim that a `rename`
//!   survives a power loss. It is called anyway because it costs one syscall
//!   and, where the platform does honour it, removes a real window. What the
//!   project does promise — and tests — is recovery from **process** crashes,
//!   which is a strictly weaker claim than power-loss durability.
//!
//! `rename_replace` publishes a staged file atomically: on both supported
//! platforms a rename onto an existing path either completes or does not,
//! leaving no partially written target visible to a reader.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// A handle that stops other processes from deleting a freshly published pack (ADR-008 §5.2).
///
/// A new pack is written, renamed into `packs/` and then named by an index row. Between the
/// rename and that commit, an external deleter (a real-time AV scanner is the observed case)
/// can remove the file, and the store would then hold rows pointing at nothing. Holding the
/// file open **without** `FILE_SHARE_DELETE` makes such a delete fail for its lifetime.
///
/// The lifetime is deliberately narrow: acquired right after the rename, dropped as soon as
/// the index transaction has committed. It adds no `fsync` and changes no publication step; it
/// only denies deletion during the window in which the file is not yet referenced.
#[derive(Debug)]
pub(crate) struct Pin {
    // The handle only has to exist; nothing is read through it.
    _file: File,
}

impl Pin {
    /// Opens `path` for reading with a share mode that forbids deletion.
    ///
    /// # Errors
    ///
    /// Whatever the platform reports; the caller decides what a failure means.
    pub(crate) fn hold(path: &Path) -> io::Result<Self> {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            const FILE_SHARE_READ: u32 = 0x0000_0001;
            const FILE_SHARE_WRITE: u32 = 0x0000_0002;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(path)?;
            Ok(Self { _file: file })
        }
        // On Unix an open handle does not deny unlinking (the name can be removed while the
        // inode lives on), so this makes **no promise** there: it is compiled so the call sites
        // are platform-independent, and the test that asserts the guarantee is `cfg(windows)`.
        #[cfg(not(windows))]
        {
            Ok(Self {
                _file: File::open(path)?,
            })
        }
    }
}

/// Makes a file's contents durable.
pub(crate) fn sync_file(file: &File) -> io::Result<()> {
    #[cfg(feature = "sync-probe")]
    crate::probe::record(crate::probe::Event::SyncFile);
    file.sync_all()
}

/// Renames `from` onto `to`, replacing `to` if it exists.
///
/// Both paths must be on the same filesystem; a cross-device rename fails with
/// an error rather than being emulated by copy-and-delete (ADR-002 §3).
pub(crate) fn rename_replace(from: &Path, to: &Path) -> io::Result<()> {
    let result = std::fs::rename(from, to);
    // Record only a rename that happened: a failed rename is part of a staging attempt that
    // publishes nothing, and the sequence checker must be able to tell those apart.
    #[cfg(feature = "sync-probe")]
    if result.is_ok() {
        crate::probe::record(crate::probe::Event::Rename);
    }
    result
}

/// Makes a directory's entries durable, best effort (see the module docs).
///
/// Returns `Ok(())` on Windows even when the flush reports failure, because
/// that platform has no contract to fail against; a genuine open failure is
/// still reported so a missing directory is not silently ignored.
pub(crate) fn sync_directory(directory: &Path) -> io::Result<()> {
    #[cfg(feature = "sync-probe")]
    crate::probe::record(crate::probe::Event::SyncDir);
    platform::sync_directory(directory)
}

/// Lists every entry under `directory` (files and subdirectories).
///
/// Used by the recovery step to enumerate `tmp/` and by tests.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn read_directory(directory: &Path) -> io::Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        entries.push(entry?.path());
    }
    Ok(entries)
}

#[cfg(unix)]
mod platform {
    use super::*;

    pub(super) fn sync_directory(directory: &Path) -> io::Result<()> {
        File::open(directory)?.sync_all()
    }
}

#[cfg(windows)]
mod platform {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;

    use super::*;

    /// `FILE_FLAG_BACKUP_SEMANTICS`: required to obtain a handle to a directory.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    // The one `unsafe` call in this crate, confined to this function. It exists
    // because Win32 offers no safe directory-flush API at all; see the module
    // documentation for the limits of what it achieves.
    #[allow(unsafe_code)]
    pub(super) fn sync_directory(directory: &Path) -> io::Result<()> {
        // A directory handle needs the backup-semantics flag; access mode is
        // irrelevant to FlushFileBuffers, so a read handle is enough.
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(directory)?;
        // SAFETY: `handle` owns a live Windows handle for the duration of the
        // call; FlushFileBuffers reads only that handle value, takes no buffers
        // and does not retain it after returning. Its result is a status code,
        // handled below.
        let status = unsafe { FlushFileBuffers(handle.as_raw_handle()) };
        let _ = status;
        // Best effort: see the module documentation. A failure here cannot be
        // acted on, and the index commit that follows issues its own flush.
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;

    pub(super) fn sync_directory(_directory: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syncing_a_file_and_its_directory_succeeds() {
        let root = std::env::temp_dir().join(format!(
            "cas-store-sync-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        let path = root.join("file");
        let mut file = File::create(&path).expect("create");
        std::io::Write::write_all(&mut file, b"bytes").expect("write");
        sync_file(&file).expect("file sync");
        drop(file);
        sync_directory(&root).expect("directory sync");
        let entries = read_directory(&root).expect("read dir");
        assert_eq!(entries.len(), 1);
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn rename_replaces_an_existing_target() {
        let root = std::env::temp_dir().join(format!(
            "cas-store-rename-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        let from = root.join("from");
        let to = root.join("to");
        std::fs::write(&from, b"new").expect("write from");
        std::fs::write(&to, b"old").expect("write to");
        rename_replace(&from, &to).expect("rename");
        assert_eq!(std::fs::read(&to).expect("read to"), b"new");
        assert!(!from.exists());
        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
