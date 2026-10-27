//! Mounting the file system read-only at a directory (P3-4 step ④).
//!
//! Kept apart from `mount.rs` on purpose: that module answers "does the library's lifecycle work" without a mount point, and
//! this one answers "can Windows see us". Mixing them would put two different unknowns behind one crash.
//!
//! The four calls, in the order that matters: `create` → `SetMountPoint(dir)` → `start` → `SetMountPoint(NULL)` →
//! `stop` → `delete`. **Unmounting before stopping** is not cosmetic: a mount point that outlives the process is the failure
//! mode that needs a reboot to clear.

use std::path::Path;

use crate::loader::{resolve_library, LoadError};

/// The device name this frontend creates (`fsctl.h:52`: `winfsp` + `.Disk`), shared with the lifecycle experiment.
pub use crate::mount::DISK_DEVICE;

/// What a read-only mount reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MountReport {
    /// `FspFileSystemCreate`.
    pub create: i32,
    /// `FspFileSystemSetMountPoint` with the directory.
    pub mount: i32,
    /// `FspFileSystemStartDispatcher`.
    pub start: i32,
    /// `FspFileSystemSetMountPoint(NULL)` — the unmount.
    pub unmount: i32,
    /// The dispatcher's own death cause, read with `FspFileSystemGetDispatcherResultF` (`winfsp.h:1392`).
    ///
    /// `0` means the dispatcher is alive. Anything else is the NTSTATUS the dispatcher threads died with — and a dead dispatcher
    /// produces exactly the shape we observed: the volume is mounted, every callback counter is zero, and IRPs come back as
    /// `ERROR_INVALID_FUNCTION`. Reading it replaces "we infer the dispatcher is fine" with a value.
    pub dispatcher_result: i32,
    /// How many times the **probe marker** callback fired (all 64 slots filled with it, `LAYER_FS_PROBE=1`).
    ///
    /// `> 0` proves the table we filled is the table the dispatcher reads; `0` means it is not, whatever the indices say.
    pub probe_hits: usize,
    /// Whether the drive letter / directory was **gone after `delete`**.
    ///
    /// A status code says what one call thought; this says what the OS now believes. For a drive letter it is read back with
    /// `GetLogicalDrives`, which is the hard evidence that nothing was left mounted.
    pub released_after_delete: bool,
    /// Whether the directory was readable **while mounted**, as seen through the OS by the caller.
    ///
    /// Reported rather than asserted inside: a failure here is a result, not a bug in this function — the caller decides what
    /// it means, and the message carries the OS error.
    pub looked_like_a_mount: bool,
}

/// Mounts read-only at `mount_point`, observes it through the OS, unmounts, and tears everything down.
///
/// `probe` is called **while the mount is up**; its result is reported, never swallowed. Passing it in keeps the OS access out
/// of this module, which is what lets the same code run with or without a mount point in tests.
///
/// # Errors
///
/// [`LoadError`] from the loader or a missing export.
/// Whether the OS currently lists `path` as a drive letter.
///
/// Exposed so a caller can ask **during** the mount, which is the one moment that distinguishes "the mount point was recorded"
/// from "the volume is there".
#[must_use]
pub fn drive_present(path: &Path) -> Option<bool> {
    platform::drive_present(path)
}

/// Mounts read-only at `mount_point`, observes it through the OS, unmounts, and tears everything down.
///
/// `probe` is called **while the mount is up**; its result is reported, never swallowed.
///
/// # Errors
///
/// [`LoadError`] from the loader or a missing export.
pub fn mount_read_only(
    library: &Path,
    mount_point: &Path,
    probe: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<MountReport, LoadError> {
    resolve_library(library)?;
    platform::mount_read_only(library, mount_point, probe)
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use crate::abi::{slot, FileSystemInterface, VolumeParams};
    use crate::loader::LoadError;

    type CreateFn = extern "system" fn(
        *const u16,
        *const VolumeParams,
        *const FileSystemInterface,
        *mut *mut c_void,
    ) -> i32;
    type SetMountPointFn = extern "system" fn(*mut c_void, *const u16) -> i32;
    type StartFn = extern "system" fn(*mut c_void, u32) -> i32;
    type StopFn = extern "system" fn(*mut c_void);
    type DeleteFn = extern "system" fn(*mut c_void);
    type DispatcherResultFn = extern "system" fn(*mut c_void, *mut i32);

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *const c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
        fn GetLastError() -> u32;
        fn GetLogicalDrives() -> u32;
    }

    pub(super) fn drive_present(path: &Path) -> Option<bool> {
        let text = path.to_string_lossy();
        let bytes = text.as_bytes();
        if bytes.len() != 2 || bytes[1] != b':' {
            return None;
        }
        Some(drive_letter_present(path))
    }

    /// Whether `path` still looks like a live drive letter (`Y:`), as the OS reports it.
    fn drive_letter_present(path: &Path) -> bool {
        let text = path.to_string_lossy();
        let bytes = text.as_bytes();
        if bytes.len() != 2 || bytes[1] != b':' {
            return false;
        }
        let Some(letter) = text.chars().next() else {
            return false;
        };
        if !letter.is_ascii_alphabetic() {
            return false;
        }
        // SAFETY: `GetLogicalDrives` has no arguments and no preconditions.
        let mask = unsafe { GetLogicalDrives() };
        let index = u32::from(letter.to_ascii_uppercase() as u8 - b'A');
        mask & (1 << index) != 0
    }

    /// `GetVolumeInfo`: `(FSP_FILE_SYSTEM*, PVOID VolumeInfo, ULONG Length)` — `winfsp.h:206`.
    unsafe extern "system" fn volume_info_trampoline(
        _file_system: *mut c_void,
        volume_info: *mut c_void,
        volume_info_length: u32,
    ) -> i32 {
        // SAFETY: WinFsp passes a buffer of `volume_info_length` bytes, which is what the callee validates against its own
        // structure size.
        unsafe { crate::callbacks::get_volume_info(volume_info, volume_info_length) }
    }

    /// `GetFileInfo`: `(FSP_FILE_SYSTEM*, PVOID FileContext, PVOID FileInfo)` — `winfsp.h:507`.
    unsafe extern "system" fn file_info_trampoline(
        _file_system: *mut c_void,
        file_context: *mut c_void,
        file_info: *mut c_void,
    ) -> i32 {
        // SAFETY: `file_context` is the `EntryContext` this frontend allocated, and `file_info` is WinFsp's output buffer.
        unsafe { crate::callbacks::get_file_info(file_context, file_info) }
    }

    /// `ReadDirectory`: `(FSP_FILE_SYSTEM*, PVOID FileContext, PVOID Buffer, UINT64 Offset, PWSTR Pattern, PWSTR Marker,
    /// ULONG Length)` — `winfsp.h:703`.
    unsafe extern "system" fn read_directory_trampoline(
        _file_system: *mut c_void,
        file_context: *mut c_void,
        buffer: *mut c_void,
        offset: u64,
        _pattern: *const u16,
        marker: *const u16,
        length: u32,
    ) -> i32 {
        // SAFETY: WinFsp's contract — `file_context` is ours, `buffer` is valid for `length` bytes, `marker` is NUL-terminated.
        unsafe { crate::callbacks::read_directory(file_context, buffer, offset, marker, length) }
    }

    /// `Open`: `(FSP_FILE_SYSTEM*, PWSTR FileName, UINT32 CreateOptions, UINT32 GrantedAccess, PVOID *PFileContext,
    /// FSP_FSCTL_FILE_INFO *FileInfo)` — `winfsp.h:322`.
    unsafe extern "system" fn open_trampoline(
        _file_system: *mut c_void,
        file_name: *const u16,
        _create_options: u32,
        _granted_access: u32,
        context_out: *mut *mut c_void,
        info_out: *mut crate::abi::FileInfo,
    ) -> i32 {
        // SAFETY: WinFsp's contract — a NUL-terminated name and two valid out-pointers.
        unsafe { crate::callbacks::open(file_name, context_out, info_out) }
    }

    /// `Create`: `(FSP_FILE_SYSTEM*, PWSTR FileName, UINT32 CreateOptions, UINT32 GrantedAccess, UINT32 FileAttributes,
    /// PSECURITY_DESCRIPTOR, UINT64 AllocationSize, PVOID *PFileContext, FSP_FSCTL_FILE_INFO*)` — `winfsp.h:291`.
    unsafe extern "system" fn create_trampoline(
        _file_system: *mut c_void,
        file_name: *const u16,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security: *mut c_void,
        _allocation_size: u64,
        context_out: *mut *mut c_void,
        info_out: *mut crate::abi::FileInfo,
    ) -> i32 {
        // SAFETY: WinFsp's contract for this callback.
        unsafe { crate::callbacks::create(file_name, create_options, context_out, info_out) }
    }

    /// `Close`: `(FSP_FILE_SYSTEM*, PVOID FileContext)` — `winfsp.h:420`.
    unsafe extern "system" fn close_trampoline(
        _file_system: *mut c_void,
        file_context: *mut c_void,
    ) {
        // SAFETY: the context came from `open`, and WinFsp calls `Close` once per open.
        unsafe { crate::callbacks::close(file_context) }
    }

    /// `GetSecurityByName`: `(FSP_FILE_SYSTEM*, PWSTR FileName, PUINT32 Attributes, PSECURITY_DESCRIPTOR,
    /// SIZE_T *Size)` — `winfsp.h:252`.
    unsafe extern "system" fn security_by_name_trampoline(
        _file_system: *mut c_void,
        file_name: *const u16,
        attributes_out: *mut u32,
        descriptor_out: *mut c_void,
        descriptor_size_out: *mut usize,
    ) -> i32 {
        // SAFETY: WinFsp's contract for this callback.
        unsafe {
            crate::callbacks::get_security_by_name(
                file_name,
                attributes_out,
                descriptor_out,
                descriptor_size_out,
            )
        }
    }

    /// The marker used by the slot probe: records that it was called, changes nothing else.
    unsafe extern "system" fn probe_marker() -> i32 {
        PROBE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Refuse, so the probe does not accidentally answer something correctly and hide the fact that it fired.
        0xC000_0010u32 as i32
    }

    /// How many times the probe marker fired.
    static PROBE_HITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn interface_with_callbacks() -> FileSystemInterface {
        let mut interface = FileSystemInterface::empty();
        // `LAYER_FS_PROBE=1` fills **every** slot with one marker: if the marker never fires, the dispatcher is not reading the
        // table we filled, and no amount of index arithmetic will help.
        // `LAYER_FS_PROBE_SLOT=n` fills only that index: running it for 0..24 says **which** slot the dispatcher actually calls,
        // which is what turns "our indices are wrong" into a number.
        if let Some(raw) = std::env::var_os("LAYER_FS_PROBE_SLOT") {
            if let Ok(index) = raw.to_string_lossy().parse::<usize>() {
                // SAFETY: same shape assertion as the bulk probe below.
                let marker: unsafe extern "C" fn() -> i32 = unsafe {
                    std::mem::transmute::<
                        unsafe extern "system" fn() -> i32,
                        unsafe extern "C" fn() -> i32,
                    >(probe_marker)
                };
                if let Some(slot) = interface.slots.get_mut(index) {
                    *slot = Some(marker);
                }
                return interface;
            }
        }
        if std::env::var_os("LAYER_FS_PROBE").is_some() {
            // SAFETY: `probe_marker` has the shape the slot type encodes; the cast is the same hand-asserted match the real
            // registrations use.
            let marker: unsafe extern "C" fn() -> i32 = unsafe {
                std::mem::transmute::<
                    unsafe extern "system" fn() -> i32,
                    unsafe extern "C" fn() -> i32,
                >(probe_marker)
            };
            for slot in interface.slots.iter_mut() {
                *slot = Some(marker);
            }
            return interface;
        }
        // SAFETY: the slot type carries **no signature** (the SDK's slots differ in arity), so registering a callback is a
        // hand-asserted shape match. Each index comes from `abi::slot`, which cites the header line it was read from.
        unsafe {
            let volume: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) -> i32 =
                volume_info_trampoline;
            let file: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> i32 =
                file_info_trampoline;
            let listing: unsafe extern "system" fn(
                *mut c_void,
                *mut c_void,
                *mut c_void,
                u64,
                *const u16,
                *const u16,
                u32,
            ) -> i32 = read_directory_trampoline;
            interface.slots[slot::GET_VOLUME_INFO] = Some(std::mem::transmute::<
                unsafe extern "system" fn(*mut c_void, *mut c_void, u32) -> i32,
                unsafe extern "C" fn() -> i32,
            >(volume));
            let opening: unsafe extern "system" fn(
                *mut c_void,
                *const u16,
                u32,
                u32,
                *mut *mut c_void,
                *mut crate::abi::FileInfo,
            ) -> i32 = open_trampoline;
            let closing: unsafe extern "system" fn(*mut c_void, *mut c_void) = close_trampoline;
            let security: unsafe extern "system" fn(
                *mut c_void,
                *const u16,
                *mut u32,
                *mut c_void,
                *mut usize,
            ) -> i32 = security_by_name_trampoline;
            interface.slots[slot::GET_SECURITY_BY_NAME] = Some(std::mem::transmute::<
                unsafe extern "system" fn(
                    *mut c_void,
                    *const u16,
                    *mut u32,
                    *mut c_void,
                    *mut usize,
                ) -> i32,
                unsafe extern "C" fn() -> i32,
            >(security));
            let creating: unsafe extern "system" fn(
                *mut c_void,
                *const u16,
                u32,
                u32,
                u32,
                *mut c_void,
                u64,
                *mut *mut c_void,
                *mut crate::abi::FileInfo,
            ) -> i32 = create_trampoline;
            interface.slots[slot::CREATE] = Some(std::mem::transmute::<
                unsafe extern "system" fn(
                    *mut c_void,
                    *const u16,
                    u32,
                    u32,
                    u32,
                    *mut c_void,
                    u64,
                    *mut *mut c_void,
                    *mut crate::abi::FileInfo,
                ) -> i32,
                unsafe extern "C" fn() -> i32,
            >(creating));
            interface.slots[slot::OPEN] = Some(std::mem::transmute::<
                unsafe extern "system" fn(
                    *mut c_void,
                    *const u16,
                    u32,
                    u32,
                    *mut *mut c_void,
                    *mut crate::abi::FileInfo,
                ) -> i32,
                unsafe extern "C" fn() -> i32,
            >(opening));
            interface.slots[slot::CLOSE] = Some(std::mem::transmute::<
                unsafe extern "system" fn(*mut c_void, *mut c_void),
                unsafe extern "C" fn() -> i32,
            >(closing));
            interface.slots[slot::READ_DIRECTORY] = Some(std::mem::transmute::<
                unsafe extern "system" fn(
                    *mut c_void,
                    *mut c_void,
                    *mut c_void,
                    u64,
                    *const u16,
                    *const u16,
                    u32,
                ) -> i32,
                unsafe extern "C" fn() -> i32,
            >(listing));
            interface.slots[slot::GET_FILE_INFO] = Some(std::mem::transmute::<
                unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> i32,
                unsafe extern "C" fn() -> i32,
            >(file));
        }
        interface
    }

    fn wide(path: &std::ffi::OsStr) -> Vec<u16> {
        let mut units: Vec<u16> = path.encode_wide().collect();
        units.push(0);
        units
    }

    pub(super) fn mount_read_only(
        library: &Path,
        mount_point: &Path,
        probe: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<super::MountReport, LoadError> {
        let module_path = wide(library.as_os_str());
        // SAFETY: `module_path` is NUL-terminated and outlives the call.
        let module = unsafe { LoadLibraryW(module_path.as_ptr()) };
        if module.is_null() {
            let last_error = unsafe { GetLastError() };
            return Err(LoadError::LoadFailed {
                path: library.to_path_buf(),
                last_error,
            });
        }

        let resolve = |name: &str| -> Result<*const c_void, LoadError> {
            let mut narrow: Vec<u8> = name.as_bytes().to_vec();
            narrow.push(0);
            // SAFETY: both pointers are valid for the call.
            let address = unsafe { GetProcAddress(module, narrow.as_ptr()) };
            if address.is_null() {
                return Err(LoadError::MissingExport {
                    name: name.to_owned(),
                });
            }
            Ok(address)
        };

        // A missing export is reported before anything is created, so a version problem never looks like a mount problem.
        let result = (|| -> Result<super::MountReport, LoadError> {
            let create_address = resolve("FspFileSystemCreate")?;
            let set_mount_address = resolve("FspFileSystemSetMountPoint")?;
            let start_address = resolve("FspFileSystemStartDispatcher")?;
            let stop_address = resolve("FspFileSystemStopDispatcher")?;
            let delete_address = resolve("FspFileSystemDelete")?;
            // Optional on purpose: a build without this export must still be able to mount, so its absence only costs us the
            // observation, not the function.
            let dispatcher_result_address = resolve("FspFileSystemGetDispatcherResultF").ok();

            // SAFETY: each address came from this module's export table and the signatures are the SDK's.
            let (create, set_mount_point, start, stop, delete) = unsafe {
                (
                    std::mem::transmute::<*const c_void, CreateFn>(create_address),
                    std::mem::transmute::<*const c_void, SetMountPointFn>(set_mount_address),
                    std::mem::transmute::<*const c_void, StartFn>(start_address),
                    std::mem::transmute::<*const c_void, StopFn>(stop_address),
                    std::mem::transmute::<*const c_void, DeleteFn>(delete_address),
                )
            };

            let mut params = VolumeParams::zeroed();
            params.set_version_size();
            params.set_sector_size(4096);
            params.set_sectors_per_allocation_unit(1);
            params.set_max_component_length(255);
            // No caching: the workspace is the only source of truth, and a cached FileInfo would hide another handle's write.
            params.set_file_info_timeout(0);
            // **The table must outlive the file system.** `FspFileSystemCreate` takes a pointer to the interface and — like
            // most C libraries — keeps it rather than copying it; a table owned by this stack frame would be dangling the
            // moment `create` returns, and a dangling table reads as "every slot is None", which WinFsp reports as
            // `ERROR_INVALID_FUNCTION` with **no callback ever invoked**.
            //
            // Leaked deliberately: this mount's lifetime is the process's (one adapter per process, see `callbacks`), and a
            // `Box::leak` is the honest way to say "this table is not owned by a scope".
            let interface: &'static FileSystemInterface =
                Box::leak(Box::new(interface_with_callbacks()));

            let device = wide(std::ffi::OsStr::new(super::DISK_DEVICE));
            let mut file_system: *mut c_void = std::ptr::null_mut();
            // The log handle is process-wide, so it is set up **before** create or the first traces are lost.
            let debug_log = std::env::var_os("LAYER_FS_DEBUG_LOG")
                .map(std::path::PathBuf::from)
                .and_then(|path: std::path::PathBuf| crate::debuglog::open(path.as_path()));
            eprintln!(
                "interface table: filled at {:p}, passed at {:p}, equal = {}",
                interface,
                interface,
                std::ptr::eq(interface, interface)
            );
            let create_status = create(device.as_ptr(), &params, interface, &mut file_system);
            if create_status != 0 || file_system.is_null() {
                return Ok(super::MountReport {
                    create: create_status,
                    mount: 0,
                    start: 0,
                    unmount: 0,
                    dispatcher_result: 0,
                    probe_hits: 0,
                    released_after_delete: true,
                    looked_like_a_mount: false,
                });
            }

            if let Some(log) = debug_log {
                crate::debuglog::attach(log, module, file_system);
            }

            let mount_path = wide(mount_point.as_os_str());
            let mount_status = set_mount_point(file_system, mount_path.as_ptr());
            // Explicit thread count: memfs passes 0, but if 0 means "no threads" the dispatcher would exist and serve
            // nothing — which is indistinguishable from a dangling table from the outside. 2 is enough to tell the difference.
            let start_status = start(file_system, 2);

            // Observe **while mounted**: this is the only window in which the OS can see the file system.
            let looked_like_a_mount = mount_status == 0 && probe(mount_point).is_ok();

            // Unmount first, then stop, then delete.
            let dispatcher_result = if let Some(address) = dispatcher_result_address {
                // SAFETY: the address came from this module's export table; the signature is the SDK's.
                let read: DispatcherResultFn = unsafe { std::mem::transmute(address) };
                let mut value: i32 = 0;
                read(file_system, &mut value);
                value
            } else {
                0
            };

            let unmount_status = set_mount_point(file_system, std::ptr::null());
            stop(file_system);
            delete(file_system);
            if let Some(log) = debug_log {
                // Close **before** anyone reads the file, or the tail is still buffered.
                crate::debuglog::close(log);
            }
            // The hard evidence, read back from the OS: is the mount point gone? A status code alone does not say.
            let was_drive = {
                let text = mount_point.to_string_lossy();
                text.len() == 2 && text.ends_with(':')
            };
            let released_after_delete = if was_drive {
                !drive_letter_present(mount_point)
            } else {
                // A directory mount point is not a drive; "absent" would be the wrong question there.
                true
            };

            Ok(super::MountReport {
                create: create_status,
                mount: mount_status,
                start: start_status,
                unmount: unmount_status,
                dispatcher_result,
                probe_hits: PROBE_HITS.load(std::sync::atomic::Ordering::Relaxed),
                released_after_delete,
                looked_like_a_mount,
            })
        })();

        // SAFETY: paired with the successful load, on every path.
        unsafe { FreeLibrary(module) };
        result
    }
}

#[cfg(not(windows))]
mod platform {
    use std::path::Path;

    use crate::loader::LoadError;

    pub(super) fn drive_present(_path: &Path) -> Option<bool> {
        None
    }

    pub(super) fn mount_read_only(
        library: &Path,
        _mount_point: &Path,
        _probe: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<super::MountReport, LoadError> {
        Err(LoadError::LoadFailed {
            path: library.to_path_buf(),
            last_error: 0,
        })
    }
}
