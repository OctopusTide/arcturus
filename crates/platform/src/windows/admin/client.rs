use super::super::{ffi, process};
use daemon::sys::{Elevated, ProcessPriority, needs_elevation};
use proto::ApiError;
use proto::admin::{MAX_FRAME, Operation, Reply, Request, VERSION};
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, IntoRawHandle};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::NamedPipeClient;

pub struct ServiceElevated;
impl ServiceElevated {
    fn call<T: DeserializeOwned>(&self, operation: Operation) -> Result<T, ApiError> {
        operation.validate(daemon::store::now_ms())?;
        let result = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(15), rpc(operation))
                    .await
                    .map_err(|_| "administrator request timed out".to_owned())?
            })
        })
        .join()
        .map_err(|_| needs_elevation("administrator client failed"))?;
        match result {
            Ok(Ok(value)) => serde_json::from_value(value)
                .map_err(|e| ApiError::internal(format!("administrator result: {e}"))),
            Ok(Err(error)) => Err(error),
            Err(message) => Err(needs_elevation(&format!(
                "administrator service unavailable: {message}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_requests_fail_before_contacting_a_service() {
        let e = ServiceElevated
            .set_interface_metric("not-a-guid;calc", 1)
            .unwrap_err();
        assert_eq!(e.code, proto::ErrorCode::InvalidArgs);
        assert_eq!(
            ServiceElevated
                .set_update_pause(Some(i64::MAX))
                .unwrap_err()
                .code,
            proto::ErrorCode::InvalidArgs
        );
    }
}
async fn rpc(operation: Operation) -> Result<Result<serde_json::Value, ApiError>, String> {
    let sid = ffi::current_user_sid().map_err(|e| e.to_string())?;
    let dir = super::service::registered_directory(&sid)?;
    if !dir.join("service.json").is_file() {
        return Err("not installed".into());
    }
    super::service::verify_secure_chain(&dir.join("service.json"))?;
    let endpoint = super::endpoint(&sid);
    // Exclude FILE_CREATE_PIPE_INSTANCE from the client's rights.
    let file = std::fs::OpenOptions::new()
        .access_mode(0x0012019b)
        .custom_flags(0x40000000 | 0x00100000 | 0x00010000)
        .open(endpoint)
        .map_err(|e| e.to_string())?;
    let mut pipe = unsafe { NamedPipeClient::from_raw_handle(file.into_raw_handle()) }
        .map_err(|e| e.to_string())?;
    let pid = ffi::pipe_server_pid(pipe.as_raw_handle()).ok_or("no service PID")?;
    if ffi::process_user_sid(pid).is_ok_and(|sid| sid != "S-1-5-18") {
        return Err("pipe server is not LocalSystem".into());
    }
    let expected = std::fs::canonicalize(dir.join(format!("{}.exe", proto::names::ADMIN_BIN)))
        .map_err(|e| e.to_string())?;
    super::service::verify_secure_chain(&dir.join(format!("{}.exe", proto::names::ADMIN_BIN)))?;
    super::service::verify_service_process(&sid, pid, &expected)?;
    // SCM registration + running PID is authoritative even when a normal user
    // cannot query the LocalSystem image. A readable conflicting image is rejected.
    if let Some(actual) = ffi::process_image_path(pid) {
        let actual = std::fs::canonicalize(Path::new(&actual)).map_err(|e| e.to_string())?;
        if !actual
            .to_string_lossy()
            .eq_ignore_ascii_case(&expected.to_string_lossy())
        {
            return Err("wrong administrator service image".into());
        }
    }
    static ID: AtomicU64 = AtomicU64::new(1);
    let id = ID.fetch_add(1, Ordering::SeqCst);
    let mut request = serde_json::to_vec(&Request {
        v: VERSION,
        id,
        operation,
    })
    .map_err(|e| e.to_string())?;
    request.push(b'\n');
    if request.len() > MAX_FRAME {
        return Err("request too large".into());
    }
    pipe.write_all(&request).await.map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if bytes.len() >= MAX_FRAME {
            return Err("reply too large".into());
        }
        if pipe.read(&mut byte).await.map_err(|e| e.to_string())? == 0 {
            return Err("service closed without reply".into());
        }
        if byte[0] == b'\n' {
            break;
        }
        bytes.push(byte[0]);
    }
    let reply: Reply = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    // Preserve service operation errors rather than silently claiming ordinary-user fallback.
    Ok(reply.checked(id))
}
impl Elevated for ServiceElevated {
    fn restore_interface_metric(
        &self,
        interface: &str,
        _metric: u32,
        _automatic: bool,
    ) -> Result<(), ApiError> {
        self.call(Operation::RestoreInterfaceMetric {
            interface: interface.into(),
        })
    }
    fn suspension_state(
        &self,
        pid: u32,
        started_at: i64,
    ) -> Result<Vec<daemon::sys::ThreadState>, ApiError> {
        self.call(Operation::SuspensionState { pid, started_at })
    }
    fn freeze_process(
        &self,
        pid: u32,
        started_at: i64,
        threads: &[daemon::sys::ThreadState],
    ) -> Result<(), ApiError> {
        self.call(Operation::Freeze {
            pid,
            started_at,
            threads: threads
                .iter()
                .map(|t| proto::admin::ThreadReceipt {
                    tid: t.tid,
                    started_at: t.started_at,
                    suspend_count: t.suspend_count,
                })
                .collect(),
        })
    }
    fn thaw_process(
        &self,
        pid: u32,
        started_at: i64,
        threads: &[daemon::sys::ThreadState],
    ) -> Result<(), ApiError> {
        self.call(Operation::Thaw {
            pid,
            started_at,
            threads: threads
                .iter()
                .map(|t| proto::admin::ThreadReceipt {
                    tid: t.tid,
                    started_at: t.started_at,
                    suspend_count: t.suspend_count,
                })
                .collect(),
        })
    }
    fn terminate_process_checked(&self, pid: u32, started_at: i64) -> Result<(), ApiError> {
        self.call(Operation::Terminate { pid, started_at })
    }
    fn exit_policy(
        &self,
        program: &str,
    ) -> Result<Option<daemon::sys::exit_firewall::ExitFirewallPolicy>, ApiError> {
        self.call(Operation::ExitFirewall {
            request: serde_json::json!({"op":"get","program":program}),
        })
    }
    fn set_exit_policy(
        &self,
        policy: &daemon::sys::exit_firewall::ExitFirewallPolicy,
    ) -> Result<(), ApiError> {
        let _: bool = self.call(Operation::ExitFirewall {
            request: serde_json::json!({"op":"set","policy":policy}),
        })?;
        Ok(())
    }
    fn remove_exit_policy(&self, program: &str) -> Result<(), ApiError> {
        let _: bool = self.call(Operation::ExitFirewall {
            request: serde_json::json!({"op":"remove","program":program}),
        })?;
        Ok(())
    }
    fn set_exit_blocked(&self, program: &str, blocked: bool) -> Result<(), ApiError> {
        let _: bool = self.call(Operation::ExitFirewall {
            request: serde_json::json!({"op":"block","program":program,"blocked":blocked}),
        })?;
        Ok(())
    }
    fn backend(&self) -> &'static str {
        if self.call::<serde_json::Value>(Operation::Ping {}).is_ok() {
            "service"
        } else {
            "user"
        }
    }
    fn set_interface_metric(&self, interface: &str, metric: u32) -> Result<(), ApiError> {
        self.call(Operation::InterfaceMetric {
            interface: interface.into(),
            metric,
        })
    }
    fn terminate_process(&self, pid: u32) -> Result<(), ApiError> {
        let p = process::process_info(pid)?;
        self.call(Operation::Terminate {
            pid,
            started_at: p.started_at,
        })
    }
    fn set_process_priority(&self, pid: u32, level: ProcessPriority) -> Result<(), ApiError> {
        let p = process::process_info(pid)?;
        let level = serde_json::to_value(level)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        self.call(Operation::Priority {
            pid,
            started_at: p.started_at,
            level,
        })
    }
    fn update_pause(&self) -> Result<Option<i64>, ApiError> {
        self.call(Operation::UpdatePause {})
    }
    fn set_update_pause(&self, until: Option<i64>) -> Result<(), ApiError> {
        self.call(Operation::SetUpdatePause { until })
    }
    fn adapter_power_saving(&self) -> Result<BTreeMap<String, bool>, ApiError> {
        self.call(Operation::AdapterPowerSaving {})
    }
    fn set_adapter_power_saving(&self, state: &BTreeMap<String, bool>) -> Result<(), ApiError> {
        self.call(Operation::SetAdapterPowerSaving {
            state: state.clone(),
        })
    }
    fn power_requests(&self) -> Result<Vec<String>, ApiError> {
        self.call(Operation::PowerRequests {})
    }
}
