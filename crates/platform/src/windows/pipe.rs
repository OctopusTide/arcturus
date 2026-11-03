//! Named-pipe transport. Server: local-only (remote clients rejected), DACL granting access
//! to the current user alone, first instance created with FILE_FLAG_FIRST_PIPE_INSTANCE so a
//! second daemon (or a squatter) cannot share the name. Client: refuses a pipe served by a
//! process of another Windows user.

use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

use daemon::transport::{BindError, Listener, Peer, Transport};
use proto::client::Stream;
use proto::{ApiError, ErrorCode};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

use super::ffi;

/// Security descriptor `D:P(A;;GA;;;<user SID>)`: a protected DACL with a single entry
/// giving the current user full access. Nobody else (not even SYSTEM) can open the pipe.
struct UserOnlyDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is immutable after creation and only read by CreateNamedPipeW.
unsafe impl Send for UserOnlyDescriptor {}
unsafe impl Sync for UserOnlyDescriptor {}

impl UserOnlyDescriptor {
    fn new() -> io::Result<Self> {
        let sddl = format!("D:P(A;;GA;;;{})", ffi::current_user_sid()?);
        let wide = ffi::to_wide(&sddl);
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: NUL-terminated SDDL and a valid out-pointer.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(wide.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut())
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(UserOnlyDescriptor(sd))
    }
}

impl Drop for UserOnlyDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW (LocalAlloc).
        unsafe { LocalFree(self.0) };
    }
}

fn create_instance(name: &str, sd: &UserOnlyDescriptor, first: bool) -> io::Result<NamedPipeServer> {
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    // SAFETY: `sa` and the descriptor it points to are alive for the duration of the call.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(name, (&mut sa as *mut SECURITY_ATTRIBUTES).cast::<c_void>())
    }
}

/// The Windows transport: endpoints are pipe names (`\\.\pipe\…`).
#[derive(Debug, Clone, Copy, Default)]
pub struct PipeTransport;

pub struct PipeListener {
    name: String,
    sd: UserOnlyDescriptor,
    next: NamedPipeServer,
}

impl Transport for PipeTransport {
    type Listener = PipeListener;

    /// Claims the pipe name. Must run inside a tokio runtime with I/O enabled.
    fn bind(&self, endpoint: &str) -> Result<PipeListener, BindError> {
        let sd = UserOnlyDescriptor::new().map_err(BindError::Io)?;
        let next = create_instance(endpoint, &sd, true).map_err(|e| {
            if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
                BindError::AlreadyRunning
            } else {
                BindError::Io(e)
            }
        })?;
        Ok(PipeListener { name: endpoint.to_owned(), sd, next })
    }
}

impl Listener for PipeListener {
    type Conn = NamedPipeServer;

    async fn accept(&mut self) -> io::Result<(NamedPipeServer, Peer)> {
        // `connect` is cancel-safe: if shutdown wins the race, `next` is still unused.
        let connected = self.next.connect().await;
        let fresh = create_instance(&self.name, &self.sd, false)?;
        let conn = std::mem::replace(&mut self.next, fresh);
        connected?;
        let pid = ffi::pipe_client_pid(conn.as_raw_handle());
        Ok((conn, Peer { pid }))
    }
}

fn ipc(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::IpcError, message)
}

/// Opens a client connection. `wait` bounds how long to wait when every pipe instance is
/// busy. `DAEMON_NOT_RUNNING` when nobody listens; `IPC_ERROR` when the pipe belongs to a
/// process of another Windows user.
pub fn connect(endpoint: &str, wait: Duration) -> Result<Box<dyn Stream>, ApiError> {
    let deadline = Instant::now() + wait;
    let file = loop {
        match OpenOptions::new().read(true).write(true).open(endpoint) {
            Ok(f) => break f,
            Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
                return Err(ApiError::new(
                    ErrorCode::DaemonNotRunning,
                    format!("daemon is not running (nothing listens on {endpoint})"),
                ));
            }
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(ipc("every pipe instance stayed busy until the timeout"));
                }
                ffi::wait_named_pipe(endpoint, left);
            }
            Err(e) => return Err(ipc(format!("cannot open {endpoint}: {e}"))),
        }
    };
    let pid = ffi::pipe_server_pid(file.as_raw_handle()).ok_or_else(|| ipc("cannot read the pipe server's process id"))?;
    let me = ffi::current_user_sid().map_err(|e| ipc(format!("cannot read own user SID: {e}")))?;
    let owner = ffi::process_user_sid(pid).map_err(|e| ipc(format!("cannot verify the user of pipe server pid {pid}: {e}")))?;
    if owner != me {
        return Err(ipc(format!("pipe server pid {pid} runs as another user; refusing to use it")));
    }
    Ok(Box::new(file))
}
