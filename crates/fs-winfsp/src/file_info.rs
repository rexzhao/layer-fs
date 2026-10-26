//! `GetFileInfo`'s body (P3-4): the adapter's view of an entry, in the SDK's record.
//!
//! Pure Rust for the same reason as `volume`: the interesting mistakes are arithmetic and policy, not FFI.
//!
//! Two of them are worth naming up front:
//!
//! - **`FILETIME` is not Unix time.** The conversion goes through `fs_windows::time` — the single place that knows about
//!   the 1601 epoch, and the place P3-1 fixed an `i64` overflow in. Re-deriving the offset here would be a second copy of
//!   the one constant this project has already got wrong once.
//! - **`AllocationSize` is a policy, not a fact.** The core stores exact byte lengths; NTFS reports allocation rounded up
//!   to a cluster. Reporting the exact size would make every file look "sparse" to tools that compare the two, so the
//!   rounding is explicit here with the cluster size named.

use crate::abi::FileInfo;
use crate::ops::{Attributes, Kind, NodeInfo, Times};
use fs_windows::time;

/// The allocation unit this frontend reports (`AllocationSize` rounding).
///
/// 4096 is the NTFS default on every volume Windows creates since Vista; the store has no cluster concept, so this is a
/// **reported** constant, not a stored one. Named rather than inlined because a mount on a 512-byte-cluster volume would
/// need it changed, and that should be one edit with one place to look.
pub const ALLOCATION_UNIT: u64 = 4096;

/// Rounds a byte length up to the allocation unit.
#[must_use]
pub fn allocation_size(size: u64) -> u64 {
    size.div_ceil(ALLOCATION_UNIT) * ALLOCATION_UNIT
}

/// Builds the record from the adapter's metadata.
///
/// # Errors
///
/// [`fs_windows::error::Failure`] from the time conversion (a timestamp outside the representable `FILETIME` range).
pub fn file_info(info: &NodeInfo) -> Result<FileInfo, fs_windows::error::Failure> {
    Ok(FileInfo {
        // `get_info` already reports **derived** attributes (the adapter calls `derived_attributes`), so this is a
        // pass-through: re-deriving here would be a second place where DIRECTORY/NORMAL are decided.
        file_attributes: info.attributes.0,
        reparse_tag: 0,
        allocation_size: allocation_size(info.size),
        file_size: info.size,
        creation_time: time::unix_ns_to_filetime(info.times.created)?,
        last_access_time: time::unix_ns_to_filetime(info.times.accessed)?,
        last_write_time: time::unix_ns_to_filetime(info.times.written)?,
        change_time: time::unix_ns_to_filetime(info.times.changed)?,
        index_number: info.file_id,
        hard_links: 0,
        ea_size: 0,
    })
}

/// A convenience for the wrapper: the record for a directory entry that the caller has already shaped.
#[must_use]
pub fn directory_size(kind_is_directory: bool) -> u64 {
    if kind_is_directory {
        0
    } else {
        ALLOCATION_UNIT
    }
}

/// The attributes a caller should pass when it has stored bits rather than a `NodeInfo`.
#[must_use]
pub fn attributes_for(kind: Kind, stored: u32) -> Attributes {
    Attributes(fs_semantics::attributes::report(
        kind == Kind::Directory,
        stored,
    ))
}

/// The times a caller should pass when it has four values.
#[must_use]
pub fn times(created: i64, accessed: i64, written: i64, changed: i64) -> Times {
    Times {
        created,
        accessed,
        written,
        changed,
    }
}
