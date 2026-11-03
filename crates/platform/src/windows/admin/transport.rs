use super::super::ffi;
use proto::admin::{MAX_FRAME, Reply, Request, VERSION};
use proto::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub user_sid: String,
    pub directory: PathBuf,
}
impl Config {
    pub fn daemon(&self) -> PathBuf {
        self.directory
            .join(format!("{}.exe", proto::names::DAEMON_BIN))
    }
    pub fn service(&self) -> PathBuf {
        self.directory
            .join(format!("{}.exe", proto::names::ADMIN_BIN))
    }
    pub fn validate(&self) -> Result<(), String> {
        if !super::valid_sid(&self.user_sid) || !self.directory.is_absolute() {
            return Err("invalid service configuration".into());
        }
        Ok(())
    }
}
fn same_path(a: &Path, b: &Path) -> bool {
    std::fs::canonicalize(a)
        .ok()
        .zip(std::fs::canonicalize(b).ok())
        .is_some_and(|(a, b)| {
            a.to_string_lossy()
                .eq_ignore_ascii_case(&b.to_string_lossy())
        })
}
pub fn authorize(config: &Config, sid: &str, path: &Path) -> Result<(), ApiError> {
    authorize_with(config, sid, path, super::service::verify_secure_chain)
}
fn authorize_with(
    config: &Config,
    sid: &str,
    path: &Path,
    verify: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), ApiError> {
    if sid != config.user_sid || !same_path(path, &config.daemon()) {
        return Err(ApiError::new(
            ErrorCode::Denied,
            "administrator pipe accepts only its installed daemon and configured user",
        ));
    }
    verify(&config.daemon()).map_err(|e| ApiError::new(ErrorCode::Denied, e))
}
struct Descriptor(PSECURITY_DESCRIPTOR);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn create(config: &Config, first: bool) -> io::Result<NamedPipeServer> {
    let sddl = ffi::to_wide(format!(
        "D:P(A;;GA;;;SY)(A;;0x0012019b;;;{})",
        config.user_sid
    ));
    let mut sd = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let sd = Descriptor(sd);
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .max_instances(4)
            .create_with_security_attributes_raw(
                super::endpoint(&config.user_sid),
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
            )
    }
}
pub async fn serve(
    config: Config,
    stop: Arc<AtomicBool>,
    ready: impl FnOnce(),
) -> Result<(), String> {
    config.validate()?;
    super::service::verify_secure_chain(&config.directory)?;
    for name in [
        "service.json",
        "recovery.json",
        "recovery.new",
        "audit.ndjson",
        "audit.previous.ndjson",
    ] {
        let path = config.directory.join(name);
        if path.exists() {
            super::service::verify_secure_chain(&path)?;
        }
    }
    let mut backend = super::operations::Backend::load(&config.directory)?;
    let _ = backend.restore(); // Retry failed receipts while serving read-only status.
    let serving = async {
        let mut next = create(&config, true).map_err(|e| e.to_string())?;
        if !stop.load(Ordering::SeqCst) {
            ready();
        }
        while !stop.load(Ordering::SeqCst) {
            let _ = backend.maintain();
            match tokio::time::timeout(Duration::from_millis(250), next.connect()).await {
                Err(_) => {
                    continue;
                }
                Ok(Err(e)) if matches!(e.raw_os_error(), Some(109 | 232 | 233)) => {
                    // A client may disconnect before connect() completes. Drop this
                    // instance and re-establish exclusive ownership of the pipe name.
                    drop(next);
                    next = create(&config, true).map_err(|e| e.to_string())?;
                    continue;
                }
                Ok(Err(e)) => return Err(e.to_string()),
                Ok(Ok(())) => {}
            }
            let fresh = create(&config, false).map_err(|e| e.to_string())?;
            let mut pipe = std::mem::replace(&mut next, fresh);
            let Some(pid) = ffi::pipe_client_pid(pipe.as_raw_handle()) else {
                continue;
            };
            let Some(path) = ffi::process_image_path(pid) else {
                continue;
            };
            let Ok(sid) = ffi::process_user_sid(pid) else {
                continue;
            };
            if authorize(&config, &sid, Path::new(&path)).is_err() {
                continue;
            }
            // One bounded frame per connection. Client PID, path and SID are OS supplied.
            let exchange = async {
                let mut bytes = Vec::new();
                let mut byte = [0u8; 1];
                while bytes.len() <= MAX_FRAME {
                    if pipe.read(&mut byte).await? == 0 {
                        return Ok::<(), io::Error>(());
                    }
                    if byte[0] == b'\n' {
                        break;
                    }
                    bytes.push(byte[0]);
                }
                if bytes.len() > MAX_FRAME {
                    return Ok(());
                }
                let request: Request = match serde_json::from_slice(&bytes) {
                    Ok(r) => r,
                    Err(_) => return Ok(()),
                };
                let result = if request.v != VERSION {
                    Err(ApiError::new(
                        ErrorCode::ProtocolVersion,
                        "administrator protocol mismatch",
                    ))
                } else {
                    request
                        .operation
                        .validate(daemon::store::now_ms())
                        .and_then(|_| backend.execute(&config, pid, &request.operation))
                };
                let reply = Reply::new(request.id, result);
                let mut bytes = serde_json::to_vec(&reply).map_err(io::Error::other)?;
                bytes.push(b'\n');
                if bytes.len() > MAX_FRAME {
                    return Err(io::Error::other("administrator reply too large"));
                }
                pipe.write_all(&bytes).await?;
                Ok(())
            };
            let _ = tokio::time::timeout(Duration::from_secs(15), exchange).await;
        }
        Ok::<(), String>(())
    }
    .await;
    let restored = backend.restore();
    match (serving, restored) {
        (Ok(()), result) => result,
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(recovery)) => Err(format!("{e}; recovery pending: {recovery}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matching_identity_and_verified_deployment_reach_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            user_sid: "S-1-5-21-1-2-3-1001".into(),
            directory: dir.path().into(),
        };
        std::fs::write(config.daemon(), b"test executable").unwrap();
        let mut verified = false;
        authorize_with(&config, &config.user_sid, &config.daemon(), |path| {
            verified = path == config.daemon();
            Ok(())
        })
        .unwrap();
        assert!(verified);
    }
    #[test]
    fn wrong_user_and_wrong_executable_cannot_reach_privileged_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            user_sid: "S-1-5-21-1-2-3-1001".into(),
            directory: dir.path().into(),
        };
        assert!(authorize(&config, "S-1-5-21-1-2-3-1002", &config.daemon()).is_err());
        assert!(
            authorize(
                &config,
                &config.user_sid,
                &dir.path().join("some-agent.exe")
            )
            .is_err()
        );
        std::fs::write(config.daemon(), b"test executable").unwrap();
        assert!(
            authorize(&config, &config.user_sid, &config.daemon()).is_err(),
            "matching path alone cannot authorize a writable installation"
        );
    }
}
