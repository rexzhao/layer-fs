//! Minimal Windows host file-identity and volume-capacity helpers so upstream
//! `layerfs-overlay` and `layerfs-store-service` keep `#![forbid(unsafe_code)]`.

use std::{fs::File, io, path::Path};

/// Physical identity and size observations of a host file on Windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    /// Volume serial number (Windows counterpart to Unix `st_dev`).
    pub device: u64,
    /// 64-bit file index on that volume (Windows counterpart to Unix `st_ino`).
    pub inode: u64,
    /// Hard-link count (`nNumberOfLinks`).
    pub links: u64,
    /// Logical byte length (`EndOfFile`).
    pub logical_bytes: u64,
    /// On-disk allocated bytes (`AllocationSize`).
    pub allocated_bytes: u64,
    /// True when the entry is a regular file (neither a directory nor a reparse point).
    pub is_regular_file: bool,
}

#[cfg(windows)]
mod win32 {
    use super::FileIdentity;
    use std::{
        ffi::c_void,
        fs::File,
        io,
        os::windows::{ffi::OsStrExt, io::AsRawHandle},
        path::Path,
    };

    const INVALID_HANDLE_VALUE: isize = -1;
    const FILE_SHARE_ALL: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_STANDARD_INFO_CLASS: u32 = 1;

    #[repr(C)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time: [u32; 2],
        last_access_time: [u32; 2],
        last_write_time: [u32; 2],
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    #[repr(C)]
    struct FileStandardInfo {
        allocation_size: i64,
        end_of_file: i64,
        number_of_links: u32,
        delete_pending: u8,
        directory: u8,
        _pad: [u8; 2],
    }

    #[repr(C)]
    struct FileZeroDataInformation {
        file_offset: i64,
        beyond_final_zero: i64,
    }

    const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
    const FSCTL_SET_ZERO_DATA: u32 = 0x0009_80C8;

    #[link(name = "kernel32")]
    extern "system" {
        fn DeviceIoControl(
            device: *mut c_void,
            io_control_code: u32,
            in_buffer: *const c_void,
            in_buffer_size: u32,
            out_buffer: *mut c_void,
            out_buffer_size: u32,
            bytes_returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *const c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: *mut c_void,
        ) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetFileInformationByHandle(
            file: *mut c_void,
            info: *mut ByHandleFileInformation,
        ) -> i32;
        fn GetFileInformationByHandleEx(
            file: *mut c_void,
            info_class: u32,
            info: *mut c_void,
            buffer_size: u32,
        ) -> i32;
        fn GetDiskFreeSpaceExW(
            directory_name: *const u16,
            free_bytes_available: *mut u64,
            total_bytes: *mut u64,
            total_free_bytes: *mut u64,
        ) -> i32;
        fn GetVolumePathNameW(
            file_name: *const u16,
            volume_path_name: *mut u16,
            buffer_length: u32,
        ) -> i32;
        fn GetDriveTypeW(root_path_name: *const u16) -> u32;
        fn GetVolumeInformationW(
            root_path_name: *const u16,
            volume_name_buffer: *mut u16,
            volume_name_size: u32,
            volume_serial_number: *mut u32,
            maximum_component_length: *mut u32,
            file_system_flags: *mut u32,
            file_system_name_buffer: *mut u16,
            file_system_name_size: u32,
        ) -> i32;
    }

    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_RAMDISK: u32 = 6;

    fn identity_from_handle(handle: *mut c_void) -> io::Result<FileIdentity> {
        let mut info = ByHandleFileInformation {
            file_attributes: 0,
            creation_time: [0; 2],
            last_access_time: [0; 2],
            last_write_time: [0; 2],
            volume_serial_number: 0,
            file_size_high: 0,
            file_size_low: 0,
            number_of_links: 0,
            file_index_high: 0,
            file_index_low: 0,
        };
        // SAFETY: `handle` is a valid OS file handle and `info` is writable.
        if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let logical_bytes = (u64::from(info.file_size_high) << 32) | u64::from(info.file_size_low);
        let mut standard = FileStandardInfo {
            allocation_size: 0,
            end_of_file: 0,
            number_of_links: 0,
            delete_pending: 0,
            directory: 0,
            _pad: [0; 2],
        };
        // SAFETY: `handle` is valid and `standard` matches `FILE_STANDARD_INFO`.
        let allocated_bytes = if unsafe {
            GetFileInformationByHandleEx(
                handle,
                FILE_STANDARD_INFO_CLASS,
                (&mut standard as *mut FileStandardInfo).cast::<c_void>(),
                std::mem::size_of::<FileStandardInfo>() as u32,
            )
        } != 0
            && standard.allocation_size >= 0
        {
            standard.allocation_size as u64
        } else {
            logical_bytes
        };
        let is_regular_file = (info.file_attributes & FILE_ATTRIBUTE_DIRECTORY) == 0
            && (info.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT) == 0;
        Ok(FileIdentity {
            device: u64::from(info.volume_serial_number),
            inode: (u64::from(info.file_index_high) << 32) | u64::from(info.file_index_low),
            links: u64::from(info.number_of_links),
            logical_bytes,
            allocated_bytes,
            is_regular_file,
        })
    }

    pub(super) fn file_identity(file: &File) -> io::Result<FileIdentity> {
        identity_from_handle(file.as_raw_handle().cast::<c_void>())
    }

    pub(super) fn path_identity(path: &Path) -> io::Result<FileIdentity> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: NUL-terminated UTF-16 path; access 0 queries metadata without
        // contending with open readers/writers, and OPEN_REPARSE_POINT avoids
        // following symlinks.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0,
                FILE_SHARE_ALL,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle as isize == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let result = identity_from_handle(handle);
        // SAFETY: `handle` was just opened by `CreateFileW`.
        unsafe { CloseHandle(handle) };
        result
    }

    pub(super) fn volume_available_bytes(path: &Path) -> io::Result<u64> {
        let dir = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => path,
        };
        let wide: Vec<u16> = dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut free_available: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;
        // SAFETY: `wide` is NUL-terminated and all out-pointers are valid locals.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_available,
                &mut total,
                &mut total_free,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(free_available)
    }

    pub(super) fn local_filesystem_kind(path: &Path) -> io::Result<Option<&'static str>> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut root = vec![0u16; wide.len().max(260) + 16];
        // SAFETY: `wide` is NUL-terminated and `root` has `root.len()` UTF-16 elements.
        let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `GetVolumePathNameW` NUL-terminated `root`.
        let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };
        if !matches!(drive_type, DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_RAMDISK) {
            return Ok(None);
        }
        let mut fs_name = [0u16; 64];
        // SAFETY: `root` is NUL-terminated and `fs_name` has 64 UTF-16 slots.
        let got = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
            )
        };
        if got == 0 {
            return Err(io::Error::last_os_error());
        }
        let len = fs_name
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(fs_name.len());
        let name = String::from_utf16_lossy(&fs_name[..len]);
        Ok(match name.as_str() {
            "NTFS" => Some("ntfs"),
            "ReFS" => Some("refs"),
            _ => None,
        })
    }

    pub(super) fn set_sparse_file(file: &File) -> io::Result<()> {
        let mut returned: u32 = 0;
        // SAFETY: `file` owns a valid writable Win32 file handle; `FSCTL_SET_SPARSE`
        // takes no input/output buffers when setting the sparse attribute.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle().cast::<c_void>(),
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn punch_hole(file: &File, offset: u64, length: u64) -> io::Result<()> {
        if length == 0 {
            return Ok(());
        }
        let end = offset.saturating_add(length);
        let Ok(file_offset) = i64::try_from(offset) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "offset overflow"));
        };
        let Ok(beyond_final_zero) = i64::try_from(end) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "range overflow"));
        };
        let range = FileZeroDataInformation {
            file_offset,
            beyond_final_zero,
        };
        let mut returned: u32 = 0;
        // SAFETY: `file` owns a valid writable handle and `range` matches `FILE_ZERO_DATA_INFORMATION`.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle().cast::<c_void>(),
                FSCTL_SET_ZERO_DATA,
                (&range as *const FileZeroDataInformation).cast::<c_void>(),
                std::mem::size_of::<FileZeroDataInformation>() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Queries physical identity and allocation size for an already-open file handle.
pub fn file_identity(file: &File) -> io::Result<FileIdentity> {
    #[cfg(windows)]
    {
        win32::file_identity(file)
    }
    #[cfg(not(windows))]
    {
        let _ = file;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "layerfs-win32-host is Windows-only",
        ))
    }
}

/// Queries physical identity for `path` without following symlinks/reparse points.
pub fn path_identity(path: &Path) -> io::Result<FileIdentity> {
    #[cfg(windows)]
    {
        win32::path_identity(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "layerfs-win32-host is Windows-only",
        ))
    }
}

/// Returns free bytes available to the caller on the volume holding `path`.
pub fn volume_available_bytes(path: &Path) -> io::Result<u64> {
    #[cfg(windows)]
    {
        win32::volume_available_bytes(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "layerfs-win32-host is Windows-only",
        ))
    }
}

/// Returns `"ntfs"` or `"refs"` when `path` resides on a local Windows volume
/// supporting SQLite WAL locks; returns `Ok(None)` for network/unsupported volumes.
pub fn local_filesystem_kind(path: &Path) -> io::Result<Option<&'static str>> {
    #[cfg(windows)]
    {
        win32::local_filesystem_kind(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "layerfs-win32-host is Windows-only",
        ))
    }
}

/// Marks `file` as a Windows NTFS/ReFS sparse file (`FSCTL_SET_SPARSE`) so arbitrary
/// large offsets allocate zero disk blocks for unwritten holes.
pub fn set_sparse_file(file: &File) -> io::Result<()> {
    #[cfg(windows)]
    {
        win32::set_sparse_file(file)
    }
    #[cfg(not(windows))]
    {
        let _ = file;
        Ok(())
    }
}

/// Releases disk clusters for `[offset, offset + length)` inside a sparse file
/// (`FSCTL_SET_ZERO_DATA`).
pub fn punch_hole(file: &File, offset: u64, length: u64) -> io::Result<()> {
    #[cfg(windows)]
    {
        win32::punch_hole(file, offset, length)
    }
    #[cfg(not(windows))]
    {
        let _ = (file, offset, length);
        Ok(())
    }
}
