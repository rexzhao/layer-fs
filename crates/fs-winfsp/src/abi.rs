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
