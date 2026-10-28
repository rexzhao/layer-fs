//! The control pipe (ADR-010 §10.2): a named pipe per mount, one JSON line per request, `hello`/`status`/`unmount` in S1.
//!
//! # Why the DACL is spelled out
//!
//! A named pipe created without a security descriptor gets the **default DACL**, which lets `Everyone` read it. A mount's control channel answers questions
//! about a repository path and accepts `unmount`, so "anyone on the box may talk to it" is not a default worth inheriting: the descriptor here is exactly one
//! allow-ACE for the **current user**, and A11b reads it back with `GetSecurityInfo` to prove it is one ACE and not "close enough".
//!
//! # Why one instance
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` plus `nMaxInstances = 1` means a second creator fails instead of silently sharing: one mount, one control channel.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::pipe::{self, Response};

/// How long a client waits for a busy pipe before giving up (`PIPE_CONNECT_WAIT`, ADR-010 §10.2).
pub const CONNECT_WAIT_MS: u32 = 2_000;

/// The pipe name for a mount: `\\.\pipe\layerfs-<record>-<pid>`.
#[must_use]
pub fn pipe_name(record: &str, pid: u32) -> String {
    format!("\\\\.\\pipe\\layerfs-{record}-{pid}")
}

/// What the server needs to answer, injected so the loop stays free of `fs-api` types.
pub trait Serving: Send + Sync {
    /// Answer one request (the protocol module decides what is legal).
    fn answer(&self, frame: &[u8]) -> Response;
    /// Ask the mount to stop; `true` when the stop was accepted.
    fn request_stop(&self) -> bool;
    /// Whether the stop sequence has begun.
    fn shutting_down(&self) -> bool;
    /// How many file objects are open, **read now** (ADR-010 §7.2).
    fn open_handles(&self) -> usize;
    /// Whether the teardown has finished: unmounted, flushed, record removed.
    ///
    /// `unmount` must answer **after** this becomes true, not when the request is accepted: a client that gets `Unmounted` and then finds the record still
    /// there has been told something false about the machine.
    fn teardown_complete(&self) -> bool;
}

/// A control server for one mount: owns the pipe, answers until asked to stop.
pub struct Server {
    name: String,
    serving: Arc<dyn Serving>,
    stopping: Arc<AtomicBool>,
}

impl Server {
    /// Creates the pipe **and** its name; the pipe itself is created lazily by [`Server::run`] so a caller can put the name in the record first.
    #[must_use]
    pub fn new(name: String, serving: Arc<dyn Serving>) -> Self {
        Self {
            name,
            serving,
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The pipe name, for the mount record (`pipe=` field).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Runs the accept loop on the calling thread until the mount stops.
    ///
    /// One connection at a time is enough: the protocol is request/response and the callers are CLI commands that are done in milliseconds. A queue would be
    /// more code and more failure modes for a channel that answers two read-only questions and one shutdown.
    pub fn run(&self) -> Result<(), String> {
        while !self.stopping.load(Ordering::SeqCst) && !self.serving.shutting_down() {
            let Some(connection) = accept(&self.name)? else {
                continue;
            };
            // A bad client must not take the mount with it: every error here is this connection's problem, and the loop goes round again (A11).
            let _ = self.serve_one(connection);
        }
        Ok(())
    }

    fn serve_one(&self, mut connection: std::fs::File) -> Result<(), String> {
        // **An oversized frame is answered, then the connection ends.** Dropping it silently would leave the client waiting for a reply that never comes and
        // calling it a timeout; the protocol has a name for this case, so it uses it — and the accept loop carries on with the next client (A11).
        let frame = match read_frame(&mut connection) {
            Ok(frame) => frame,
            Err(over) if over == OVER_LIMIT => {
                let _ = connection.write_all(&pipe::frame(&Response::FrameTooLarge {
                    max: pipe::FRAME_MAX,
                }));
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        // `unmount` is intercepted here because it has to stop the mount, not describe it.
        let response = match serde_json::from_slice::<pipe::Request>(&frame) {
            Ok(request) if request.op == "unmount" && request.v == pipe::PROTOCOL => {
                // A **remote** `umount` refuses while a file is open (ADR-010 §7.2): the caller cannot see who has one, and unmounting under a live handle is
                // the kind of surprise that loses data. The wait is the caller's, and when it expires the mount is left exactly as it was.
                let wait_ms = request
                    .args
                    .get("wait_ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
                while self.serving.open_handles() > 0 && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                let handles = self.serving.open_handles();
                if handles > 0 {
                    Response::HandlesOpen { handles }
                } else if self.serving.request_stop() {
                    // **Answer at once, not after the teardown** — and that is a defect, not a choice: the answer "unmounted" is true, but the record is still
                    // there for a moment, so A12's second half ("the record is gone when the answer arrives") cannot pass yet. The reason is the thread
                    // structure, not the pipe: the mount process exits as soon as `mount_serving` returns, which kills this thread before it could answer a
                    // teardown-late reply. Fixing it needs the main thread to wait for this one (S1 §5.1's `DONE`), which is the next step.
                    Response::Unmounted {}
                } else {
                    Response::ShuttingDown
                }
            }
            Ok(request) if request.op == "unmount" => self.serving.answer(&frame),
            _ => self.serving.answer(&frame),
        };
        connection
            .write_all(&pipe::frame(&response))
            .map_err(|error| format!("write: {error}"))?;
        connection
            .flush()
            .map_err(|error| format!("flush: {error}"))?;
        Ok(())
    }
}

/// Marker returned by [`read_frame`] when a client sent more than the protocol allows.
const OVER_LIMIT: &str = "over-limit";

/// Reads one frame: up to the first newline, refusing anything longer than the protocol allows.
fn read_frame(connection: &mut std::fs::File) -> Result<Vec<u8>, String> {
    let mut frame = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match connection.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                frame.push(byte[0]);
                if frame.len() > pipe::FRAME_MAX {
                    // The caller answers `FrameTooLarge` and closes **this** connection; the server keeps serving.
                    return Err(OVER_LIMIT.to_owned());
                }
            }
            Err(error) => return Err(format!("read: {error}")),
        }
    }
    Ok(frame)
}

#[cfg(windows)]
mod win {

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub(crate) const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    pub(crate) const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `D:P(A;;GA;;;<sid>)` — one protected allow-ACE for the current user, and nothing else.
    pub(crate) fn current_user_sddl() -> Option<String> {
        // SAFETY: every call is paired with a close, and the buffers are sized from the API's own answer.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut needed: u32 = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            let mut buffer = vec![0u8; needed as usize];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            );
            if ok == 0 {
                CloseHandle(token);
                return None;
            }
            let user: &TOKEN_USER = &*(buffer.as_ptr().cast::<TOKEN_USER>());
            let mut sid_text: *mut u16 = std::ptr::null_mut();
            let converted = ConvertSidToStringSidW(user.User.Sid, &mut sid_text);
            CloseHandle(token);
            if converted == 0 || sid_text.is_null() {
                return None;
            }
            let mut length = 0usize;
            while *sid_text.add(length) != 0 {
                length += 1;
            }
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, length));
            windows_sys::Win32::Foundation::LocalFree(sid_text.cast());
            Some(format!("D:P(A;;GA;;;{sid})"))
        }
    }

    pub(crate) fn create_pipe(name: &str) -> Option<HANDLE> {
        let sddl = current_user_sddl()?;
        let sddl_wide = wide(&sddl);
        let mut descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR =
            std::ptr::null_mut();
        // SAFETY: the SDDL text is NUL-terminated and outlives the call; the descriptor is freed below.
        let converted = unsafe {
            windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_wide.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if converted == 0 {
            return None;
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let name_wide = wide(name);
        // SAFETY: pointers are valid for the call; the attributes live until it returns.
        let handle = unsafe {
            CreateNamedPipeW(
                name_wide.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                crate::pipe::FRAME_MAX as u32,
                crate::pipe::FRAME_MAX as u32,
                0,
                &attributes,
            )
        };
        // SAFETY: the descriptor was produced by the call above and is not used again.
        unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor) };
        if handle == INVALID_HANDLE_VALUE {
            None
        } else {
            Some(handle)
        }
    }
}

/// Waits for one client and hands back a connected stream, or `None` when the wait was interrupted.
#[cfg(windows)]
fn accept(name: &str) -> Result<Option<std::fs::File>, String> {
    let Some(handle) = win::create_pipe(name) else {
        return Err("cannot create the control pipe".to_owned());
    };
    // SAFETY: the handle is ours; if the connect fails the handle is closed below.
    let connected = unsafe {
        windows_sys::Win32::System::Pipes::ConnectNamedPipe(handle, std::ptr::null_mut())
    };
    let already = unsafe { windows_sys::Win32::Foundation::GetLastError() } == 535; // ERROR_PIPE_CONNECTED
    if connected == 0 && !already {
        // SAFETY: closing our own handle.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
        return Ok(None);
    }
    // SAFETY: ownership of the raw handle moves into `File`, which closes it on drop.
    Ok(Some(unsafe {
        <std::fs::File as std::os::windows::io::FromRawHandle>::from_raw_handle(handle as _)
    }))
}

#[cfg(not(windows))]
fn accept(_name: &str) -> Result<Option<std::fs::File>, String> {
    Err("the control pipe is only implemented on Windows".to_owned())
}

/// Connects to a mount's control pipe (`wait_ms` first if it is busy) and sends one request, returning its response.
#[cfg(windows)]
pub fn call(name: &str, request: &pipe::Request, wait_ms: u32) -> Result<Response, String> {
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut handle = unsafe {
        CreateFileW(
            name_wide.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        if error != 231 {
            // 231 = ERROR_PIPE_BUSY; anything else is a real failure to find the pipe.
            return Err(format!("cannot open {name}: os error {error}"));
        }
        let waited = unsafe {
            windows_sys::Win32::System::Pipes::WaitNamedPipeW(name_wide.as_ptr(), wait_ms)
        };
        if waited == 0 {
            return Err(format!("{name} is busy (waited {wait_ms} ms)"));
        }
        handle = unsafe {
            CreateFileW(
                name_wide.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(format!("cannot open {name} after waiting"));
        }
    }
    let mut connection = unsafe {
        <std::fs::File as std::os::windows::io::FromRawHandle>::from_raw_handle(handle as _)
    };
    let mut frame = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    frame.push(b'\n');
    connection
        .write_all(&frame)
        .map_err(|error| error.to_string())?;
    connection.flush().map_err(|error| error.to_string())?;
    let response_frame = read_frame(&mut connection)?;
    serde_json::from_slice(&response_frame).map_err(|error| format!("bad response: {error}"))
}

#[cfg(not(windows))]
pub fn call(_name: &str, _request: &pipe::Request, _wait_ms: u32) -> Result<Response, String> {
    Err("the control pipe is only implemented on Windows".to_owned())
}

#[cfg(test)]
mod tests {
    use super::pipe_name;

    /// The name is derived, not configured: `<record>-<pid>` is what a reader of the record can reconstruct without asking anyone.
    #[test]
    fn the_pipe_name_carries_the_record_and_the_pid() {
        assert_eq!(pipe_name("U", 4242), "\\\\.\\pipe\\layerfs-U-4242");
    }

    /// **A11b, the half that needs no pipe**: the descriptor names the current user and exactly one allow-ACE.
    #[cfg(windows)]
    #[test]
    fn the_dacl_is_one_allow_ace_for_the_current_user() {
        let sddl = super::win::current_user_sddl().expect("a SID is always available to a process");
        assert!(
            sddl.starts_with("D:P(A;;GA;;;"),
            "cell A11b: protected, one allow ACE, generic-all: {sddl}"
        );
        assert_eq!(
            sddl.matches("(A;").count(),
            1,
            "cell A11b: exactly one allow ACE, not 'close enough': {sddl}"
        );
        assert!(
            !sddl.contains("WD") && !sddl.contains("BU"),
            "cell A11b: no Everyone/Users ACE may appear: {sddl}"
        );
    }
}
