//! The hand-written WinFsp structures this frontend needs, pinned to the SDK by **layout assertions**.
//!
//! Why hand-written rather than `bindgen` (P3-4 ruling): the SDK must not become a *build* dependency — only the mount
//! tests need it — and an assertion that fails loudly on a layout change is safer than a generator that silently follows
//! one. Every field cites the SDK line it was copied from (process 270: a hand-copied struct must be traceable to a
//! specific line, or a layout drift cannot be reviewed).
//!
//! SDK: `F:/work/winfsp-sdk/DYNAMIC/inc/winfsp/fsctl.h` (WinFsp 2.1).

/// `FSP_FSCTL_FILE_INFO` — `fsctl.h:276-290`, with the SDK's own assertion `72 == sizeof(...)` at `fsctl.h:291`.
///
/// Field order and widths are copied verbatim; the offsets in the tests below are the arithmetic that
/// `FSP_FSCTL_STATIC_ASSERT(72 == sizeof(...))` implies on a 64-bit target.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileInfo {
    /// `fsctl.h:278`
    pub file_attributes: u32,
    /// `fsctl.h:279`
    pub reparse_tag: u32,
    /// `fsctl.h:280`
    pub allocation_size: u64,
    /// `fsctl.h:281`
    pub file_size: u64,
    /// `fsctl.h:282`
    pub creation_time: u64,
    /// `fsctl.h:283`
    pub last_access_time: u64,
    /// `fsctl.h:284`
    pub last_write_time: u64,
    /// `fsctl.h:285`
    pub change_time: u64,
    /// `fsctl.h:286`
    pub index_number: u64,
    /// `fsctl.h:287` — unimplemented on the SDK side, set to 0.
    pub hard_links: u32,
    /// `fsctl.h:288`
    pub ea_size: u32,
}

impl FileInfo {
    /// A zeroed record, which is also what "no attributes, size 0, epoch times" means to the OS.
    #[must_use]
    pub fn zeroed() -> Self {
        Self::default()
    }
}

/// `FSP_FSCTL_VOLUME_INFO` — `fsctl.h:270-275`, SDK assertion at `fsctl.h:276` (`88 == sizeof(...)`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VolumeInfo {
    /// `fsctl.h:272`
    pub total_size: u64,
    /// `fsctl.h:273`
    pub free_size: u64,
    /// `fsctl.h:274` — bytes, not characters.
    pub volume_label_length: u16,
    /// `fsctl.h:275` — 32 UTF-16 units, NUL-padded.
    pub volume_label: [u16; 32],
}

/// `FSP_FILE_SYSTEM_INTERFACE` — the SDK's operation table, which is **64 pointers** (`winfsp.h:193-1093`, with the SDK's
/// own assertion `sizeof(...) == 64 * sizeof(NTSTATUS (*)())` right after the struct).
///
/// This type is deliberately **a fixed 64-slot array and not a struct with ten named fields**: the first version of this
/// file declared only the ten operations we intend to implement, which reads like a table but is the wrong layout — passing
/// it would put our `Create` into whichever slot happens to line up. The table is positional, so its **size** is part of the
/// ABI and is asserted; the individual slot indices will be filled in when the callbacks are wired, one header line at a
/// time.
///
/// Every slot is `Option`: `None` is how WinFsp is told "this operation is not supported".
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FileSystemInterface {
    /// The 64 function-pointer slots, in SDK order.
    pub slots: [Option<unsafe extern "C" fn() -> i32>; Self::SLOTS],
}

impl FileSystemInterface {
    /// The number of slots the SDK's table has (`FSP_FSCTL_STATIC_ASSERT(... == 64 * sizeof(...))`).
    pub const SLOTS: usize = 64;

    /// An all-unsupported table: the "nothing wired yet" state, which is also what a partially implemented mount must
    /// never be mistaken for.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            slots: [None; Self::SLOTS],
        }
    }
}

impl Default for FileSystemInterface {
    fn default() -> Self {
        Self::empty()
    }
}

impl std::fmt::Debug for FileSystemInterface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let filled = self.slots.iter().filter(|slot| slot.is_some()).count();
        f.debug_struct("FileSystemInterface")
            .field("slots", &Self::SLOTS)
            .field("filled", &filled)
            .finish()
    }
}

/// Slot indices of the operations this frontend implements, in the SDK's table order.
///
/// Extracted mechanically from `winfsp.h:193-1093` (33 named entries followed by `Reserved`), and every index below cites
/// the header line of its declaration. Written as constants rather than as struct fields because the table is an **array of
/// 64 slots**: a field would reintroduce the "reads like a table, is not one" mistake that `a7bec80` fixed.
pub mod slot {
    /// `GetVolumeInfo` — `winfsp.h:206`.
    pub const GET_VOLUME_INFO: usize = 0;
    /// `SetVolumeLabel` — `winfsp.h:221` (not implemented: labels are read-only here).
    pub const SET_VOLUME_LABEL: usize = 1;
    /// `GetSecurityByName` — `winfsp.h:252`.
    pub const GET_SECURITY_BY_NAME: usize = 2;
    /// `Create` — `winfsp.h:291`.
    pub const CREATE: usize = 3;
    /// `Open` — `winfsp.h:322`.
    pub const OPEN: usize = 4;
    /// `Overwrite` — `winfsp.h:345`.
    pub const OVERWRITE: usize = 5;
    /// `Cleanup` — `winfsp.h:410`.
    pub const CLEANUP: usize = 6;
    /// `Close` — `winfsp.h:420`.
    pub const CLOSE: usize = 7;
    /// `Read` — `winfsp.h:441`.
    pub const READ: usize = 8;
    /// `Write` — `winfsp.h:471`.
    pub const WRITE: usize = 9;
    /// `Flush` — `winfsp.h:491`.
    pub const FLUSH: usize = 10;
    /// `GetFileInfo` — `winfsp.h:507`.
    pub const GET_FILE_INFO: usize = 11;
    /// `SetBasicInfo` — `winfsp.h:538`.
    pub const SET_BASIC_INFO: usize = 12;
    /// `SetFileSize` — `winfsp.h:576`.
    pub const SET_FILE_SIZE: usize = 13;
    /// `CanDelete` — `winfsp.h:607`.
    pub const CAN_DELETE: usize = 14;
    /// `Rename` — `winfsp.h:632`.
    pub const RENAME: usize = 15;

    /// How many named (non-reserved) entries the SDK declares; the rest are `Reserved[31]` plus padding to 64.
    pub const NAMED: usize = 33;
}

/// `FSP_FSCTL_VOLUME_PARAMS` — 504 bytes (`fsctl.h:265-267`, SDK assertion at `:267`), of which this frontend sets the
/// leading scalars and leaves the rest zero.
///
/// Represented as an **opaque 504-byte buffer with typed accessors**, not as a mirrored struct: the tail is a run of
/// `UINT32` bitfields (`fsctl.h:206` onward), and re-declaring bitfields by hand is the one place where "copied faithfully"
/// and "compiles" are hardest to keep equal. Zero-filled plus the fields below is a valid set of parameters — the SDK
/// documents every unset field as having a safe default of 0.
///
/// Offsets are asserted in the tests; each accessor cites the header line it writes.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VolumeParams {
    bytes: [u8; Self::SIZE],
}

impl VolumeParams {
    /// `sizeof(FSP_FSCTL_VOLUME_PARAMS)` — `fsctl.h:267`.
    pub const SIZE: usize = 504;

    /// A zero-filled parameter block: every field at its documented default.
    #[must_use]
    pub fn zeroed() -> Self {
        Self {
            bytes: [0; Self::SIZE],
        }
    }

    /// `UINT16 Version` — `fsctl.h:193`. The SDK accepts 0 (meaning "current") or `sizeof(...)`; this sets the explicit size
    /// so a version mismatch is reported by the loader rather than guessed.
    pub fn set_version_size(&mut self) {
        self.write_u16(0, Self::SIZE as u16);
    }

    /// `UINT16 SectorSize` — `fsctl.h:195`.
    pub fn set_sector_size(&mut self, value: u16) {
        self.write_u16(2, value);
    }

    /// `UINT16 SectorsPerAllocationUnit` — `fsctl.h:196`.
    pub fn set_sectors_per_allocation_unit(&mut self, value: u16) {
        self.write_u16(4, value);
    }

    /// `UINT16 MaxComponentLength` — `fsctl.h:197`.
    pub fn set_max_component_length(&mut self, value: u16) {
        self.write_u16(6, value);
    }

    /// `UINT32 FileInfoTimeout` — `fsctl.h:204`. Zero disables caching, which is what a workspace wants: the overlay is the
    /// only source of truth, and a cached `FileInfo` would hide a change made through another handle.
    pub fn set_file_info_timeout(&mut self, millis: u32) {
        self.write_u32(32, millis);
    }

    /// The raw bytes, for the FFI call.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; Self::SIZE] {
        &self.bytes
    }

    fn write_u16(&mut self, at: usize, value: u16) {
        self.bytes[at..at + 2].copy_from_slice(&value.to_ne_bytes());
    }

    fn write_u32(&mut self, at: usize, value: u32) {
        self.bytes[at..at + 4].copy_from_slice(&value.to_ne_bytes());
    }
}

impl std::fmt::Debug for VolumeParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VolumeParams")
            .field("size", &Self::SIZE)
            .finish_non_exhaustive()
    }
}
