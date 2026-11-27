//! True parallel positional file I/O using Windows `FILE_FLAG_OVERLAPPED` handles.
//!
//! On Windows, a standard `std::fs::File` opens a **synchronous** `FILE_OBJECT`, causing the
//! Windows I/O Manager to serialize concurrent `seek_read`/`seek_write` calls on the same
//! `HANDLE` inside the kernel (even across 16 WinFsp dispatcher threads).
//!
//! `ParallelFile` opens handles with `FILE_FLAG_OVERLAPPED` and performs positional `ReadFile` /
//! `WriteFile` using a thread-local manual-reset Win32 Event + `OVERLAPPED` struct, unlocking
//! genuine concurrent kernel I/O on a single shared `HANDLE` for both `.pack` files and hot
//! overlay databases (such as Unity's `ArtifactDB`).
//!
//! Because `ParallelFile` encapsulates the underlying `File` and never exposes `std::io::Read`,
//! `std::io::Write`, or `FileExt::seek_read`/`seek_write`, std can never encounter
//! `STATUS_PENDING` on an overlapped handle.

use crate::error::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// A file handle that executes genuinely parallel positional reads/writes across threads.
pub struct ParallelFile {
    file: File,
}

impl ParallelFile {
    /// Opens an existing file read-only for parallel positional reads.
    pub fn open_read(path: &Path) -> io::Result<Self> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        configure_overlapped(&mut opts);
        Ok(Self {
            file: opts.open(path)?,
        })
    }

    /// Opens an existing file read/write for parallel positional I/O (does not create).
    pub fn open_existing_rw(path: &Path) -> io::Result<Self> {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true);
        configure_overlapped(&mut opts);
        Ok(Self {
            file: opts.open(path)?,
        })
    }

    /// Creates or truncates a file for parallel read/write positional I/O.
    pub fn create_empty_rw(path: &Path) -> io::Result<Self> {
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).read(true).write(true);
        configure_overlapped(&mut opts);
        Ok(Self {
            file: opts.open(path)?,
        })
    }

    /// Opens an existing file or creates it without truncating for parallel read/write I/O.
    pub fn open_or_create_rw(path: &Path) -> io::Result<Self> {
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(false).read(true).write(true);
        configure_overlapped(&mut opts);
        Ok(Self {
            file: opts.open(path)?,
        })
    }

    /// Marks the underlying file as an NTFS sparse file (best-effort no-op on non-Windows).
    pub fn set_sparse(&self) {
        let _ = layerfs_win32_host::set_sparse_file(&self.file);
    }

    /// Returns the file's logical byte length.
    pub fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Returns true if the file's logical byte length is 0.
    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Sets the file's logical length (truncates or sparse-extends).
    pub fn set_len(&self, new_len: u64) -> io::Result<()> {
        self.file.set_len(new_len)
    }

    /// Flushes OS buffers for this file to durable storage (`FlushFileBuffers` / `fdatasync`).
    pub fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    /// Positional read of up to `dst.len()` bytes starting at `offset` (`Ok(0)` at EOF).
    pub fn read_at(&self, offset: u64, dst: &mut [u8]) -> io::Result<usize> {
        if dst.is_empty() {
            return Ok(0);
        }
        platform::read_at(&self.file, offset, dst)
    }

    /// Positional read of exactly `dst.len()` bytes starting at `offset`.
    pub fn read_exact_at(&self, mut offset: u64, mut dst: &mut [u8]) -> Result<()> {
        while !dst.is_empty() {
            let n = self.read_at(offset, dst)?;
            if n == 0 {
                return Err(Error::Corrupt(format!(
                    "unexpected EOF reading file at offset {offset}"
                )));
            }
            offset += n as u64;
            dst = &mut dst[n..];
        }
        Ok(())
    }

    /// Positional write of all bytes in `data` starting at `offset`.
    pub fn write_all_at(&self, mut offset: u64, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = platform::write_at(&self.file, offset, data)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "write_at returned 0 bytes",
                ));
            }
            offset += n as u64;
            data = &data[n..];
        }
        Ok(())
    }
}

#[cfg(windows)]
fn configure_overlapped(opts: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
    // FILE_SHARE_READ (1) | FILE_SHARE_WRITE (2) | FILE_SHARE_DELETE (4)
    opts.share_mode(0x07);
    opts.custom_flags(FILE_FLAG_OVERLAPPED);
}

#[cfg(not(windows))]
fn configure_overlapped(_opts: &mut OpenOptions) {}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;

    const ERROR_HANDLE_EOF: u32 = 38;
    const ERROR_IO_PENDING: u32 = 997;

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        h_event: *mut c_void,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateEventW(
            event_attributes: *const c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn ReadFile(
            file: *mut c_void,
            buffer: *mut c_void,
            number_of_bytes_to_read: u32,
            number_of_bytes_read: *mut u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn WriteFile(
            file: *mut c_void,
            buffer: *const c_void,
            number_of_bytes_to_write: u32,
            number_of_bytes_written: *mut u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn GetOverlappedResult(
            file: *mut c_void,
            overlapped: *mut Overlapped,
            number_of_bytes_transferred: *mut u32,
            wait: i32,
        ) -> i32;
        fn GetLastError() -> u32;
    }

    struct ThreadEvent(*mut c_void);

    impl ThreadEvent {
        fn new() -> Self {
            // SAFETY: creates an unnamed manual-reset Win32 event owned by this thread.
            let h = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            assert!(!h.is_null(), "CreateEventW failed");
            Self(h)
        }
    }

    impl Drop for ThreadEvent {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: closes the thread-owned Win32 event handle on thread exit.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    thread_local! {
        static IO_EVENT: ThreadEvent = ThreadEvent::new();
    }

    #[inline]
    fn with_event<T>(f: impl FnOnce(*mut c_void) -> T) -> T {
        IO_EVENT.with(|ev| f(ev.0))
    }

    pub fn read_at(file: &File, offset: u64, dst: &mut [u8]) -> io::Result<usize> {
        let len = dst.len().min(u32::MAX as usize) as u32;
        let handle = file.as_raw_handle();
        with_event(|h_event| {
            let mut ov = Overlapped {
                internal: 0,
                internal_high: 0,
                offset: offset as u32,
                offset_high: (offset >> 32) as u32,
                h_event,
            };
            let mut transferred: u32 = 0;
            // SAFETY: `dst` is valid for `len` bytes and `ov` stays alive on stack until completion.
            let ok = unsafe {
                ReadFile(
                    handle,
                    dst.as_mut_ptr().cast::<c_void>(),
                    len,
                    &mut transferred,
                    &mut ov,
                )
            };
            if ok != 0 {
                return Ok(transferred as usize);
            }
            let err = unsafe { GetLastError() };
            if err == ERROR_HANDLE_EOF {
                return Ok(0);
            }
            if err == ERROR_IO_PENDING {
                let waited = unsafe { GetOverlappedResult(handle, &mut ov, &mut transferred, 1) };
                if waited != 0 {
                    return Ok(transferred as usize);
                }
                let err2 = unsafe { GetLastError() };
                if err2 == ERROR_HANDLE_EOF {
                    return Ok(0);
                }
                return Err(io::Error::from_raw_os_error(err2 as i32));
            }
            Err(io::Error::from_raw_os_error(err as i32))
        })
    }

    pub fn write_at(file: &File, offset: u64, data: &[u8]) -> io::Result<usize> {
        let len = data.len().min(u32::MAX as usize) as u32;
        let handle = file.as_raw_handle();
        with_event(|h_event| {
            let mut ov = Overlapped {
                internal: 0,
                internal_high: 0,
                offset: offset as u32,
                offset_high: (offset >> 32) as u32,
                h_event,
            };
            let mut transferred: u32 = 0;
            // SAFETY: `data` is valid for `len` bytes and `ov` stays alive on stack until completion.
            let ok = unsafe {
                WriteFile(
                    handle,
                    data.as_ptr().cast::<c_void>(),
                    len,
                    &mut transferred,
                    &mut ov,
                )
            };
            if ok != 0 {
                return Ok(transferred as usize);
            }
            let err = unsafe { GetLastError() };
            if err == ERROR_IO_PENDING {
                let waited = unsafe { GetOverlappedResult(handle, &mut ov, &mut transferred, 1) };
                if waited != 0 {
                    return Ok(transferred as usize);
                }
                let err2 = unsafe { GetLastError() };
                return Err(io::Error::from_raw_os_error(err2 as i32));
            }
            Err(io::Error::from_raw_os_error(err as i32))
        })
    }
}

#[cfg(not(windows))]
mod platform {
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::FileExt;

    pub fn read_at(file: &File, offset: u64, dst: &mut [u8]) -> io::Result<usize> {
        file.read_at(dst, offset)
    }

    pub fn write_at(file: &File, offset: u64, data: &[u8]) -> io::Result<usize> {
        file.write_at(data, offset)
    }
}
