//! Volume information (P3-1 §1 `volume`).
//!
//! A workspace is a view over a content-addressed store shared by every branch: there is no such thing as
//! "this workspace's free bytes". The frontend therefore reports **policy constants**, and the point of this
//! module is that those constants have **one source** — two places with their own magic number will drift, and
//! then Explorer and `dir` will disagree about the same share.

/// Total bytes reported for the volume.
pub const TOTAL_BYTES: u64 = 1 << 40;

/// Free bytes reported for the volume.
///
/// Kept equal to [`TOTAL_BYTES`] on purpose: the store grows with what is published, so "free" is not a
/// quantity this layer can know, and a fixed fraction would be a lie with a decimal point.
pub const FREE_BYTES: u64 = TOTAL_BYTES;

/// The blocks the volume reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeInfo {
    /// Total bytes.
    pub total_bytes: u64,
    /// Free bytes.
    pub free_bytes: u64,
    /// Sectors per allocation unit (WinFsp requires it; 1 keeps the arithmetic exact).
    pub sectors_per_unit: u32,
    /// Bytes per sector.
    pub bytes_per_sector: u32,
}

/// The one place the frontend's volume numbers come from.
#[must_use]
pub const fn info() -> VolumeInfo {
    VolumeInfo {
        total_bytes: TOTAL_BYTES,
        free_bytes: FREE_BYTES,
        sectors_per_unit: 1,
        bytes_per_sector: 4096,
    }
}
