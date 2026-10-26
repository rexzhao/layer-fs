//! `GetVolumeInfo` — the first callback's **body**, before any `extern "C"` exists (P3-4).
//!
//! Pure Rust on purpose: the same reason `boundary` exists. A volume answer is arithmetic plus one encoding rule, and both
//! are cheaper to get right here than through a mounted drive. The `extern "C"` wrapper will only add `guarded`, the
//! conversion and the status mapping.

use crate::abi::VolumeInfo;
use fs_windows::error::NtStatus;

/// The widest label WinFsp's field can carry, in UTF-16 units, **leaving room for a terminator**.
///
/// `VolumeLabel` is `WCHAR[32]` (`fsctl.h:275`): 32 units total, so a NUL-terminated label holds at most 31.
pub const MAX_LABEL_UNITS: usize = 31;

/// Fills the volume answer.
///
/// The label length is reported in **bytes** (the SDK's `VolumeLabelLength` is a byte count, `fsctl.h:274`), while the
/// buffer is UTF-16 units — the classic off-by-two of this call, which is why the conversion lives here rather than in the
/// wrapper.
///
/// A label longer than [`MAX_LABEL_UNITS`] is **truncated**, not refused: WinFsp will display what fits, and refusing would
/// make a mounted volume invisible over a cosmetic field.
///
/// # Errors
///
/// [`NtStatus::ObjectNameInvalid`] if the label contains an interior NUL — the receiving side would end the string there
/// and silently drop the rest.
pub fn volume_info(label: &str, total_size: u64, free_size: u64) -> Result<VolumeInfo, NtStatus> {
    if label.contains('\0') {
        return Err(NtStatus::ObjectNameInvalid);
    }
    let mut units: Vec<u16> = label.encode_utf16().take(MAX_LABEL_UNITS).collect();
    let mut buffer = [0u16; 32];
    buffer[..units.len()].copy_from_slice(&units);
    units.clear();
    Ok(VolumeInfo {
        total_size,
        free_size,
        // Bytes, not units: 2 per UTF-16 unit.
        volume_label_length: u16::try_from(
            buffer
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(MAX_LABEL_UNITS)
                * 2,
        )
        .unwrap_or(0),
        volume_label: buffer,
    })
}
