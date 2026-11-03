//! `daemon::sys` on Windows.

use std::collections::BTreeMap;
use std::sync::Arc;

use daemon::sys::{
    ConnectivityHint, Elevated, Guard, MemoryStatus, PowerCapabilities, PowerSetting, PowerStatus,
    PowerValue, ProcessEntry, ProcessInfo, ProcessPriority, Suspend, System, SystemEvent, Temperature,
    TreeTracker, Usage, needs_elevation,
};
use proto::ApiError;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;

use super::{events, ffi, power, process, sensors};

/// Ordinary Win32 calls with the daemon's own rights.
pub struct WindowsSystem;

impl System for WindowsSystem {
    fn new_session_id(&self) -> Result<String, ApiError> {
        super::resume_broker::session_id()
    }
    fn resume_agent(&self, request: &daemon::sys::AgentResume) -> Result<ProcessInfo, ApiError> {
        super::resume::launch(request)
    }
    fn program_rules(&self) -> Result<Vec<daemon::arbitration::ProgramRule>, ApiError> {
        super::resource::program_rules()
    }
    fn file_signer(&self, path: &str) -> Result<Option<String>, ApiError> {
        super::resource::file_signer(path)
    }
    fn video_memory(&self) -> Result<daemon::sys::VideoMemory, ApiError> {
        super::resource::video_memory()
    }
    fn suspension_state(&self, pid: u32, at: i64) -> Result<Vec<daemon::sys::ThreadState>, ApiError> {
        super::resource::suspension_state(pid, at)
    }
    fn freeze_process(
        &self,
        pid: u32,
        at: i64,
        threads: &[daemon::sys::ThreadState],
    ) -> Result<(), ApiError> {
        super::resource::freeze_process(pid, at, threads)
    }
    fn thaw_process(&self, pid: u32, at: i64, threads: &[daemon::sys::ThreadState]) -> Result<(), ApiError> {
        super::resource::thaw_process(pid, at, threads)
    }
    fn terminate_process(&self, pid: u32, at: i64) -> Result<(), ApiError> {
        super::resource::terminate_process(pid, at)
    }
    fn network(&self) -> &dyn daemon::sys::network::Network {
        &super::network_watch::NETWORK
    }

    fn notify_user(&self, title: &str, message: &str) -> Result<(), ApiError> {
        super::network_watch::notify_user(title, message)
    }

    fn process_image_path(&self, pid: u32) -> Option<String> {
        ffi::process_image_path(pid)
    }

    fn uptime_ms(&self) -> Result<u64, ApiError> {
        // SAFETY: no arguments, cannot fail.
        Ok(unsafe { GetTickCount64() })
    }

    fn power_status(&self) -> Result<PowerStatus, ApiError> {
        power::power_status()
    }

    fn temperatures(&self) -> Result<Vec<Temperature>, ApiError> {
        sensors::temperatures()
    }

    fn memory(&self) -> Result<MemoryStatus, ApiError> {
        sensors::memory()
    }

    fn connectivity(&self) -> Result<ConnectivityHint, ApiError> {
        sensors::connectivity()
    }

    fn power_capabilities(&self) -> Result<PowerCapabilities, ApiError> {
        power::capabilities()
    }

    fn restart_pending(&self) -> Result<bool, ApiError> {
        power::restart_pending()
    }

    fn power_setting(&self, setting: PowerSetting) -> Result<PowerValue, ApiError> {
        power::read_setting(setting)
    }

    fn set_power_setting(&self, setting: PowerSetting, value: &PowerValue) -> Result<(), ApiError> {
        power::write_setting(setting, value)
    }

    fn power_mode(&self) -> Result<String, ApiError> {
        power::power_mode()
    }

    fn set_power_mode(&self, mode: &str) -> Result<(), ApiError> {
        power::set_power_mode(mode)
    }

    fn keep_awake(&self, reason: &str, display: bool) -> Result<Box<dyn Guard>, ApiError> {
        power::keep_awake(reason, display)
    }

    fn lock_screen(&self) -> Result<(), ApiError> {
        power::lock_screen()
    }

    fn display_off(&self) -> Result<(), ApiError> {
        power::display_off()
    }

    fn suspend(&self, how: Suspend) -> Result<(), ApiError> {
        power::suspend(how)
    }

    fn watch(&self, sink: Arc<dyn Fn(SystemEvent) + Send + Sync>) -> Result<Box<dyn Guard>, ApiError> {
        events::watch(sink)
    }

    fn processes(&self) -> Result<Vec<ProcessEntry>, ApiError> {
        process::processes()
    }

    fn process_info(&self, pid: u32) -> Result<ProcessInfo, ApiError> {
        process::process_info(pid)
    }

    fn process_usage(&self, pid: u32) -> Result<Usage, ApiError> {
        process::process_usage(pid)
    }

    fn track_tree(&self, root: u32, members: &[u32]) -> Result<Box<dyn TreeTracker>, ApiError> {
        process::track_tree(root, members)
    }

    fn gpu_usage(&self) -> Result<BTreeMap<u32, f64>, ApiError> {
        process::gpu_usage()
    }

    fn process_priority(&self, pid: u32) -> Result<Option<ProcessPriority>, ApiError> {
        process::process_priority(pid)
    }

    fn set_process_priority(&self, pid: u32, priority: ProcessPriority) -> Result<(), ApiError> {
        process::set_process_priority(pid, priority)
    }
}

/// The admin layer of a daemon that runs as a normal user: every operation is refused with
/// `DENIED` / `core.NEEDS_ELEVATION`. A service backend will replace this type. The
/// read/write pairs (update pause, adapter power saving, power requests) use the trait's
/// refusing defaults.
pub struct UserModeElevated;

impl Elevated for UserModeElevated {
    fn backend(&self) -> &'static str {
        "user"
    }

    fn set_interface_metric(&self, _interface: &str, _metric: u32) -> Result<(), ApiError> {
        Err(needs_elevation("set_interface_metric"))
    }

    fn terminate_process(&self, _pid: u32) -> Result<(), ApiError> {
        Err(needs_elevation("terminate_process"))
    }
}
