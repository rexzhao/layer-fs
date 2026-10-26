//! Packing directory entries into WinFsp's buffer (`ReadDirectory`'s body, P3-4).
//!
//! WinFsp hands one buffer and expects as many `FSP_FSCTL_DIR_INFO` records as fit. Two rules decide whether the result is
//! readable at all:
//!
//! - **a record is never written partially**: a half record is not "a shorter entry", it is garbage the OS will read as the
//!   name of the next one (the same reason `GetVolumeInfo` refuses a short buffer instead of filling what fits);
//! - **the next record starts at `align8(Size)`**: the padding is part of the layout, not slack.
//!
//! Both are arithmetic, so both are here rather than inside the `extern "C"` wrapper.

use crate::abi::{dir_info, FileInfo};

/// A view over WinFsp's output buffer that only ever appends **whole** records.
pub struct DirBuffer<'a> {
    bytes: &'a mut [u8],
    written: usize,
}

impl<'a> DirBuffer<'a> {
    /// Wraps the caller's buffer.
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, written: 0 }
    }

    /// Bytes of complete records written so far.
    #[must_use]
    pub fn written(&self) -> usize {
        self.written
    }

    /// Appends one entry, if it fits **whole**.
    ///
    /// Returns `false` and writes nothing when the padded record does not fit: the caller then stops and reports what it has,
    /// which is what WinFsp expects.
    pub fn push(&mut self, name: &[u16], info: &FileInfo) -> bool {
        let size = dir_info::record_size(name.len());
        let padded = dir_info::aligned(size);
        if self.written + padded > self.bytes.len() {
            return false;
        }
        let start = self.written;
        let record = &mut self.bytes[start..start + padded];
        // Zero the padding too: WinFsp may copy the whole padded span, and stale bytes there are indistinguishable from data.
        record.fill(0);
        record[dir_info::SIZE_AT..dir_info::SIZE_AT + 2]
            .copy_from_slice(&u16::try_from(size).unwrap_or(u16::MAX).to_ne_bytes());
        let info_bytes = size_of_file_info();
        let value = info_bytes_bytes(info);
        record[dir_info::FILE_INFO_AT..dir_info::FILE_INFO_AT + info_bytes]
            .copy_from_slice(&value[..info_bytes]);
        let name_start = dir_info::NAME_AT;
        for (index, unit) in name.iter().enumerate() {
            let at = name_start + index * 2;
            record[at..at + 2].copy_from_slice(&unit.to_ne_bytes());
        }
        self.written += padded;
        true
    }
}

fn size_of_file_info() -> usize {
    core::mem::size_of::<FileInfo>()
}

/// Serialises a `FileInfo` into its (already asserted) layout.
fn info_bytes_bytes(info: &FileInfo) -> [u8; 72] {
    let mut out = [0u8; 72];
    out[0..4].copy_from_slice(&info.file_attributes.to_ne_bytes());
    out[4..8].copy_from_slice(&info.reparse_tag.to_ne_bytes());
    out[8..16].copy_from_slice(&info.allocation_size.to_ne_bytes());
    out[16..24].copy_from_slice(&info.file_size.to_ne_bytes());
    out[24..32].copy_from_slice(&info.creation_time.to_ne_bytes());
    out[32..40].copy_from_slice(&info.last_access_time.to_ne_bytes());
    out[40..48].copy_from_slice(&info.last_write_time.to_ne_bytes());
    out[48..56].copy_from_slice(&info.change_time.to_ne_bytes());
    out[56..64].copy_from_slice(&info.index_number.to_ne_bytes());
    out[64..68].copy_from_slice(&info.hard_links.to_ne_bytes());
    out[68..72].copy_from_slice(&info.ea_size.to_ne_bytes());
    out
}
