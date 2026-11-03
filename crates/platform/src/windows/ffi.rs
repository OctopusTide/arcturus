//! Small Win32 helpers. Read-only queries only: nothing here changes system state.

use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::RawHandle;
use std::ptr::{null, null_mut};
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, LocalFree, SYSTEMTIME};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Pipes::{
    GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

/// NUL-terminated UTF-16 for Win32 `W` functions.
pub fn to_wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// `INTERNAL` with the thread's last Win32 error, for a call that just failed.
pub fn os_error(what: &str) -> proto::ApiError {
    proto::ApiError::internal(format!("{what} failed: {}", io::Error::last_os_error()))
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by a successful Open* call and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// String SID ("S-1-5-21-...") of the user running this process.
pub fn current_user_sid() -> io::Result<String> {
    // SAFETY: GetCurrentProcess returns a pseudo handle that needs no closing.
    unsafe { token_user_sid(GetCurrentProcess()) }
}

/// String SID of the user that owns process `pid`.
pub fn process_user_sid(pid: u32) -> io::Result<String> {
    // SAFETY: plain Win32 call; a null result is handled.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = OwnedHandle(process);
    // SAFETY: `process` is a valid process handle for the duration of the call.
    unsafe { token_user_sid(process.0) }
}

/// # Safety
/// `process` must be a valid process handle (or the current-process pseudo handle).
unsafe fn token_user_sid(process: HANDLE) -> io::Result<String> {
    let mut token: HANDLE = null_mut();
    // SAFETY: out-pointer is valid; failure is checked.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);

    let mut len = 0u32;
    // SAFETY: a zero-length query with a null buffer is the documented size probe.
    unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut len) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    // u64 storage keeps TOKEN_USER (which holds pointers) properly aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` has at least `len` writable bytes.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buf.as_mut_ptr().cast::<c_void>(),
            len,
            &mut len,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so the buffer starts with a TOKEN_USER.
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };

    let mut wide: *mut u16 = null_mut();
    // SAFETY: the SID pointer points into `buf`, which outlives the call.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut wide) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success `wide` is a NUL-terminated string allocated with LocalAlloc.
    let sid = unsafe { from_wide_ptr(wide) };
    // SAFETY: freeing the LocalAlloc'd string exactly once.
    unsafe { LocalFree(wide.cast::<c_void>()) };
    Ok(sid)
}

/// # Safety
/// `p` must point to a NUL-terminated UTF-16 string.
pub unsafe fn from_wide_ptr(p: *const u16) -> String {
    let mut len = 0;
    // SAFETY: guaranteed NUL-terminated by the caller.
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` elements were just read successfully.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

/// Full Win32 path of the executable of process `pid`, if it can be read.
pub fn process_image_path(pid: u32) -> Option<String> {
    // SAFETY: plain Win32 call; a null result is handled.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let process = OwnedHandle(process);
    let mut buf = vec![0u16; 32 * 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` writable u16s.
    if unsafe {
        QueryFullProcessImageNameW(process.0, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
    } == 0
    {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// Process id of the client connected to a server pipe handle.
pub fn pipe_client_pid(pipe: RawHandle) -> Option<u32> {
    let mut pid = 0u32;
    // SAFETY: `pipe` is a live pipe handle owned by the caller.
    (unsafe { GetNamedPipeClientProcessId(pipe as HANDLE, &mut pid) } != 0).then_some(pid)
}

/// Process id of the server behind a client pipe handle.
pub fn pipe_server_pid(pipe: RawHandle) -> Option<u32> {
    let mut pid = 0u32;
    // SAFETY: `pipe` is a live pipe handle owned by the caller.
    (unsafe { GetNamedPipeServerProcessId(pipe as HANDLE, &mut pid) } != 0).then_some(pid)
}

/// Waits until an instance of the pipe is free. Returns false on timeout or error.
pub fn wait_named_pipe(name: &str, timeout: Duration) -> bool {
    let wide = to_wide(name);
    let ms = timeout.as_millis().clamp(1, u128::from(u32::MAX - 1)) as u32;
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    unsafe { WaitNamedPipeW(wide.as_ptr(), ms) != 0 }
}

/// Whether any process has the pipe open as a server (busy or not). Does not connect.
pub fn pipe_exists(name: &str) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, GetLastError};
    let wide = to_wide(name);
    // SAFETY: `wide` is NUL-terminated and outlives the call; GetLastError right after.
    unsafe { WaitNamedPipeW(wide.as_ptr(), 1) != 0 || GetLastError() != ERROR_FILE_NOT_FOUND }
}

/// Formats a Unix timestamp in milliseconds as local wall-clock time, `YYYY-MM-DD HH:MM:SS`.
pub fn format_local_time(unix_ms: i64) -> String {
    // FILETIME counts 100 ns ticks since 1601-01-01.
    let ticks = (i128::from(unix_ms) * 10_000 + 116_444_736_000_000_000).max(0) as u64;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    // SAFETY: SYSTEMTIME is plain data; all-zero is a valid value.
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: all pointers refer to live stack values.
    let ok = unsafe {
        FileTimeToSystemTime(&ft, &mut utc) != 0
            && SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local) != 0
    };
    if !ok {
        return daemon::log::utc_time(unix_ms);
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_sid_looks_like_a_sid() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
        assert_eq!(process_user_sid(std::process::id()).unwrap(), sid);
    }

    #[test]
    fn own_image_path_is_readable() {
        let path = process_image_path(std::process::id()).unwrap();
        assert!(path.to_ascii_lowercase().ends_with(".exe"), "{path}");
    }

    #[test]
    fn local_time_has_fixed_shape() {
        let s = format_local_time(1_700_000_000_000);
        assert_eq!(s.len(), 19, "{s}");
        assert!(s.starts_with("2023-11-1"), "{s}");
    }
}

/// Return elevation and integrity RID. Unknown token information never authorizes
/// a privileged process operation.
pub fn process_security(pid: u32) -> io::Result<(bool, u32)> {
    use windows_sys::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, IsValidSid, TOKEN_ELEVATION,
        TOKEN_MANDATORY_LABEL, TokenElevation, TokenIntegrityLevel,
    };
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = OwnedHandle(process);
    let mut token = null_mut();
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    let mut elevated = TOKEN_ELEVATION::default();
    let mut size = 0;
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            (&mut elevated as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        GetTokenInformation(token.0, TokenIntegrityLevel, null_mut(), 0, &mut size);
    }
    if size == 0 || size > 65536 {
        return Err(io::Error::other("invalid token integrity size"));
    }
    let mut storage = vec![0u64; (size as usize).div_ceil(8)];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            storage.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let label = unsafe { &*storage.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
    if unsafe { IsValidSid(label.Label.Sid) } == 0 {
        return Err(io::Error::other("invalid integrity SID"));
    }
    let count = unsafe { *GetSidSubAuthorityCount(label.Label.Sid) };
    if count == 0 {
        return Err(io::Error::other("empty integrity SID"));
    }
    let rid = unsafe { *GetSidSubAuthority(label.Label.Sid, u32::from(count - 1)) };
    Ok((elevated.TokenIsElevated != 0, rid))
}
