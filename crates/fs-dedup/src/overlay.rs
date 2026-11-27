//! Per-workspace local NTFS sparse-file overlay storage.
//!
//! All foreground writes go directly to sharded local NTFS sparse files so Unity / Git / SVN
//! write performance stays near native NTFS and hot databases (LMDB / SQLite) stay compatible.

use crate::error::Result;
use crate::pack::read_exact_at;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

/// Manages loose overlay files under `<store>/overlay/<ws_id>/<shard>/i<inode_id>.dat`.
#[derive(Clone)]
pub struct OverlayStore {
    root: PathBuf,
}

impl OverlayStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn workspace_dir(&self, ws_id: u64) -> PathBuf {
        self.root.join(ws_id.to_string())
    }

    pub fn file_path(&self, ws_id: u64, inode_id: u64) -> PathBuf {
        let shard = format!("{:02x}", (inode_id & 0xff) as u8);
        self.workspace_dir(ws_id)
            .join(shard)
            .join(format!("i{inode_id}.dat"))
    }

    /// Creates or truncates a sparse overlay file for `(ws_id, inode_id)`.
    pub fn create_empty(&self, ws_id: u64, inode_id: u64) -> Result<File> {
        let path = self.file_path(ws_id, inode_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)?;
        let _ = layerfs_win32_host::set_sparse_file(&file);
        Ok(file)
    }

    /// Opens an existing overlay file for read/write (creating if absent).
    pub fn open_rw(&self, ws_id: u64, inode_id: u64) -> Result<File> {
        let path = self.file_path(ws_id, inode_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let existed = path.exists();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        if !existed {
            let _ = layerfs_win32_host::set_sparse_file(&file);
        }
        Ok(file)
    }

    /// Opens an existing overlay file read-only.
    pub fn open_ro(&self, ws_id: u64, inode_id: u64) -> Result<File> {
        Ok(File::open(self.file_path(ws_id, inode_id))?)
    }

    /// Positional read from an overlay file (returns short slice at EOF).
    pub fn read_at(&self, ws_id: u64, inode_id: u64, offset: u64, len: usize) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let path = self.file_path(ws_id, inode_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(&path)?;
        let file_len = file.metadata()?.len();
        if offset >= file_len {
            return Ok(Vec::new());
        }
        let to_read = (file_len - offset).min(len as u64) as usize;
        let mut buf = vec![0u8; to_read];
        read_exact_at(&file, offset, &mut buf)?;
        Ok(buf)
    }

    /// Positional write to an overlay file, returning the new file length.
    pub fn write_at(
        &self,
        ws_id: u64,
        inode_id: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<u64> {
        #[cfg(windows)]
        use std::os::windows::fs::FileExt;
        #[cfg(unix)]
        use std::os::unix::fs::FileExt;

        let file = self.open_rw(ws_id, inode_id)?;
        let mut pos = offset;
        let mut rem = data;
        while !rem.is_empty() {
            #[cfg(windows)]
            let n = file.seek_write(rem, pos)?;
            #[cfg(unix)]
            let n = file.write_at(rem, pos)?;
            pos += n as u64;
            rem = &rem[n..];
        }
        Ok(file.metadata()?.len())
    }

    /// Resizes an overlay file (truncating or sparse zero-extending).
    pub fn set_len(&self, ws_id: u64, inode_id: u64, new_len: u64) -> Result<()> {
        let file = self.open_rw(ws_id, inode_id)?;
        file.set_len(new_len)?;
        Ok(())
    }

    /// Deletes the loose overlay file for `(ws_id, inode_id)` if present.
    pub fn remove_file(&self, ws_id: u64, inode_id: u64) -> Result<()> {
        let path = self.file_path(ws_id, inode_id);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Deletes all overlay files for a removed workspace.
    pub fn remove_workspace(&self, ws_id: u64) -> Result<()> {
        let dir = self.workspace_dir(ws_id);
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}
