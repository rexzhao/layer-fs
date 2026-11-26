//! Host source observations; symbolic links are stored without traversal.
use crate::{backing::Stamp, host, ProjectError};
use layerfs_content::filesystem::symlink::SymlinkTarget;
use std::{
    fs::{self, Metadata},
    path::Path,
};

pub(super) fn read_link(path: &Path, scanned: &Metadata) -> Result<Vec<u8>, ProjectError> {
    // read_link never follows the target, including dangling, external or cyclic
    // links. A replaced/changed source is refused without a second read/retry.
    let before = Stamp::of_path(path, scanned)?;
    let target = fs::read_link(path)?;
    let after_meta = fs::symlink_metadata(path)?;
    let after = Stamp::of_path(path, &after_meta)?;
    if !after_meta.file_type().is_symlink() || after != before {
        return Err(ProjectError::InvalidInput);
    }
    // Keep the canonical target bound/grammar: no normalization or UTF-8 cast on Unix,
    // UTF-8 path bytes on Windows.
    let target = SymlinkTarget::new(host::os_str_to_bytes(target.as_os_str())?)
        .map_err(ProjectError::Content)?;
    Ok(target.as_bytes().to_vec())
}

/// Refuses a backing whose files are the source or lie inside it.
///
/// The placement is resolved through its links first, then each ancestor's
/// host identity is compared with the source's, so a symlinked or otherwise
/// aliased spelling of a directory inside the source is recognised. Nothing is
/// begun or changed: a backing inside the source would both alter the source
/// while it is read and be acquired as part of it.
pub(super) fn outside_source(
    source_path: &Path,
    source: &Metadata,
    placement: &Path,
) -> Result<(), ProjectError> {
    let (src_dev, src_ino) = host::dev_ino_of_path(source_path, source)?;
    let mut ancestor = fs::canonicalize(placement)?;
    loop {
        let metadata = fs::symlink_metadata(&ancestor)?;
        let (dev, ino) = host::dev_ino_of_path(&ancestor, &metadata)?;
        if dev == src_dev && ino == src_ino {
            return Err(ProjectError::BackingInsideSource);
        }
        if !ancestor.pop() {
            return Ok(());
        }
    }
}
