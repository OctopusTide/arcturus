//! The system-call boundary. Modules and the framework reach the operating system only
//! through these traits; the platform crate provides the real implementations and
//! [`fake`] provides in-memory ones for tests, so tests never touch the real system.
//!
//! Signatures are OS-neutral (no handles, GUIDs or registry paths). A task that needs a new
//! system call adds a method here with an OS-neutral signature, implements it in
//! `crates/platform/src/<os>/`, and adds it to the fakes. Methods other than the first two
//! have a default body that answers [`unsupported`], so a new platform can be filled in one
//! call at a time.

pub mod exit_firewall;
pub mod fake;
pub mod network;
pub mod programs;

use std::collections::BTreeMap;
use std::sync::Arc;

use proto::{ApiError, ErrorCode, subcode};
use serde::{Deserialize, Serialize};

/// Ordinary system calls, run with the daemon's own (normal user) rights.
pub trait System: Send + Sync {
    fn new_session_id(&self) -> Result<String, ApiError> {
        Err(unsupported("new_session_id"))
    }
    /// Start a recognised agent's fixed resume operation, without a shell or bypass flags.
    fn resume_agent(&self, _request: &AgentResume) -> Result<ProcessInfo, ApiError> {
        Err(unsupported("resume_agent"))
    }
    /// Shared read-only network notifications, requests and DNS route assessment.
    fn network(&self) -> &dyn network::Network {
        &network::UNSUPPORTED
    }

    /// A desktop notification. Platforms without a desktop leave the notice to the UI.
    fn notify_user(&self, _title: &str, _message: &str) -> Result<(), ApiError> {
        Err(unsupported("notify_user"))
    }

    /// Full path of a running process's executable.
    fn process_image_path(&self, pid: u32) -> Option<String>;

    /// Milliseconds since the machine booted.
    fn uptime_ms(&self) -> Result<u64, ApiError>;

    // ---- machine readings (all read-only) ----

    /// Battery and power source.
    fn power_status(&self) -> Result<PowerStatus, ApiError> {
        Err(unsupported("power_status"))
    }

    /// Temperature sensors the OS exposes without a driver (coarse board zones).
    fn temperatures(&self) -> Result<Vec<Temperature>, ApiError> {
        Err(unsupported("temperatures"))
    }

    fn memory(&self) -> Result<MemoryStatus, ApiError> {
        Err(unsupported("memory"))
    }

    /// What the OS itself thinks of the network (no probing).
    fn connectivity(&self) -> Result<ConnectivityHint, ApiError> {
        Err(unsupported("connectivity"))
    }

    /// Which sleep states, hibernation and lid the machine has.
    fn power_capabilities(&self) -> Result<PowerCapabilities, ApiError> {
        Err(unsupported("power_capabilities"))
    }

    /// The OS is waiting for a restart to finish installing updates.
    fn restart_pending(&self) -> Result<bool, ApiError> {
        Err(unsupported("restart_pending"))
    }

    // ---- power plan settings (normal user rights) ----

    /// A setting of the active power plan, for both power sources.
    fn power_setting(&self, setting: PowerSetting) -> Result<PowerValue, ApiError> {
        Err(unsupported(setting.name()))
    }

    /// Writes `value.ac` / `value.dc` into plan `value.plan` and makes them take effect.
    fn set_power_setting(&self, setting: PowerSetting, _value: &PowerValue) -> Result<(), ApiError> {
        Err(unsupported(setting.name()))
    }

    /// The power mode slider ([`PowerMode`] names, or the OS's own id for others).
    fn power_mode(&self) -> Result<String, ApiError> {
        Err(unsupported("power_mode"))
    }

    fn set_power_mode(&self, _mode: &str) -> Result<(), ApiError> {
        Err(unsupported("set_power_mode"))
    }

    // ---- power actions ----

    /// Asks the OS not to sleep (and, with `display`, not to turn the screen off) until the
    /// returned guard is dropped. `reason` is shown by the OS's own tools.
    fn keep_awake(&self, _reason: &str, _display: bool) -> Result<Box<dyn Guard>, ApiError> {
        Err(unsupported("keep_awake"))
    }

    fn lock_screen(&self) -> Result<(), ApiError> {
        Err(unsupported("lock_screen"))
    }

    /// Turns the screen off now; input turns it back on.
    fn display_off(&self) -> Result<(), ApiError> {
        Err(unsupported("display_off"))
    }

    /// Sleeps, hibernates or shuts the machine down. Sleep and hibernate return after
    /// the machine wakes up.
    fn suspend(&self, how: Suspend) -> Result<(), ApiError> {
        Err(unsupported(how.name()))
    }

    // ---- notifications ----

    /// Calls `sink` for every [`SystemEvent`] until the guard is dropped. `sink` runs on a
    /// system thread and must return quickly. Right after the call the current lid state
    /// may be reported once (`SystemEvent::Lid` with `initial: true`).
    fn watch(&self, _sink: Arc<dyn Fn(SystemEvent) + Send + Sync>) -> Result<Box<dyn Guard>, ApiError> {
        Err(unsupported("watch"))
    }

    // ---- processes ----

    /// Every running process, in one snapshot (no process is opened, so it is cheap).
    fn processes(&self) -> Result<Vec<ProcessEntry>, ApiError> {
        Err(unsupported("processes"))
    }

    /// Executable and start time of one process; `NOT_FOUND` once it is gone.
    fn process_info(&self, _pid: u32) -> Result<ProcessInfo, ApiError> {
        Err(unsupported("process_info"))
    }

    /// What one process has used so far.
    fn process_usage(&self, _pid: u32) -> Result<Usage, ApiError> {
        Err(unsupported("process_usage"))
    }

    /// Tracks `root` and `members` as one tree: processes they start later join it by
    /// themselves. The tracker **never ends a process**, not when it is dropped and not when
    /// the daemon dies. Fails when `root` cannot be tracked this way (the caller then
    /// follows the tree through parent ids instead).
    fn track_tree(&self, _root: u32, _members: &[u32]) -> Result<Box<dyn TreeTracker>, ApiError> {
        Err(unsupported("track_tree"))
    }

    /// GPU use per process id: percent of its busiest engine, averaged since the previous
    /// call (the first call may report nothing).
    fn gpu_usage(&self) -> Result<BTreeMap<u32, f64>, ApiError> {
        Err(unsupported("gpu_usage"))
    }

    /// `None`: a class this interface does not set (real-time).
    fn process_priority(&self, _pid: u32) -> Result<Option<ProcessPriority>, ApiError> {
        Err(unsupported("process_priority"))
    }

    /// [`access_denied`] when the user's own rights are not enough (an elevated process).
    fn set_process_priority(&self, _pid: u32, _priority: ProcessPriority) -> Result<(), ApiError> {
        Err(unsupported("set_process_priority"))
    }

    /// Who signed an executable, when the signature checks out (embedded signatures, checked
    /// offline, including catalog signatures): certificate organization, e.g. "Microsoft Corporation". `None`:
    /// unsigned, or the signature does not verify.
    fn file_signer(&self, _path: &str) -> Result<Option<String>, ApiError> {
        Err(unsupported("file_signer"))
    }

    /// Stable full paths of non-removable OS/security programs and our own binaries,
    /// and defaults discovered on this machine. Never identify an action target by name.
    fn program_rules(&self) -> Result<Vec<crate::arbitration::ProgramRule>, ApiError> {
        Ok(Vec::new())
    }

    /// All actions verify creation time on the same process handle before changing it.
    /// Suspend one process (all threads); implementations must roll back partial failure.
    fn suspension_state(&self, _pid: u32, _started_at: i64) -> Result<Vec<ThreadState>, ApiError> {
        Err(unsupported("suspension_state"))
    }
    fn freeze_process(&self, _pid: u32, _started_at: i64, _threads: &[ThreadState]) -> Result<(), ApiError> {
        Err(unsupported("freeze_process"))
    }
    /// Undo our one suspension only, preserving suspensions owned by other tools.
    fn thaw_process(&self, _pid: u32, _started_at: i64, _threads: &[ThreadState]) -> Result<(), ApiError> {
        Err(unsupported("thaw_process"))
    }
    fn terminate_process(&self, _pid: u32, _started_at: i64) -> Result<(), ApiError> {
        Err(unsupported("terminate_process"))
    }
    /// Dedicated video memory; unavailable counters return an error, never a fake zero.
    fn video_memory(&self) -> Result<VideoMemory, ApiError> {
        Err(unsupported("video_memory"))
    }

    /// When a file was last written, Unix ms; `None` when it does not exist.
    fn file_modified_ms(&self, path: &str) -> Result<Option<i64>, ApiError> {
        match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(t) => Ok(t
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_millis() as i64)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ApiError::internal(format!("cannot read {path}: {e}"))),
        }
    }

    /// Wall clock, Unix ms. Fakes can move it forward.
    fn now_ms(&self) -> i64 {
        crate::store::now_ms()
    }
}

/// Operations that need administrator rights, behind one narrow interface. The desktop
/// daemon delegates fixed operations to an authenticated helper service. Without that
/// service the backend refuses with [`needs_elevation`]; module code stays the same.
///
/// Each setting comes as a read/write pair, so a module can journal the original before the
/// change; under a backend that refuses, already the read is refused and nothing changes.
pub trait Elevated: Send + Sync {
    fn suspension_state(&self, _pid: u32, _started_at: i64) -> Result<Vec<ThreadState>, ApiError> {
        Err(needs_elevation("suspension_state"))
    }
    fn freeze_process(&self, _pid: u32, _started_at: i64, _threads: &[ThreadState]) -> Result<(), ApiError> {
        Err(needs_elevation("freeze_process"))
    }
    fn thaw_process(&self, _pid: u32, _started_at: i64, _threads: &[ThreadState]) -> Result<(), ApiError> {
        Err(needs_elevation("thaw_process"))
    }
    fn terminate_process_checked(&self, _pid: u32, _started_at: i64) -> Result<(), ApiError> {
        Err(needs_elevation("terminate_process_checked"))
    }
    fn exit_policy(&self, _program: &str) -> Result<Option<exit_firewall::ExitFirewallPolicy>, ApiError> {
        Err(needs_elevation("exit_policy"))
    }
    fn set_exit_policy(&self, _policy: &exit_firewall::ExitFirewallPolicy) -> Result<(), ApiError> {
        Err(needs_elevation("set_exit_policy"))
    }
    fn remove_exit_policy(&self, _program: &str) -> Result<(), ApiError> {
        Err(needs_elevation("remove_exit_policy"))
    }
    fn set_exit_blocked(&self, _program: &str, _blocked: bool) -> Result<(), ApiError> {
        Err(needs_elevation("set_exit_blocked"))
    }

    /// Which backend performs these operations, e.g. `"user"` or `"service"`.
    fn backend(&self) -> &'static str;

    /// Sets the routing metric of a network interface (lower = preferred). `interface` is the
    /// platform's stable interface id as a string.
    fn set_interface_metric(&self, interface: &str, metric: u32) -> Result<(), ApiError>;
    fn restore_interface_metric(
        &self,
        interface: &str,
        metric: u32,
        automatic: bool,
    ) -> Result<(), ApiError> {
        if automatic {
            Err(needs_elevation("restore automatic interface metric"))
        } else {
            self.set_interface_metric(interface, metric)
        }
    }

    /// Ends a process the user's own rights cannot end (for example an elevated one).
    fn terminate_process(&self, pid: u32) -> Result<(), ApiError>;

    /// Until when OS updates (and their automatic restarts) are paused, Unix ms; `None` =
    /// not paused.
    fn update_pause(&self) -> Result<Option<i64>, ApiError> {
        Err(needs_elevation("update_pause"))
    }

    fn set_update_pause(&self, _until: Option<i64>) -> Result<(), ApiError> {
        Err(needs_elevation("set_update_pause"))
    }

    /// Network adapter id → may the OS turn the adapter off to save power.
    fn adapter_power_saving(&self) -> Result<BTreeMap<String, bool>, ApiError> {
        Err(needs_elevation("adapter_power_saving"))
    }

    fn set_adapter_power_saving(&self, _state: &BTreeMap<String, bool>) -> Result<(), ApiError> {
        Err(needs_elevation("set_adapter_power_saving"))
    }

    /// Every program's current "do not sleep" requests, as the OS lists them.
    fn power_requests(&self) -> Result<Vec<String>, ApiError> {
        Err(needs_elevation("power_requests"))
    }

    /// For processes the user's own rights cannot change (elevated ones).
    fn set_process_priority(&self, _pid: u32, _priority: ProcessPriority) -> Result<(), ApiError> {
        Err(needs_elevation("set_process_priority"))
    }
}

/// The error a backend without administrator rights returns.
pub fn needs_elevation(operation: &str) -> ApiError {
    ApiError::new(
        ErrorCode::Denied,
        format!("'{operation}' needs administrator rights; the daemon runs as a normal user"),
    )
    .with_subcode(subcode::NEEDS_ELEVATION)
}

/// Is this the [`needs_elevation`] refusal?
pub fn is_needs_elevation(e: &ApiError) -> bool {
    e.subcode.as_deref() == Some(subcode::NEEDS_ELEVATION)
}

/// The error for a system call this platform (or machine) does not offer.
pub fn unsupported(operation: &str) -> ApiError {
    ApiError::new(
        ErrorCode::NotImplemented,
        format!("'{operation}' is not available on this system"),
    )
    .with_subcode(subcode::UNSUPPORTED)
}

/// The OS refused the daemon's own rights.
pub fn access_denied(what: &str) -> ApiError {
    ApiError::new(
        ErrorCode::Denied,
        format!("{what}: access denied with the user's own rights"),
    )
    .with_subcode(subcode::ACCESS_DENIED)
}

pub fn is_access_denied(e: &ApiError) -> bool {
    e.subcode.as_deref() == Some(subcode::ACCESS_DENIED)
}

/// Every system-call implementation the daemon uses, handed in at startup.
#[derive(Clone)]
pub struct Sys {
    pub system: Arc<dyn System>,
    pub elevated: Arc<dyn Elevated>,
}

/// Holds something the OS gives back when dropped (a keep-awake request, a subscription).
pub trait Guard: Send {}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PowerStatus {
    /// `None` when the OS does not know.
    pub on_ac: Option<bool>,
    pub battery_present: bool,
    pub battery_percent: Option<u8>,
    pub charging: bool,
    /// The OS's own estimate of the time left on battery, seconds.
    pub os_remaining_s: Option<u64>,
    /// The OS battery saver is on.
    pub saver_on: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Temperature {
    pub zone: String,
    pub celsius: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryStatus {
    pub total_mb: u64,
    pub available_mb: u64,
    /// Physical memory plus page file: what frozen programs still count against.
    pub commit_limit_mb: u64,
    pub commit_used_mb: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Connectivity {
    #[default]
    Unknown,
    None,
    /// A network, but no internet.
    LocalOnly,
    /// Internet behind something (for example a sign-in page).
    Constrained,
    Internet,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectivityHint {
    pub level: Connectivity,
    /// The OS treats the current connection as metered.
    pub metered: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerCapabilities {
    pub sleep: bool,
    pub hibernate: bool,
    /// Sleep is "modern standby" (S0 low power idle) rather than S3.
    pub modern_standby: bool,
    pub lid: bool,
}

/// A power plan setting the daemon may change. Values are integers whose meaning is fixed
/// here, whatever the OS calls them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerSetting {
    /// What closing the lid does: 0 nothing, 1 sleep, 2 hibernate, 3 shut down.
    LidAction,
    /// Highest CPU performance state, percent.
    CpuMaxPercent,
    /// Screen brightness, percent.
    DisplayBrightness,
    /// Battery percentage at which the battery saver switches itself on; 0 = never.
    SaverThreshold,
    /// USB selective suspend: 0 off, 1 on.
    UsbSuspend,
    /// Wireless adapter power saving: 0 maximum performance … 3 maximum saving.
    WifiPowerSaving,
}

impl PowerSetting {
    pub const ALL: [PowerSetting; 6] = [
        PowerSetting::LidAction,
        PowerSetting::CpuMaxPercent,
        PowerSetting::DisplayBrightness,
        PowerSetting::SaverThreshold,
        PowerSetting::UsbSuspend,
        PowerSetting::WifiPowerSaving,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            PowerSetting::LidAction => "lid_action",
            PowerSetting::CpuMaxPercent => "cpu_max_percent",
            PowerSetting::DisplayBrightness => "display_brightness",
            PowerSetting::SaverThreshold => "saver_threshold",
            PowerSetting::UsbSuspend => "usb_suspend",
            PowerSetting::WifiPowerSaving => "wifi_power_saving",
        }
    }

    pub fn from_name(name: &str) -> Option<PowerSetting> {
        PowerSetting::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// One power plan setting on both power sources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerValue {
    /// The OS's id of the plan the values belong to.
    pub plan: String,
    /// Plugged in.
    pub ac: u32,
    /// On battery.
    pub dc: u32,
}

/// Names for [`System::power_mode`].
pub struct PowerMode;

impl PowerMode {
    pub const EFFICIENCY: &str = "efficiency";
    pub const BALANCED: &str = "balanced";
    pub const PERFORMANCE: &str = "performance";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Suspend {
    Sleep,
    Hibernate,
    ShutDown,
}

impl Suspend {
    pub const fn name(self) -> &'static str {
        match self {
            Suspend::Sleep => "sleep",
            Suspend::Hibernate => "hibernate",
            Suspend::ShutDown => "shut_down",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessEntry {
    pub pid: u32,
    /// May name a process that has exited, or a later one that reused the id.
    pub parent_pid: u32,
    /// Executable file name, e.g. `python.exe`.
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub exe: Option<String>,
    /// Unix ms. With the pid it identifies the process: ids are reused.
    pub started_at: i64,
    /// Runs under the daemon's own user account; `None` when that cannot be read.
    pub own_user: Option<bool>,
}

/// Resources used so far; memory is the current private memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub cpu_ms: u64,
    /// Reads, writes and other I/O; on Windows this includes network sockets.
    pub io_ops: u64,
    pub io_bytes: u64,
    pub memory_mb: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VideoMemory {
    pub used_mb: u64,
    pub per_pid_mb: BTreeMap<u32, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResume {
    pub agent: String,
    pub executable: String,
    pub session_id: String,
    #[serde(default)]
    pub fork_session_id: Option<String>,
    /// The conversation's last reported permission mode. Unknown: as it was started.
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(skip)]
    pub broker: Option<ProcessInfo>,
    pub cwd: Option<String>,
    pub prompt: String,
    #[serde(skip)]
    pub environment: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadState {
    pub tid: u32,
    pub started_at: u64,
    pub suspend_count: u32,
}

/// A process tree followed by the OS (a Windows job object). Dropping it stops the tracking
/// only; it never ends a process.
pub trait TreeTracker: Send {
    /// Processes in the tree now.
    fn members(&self) -> Result<Vec<u32>, ApiError>;
    /// Totals since tracking started, including processes that have exited; memory is the
    /// current total.
    fn usage(&self) -> Result<Usage, ApiError>;
}

/// OS process priority. There is deliberately no real-time class: it can freeze mouse,
/// keyboard and the daemon itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessPriority {
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
}

/// Something the OS reported, delivered through [`System::watch`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SystemEvent {
    /// `initial`: the state at subscription time, not a change.
    Lid {
        closed: bool,
        initial: bool,
    },
    PowerSource {
        on_ac: bool,
    },
    /// The user's session was locked or unlocked.
    Session {
        locked: bool,
    },
    Display {
        on: bool,
    },
    /// The machine is about to sleep or hibernate.
    Suspending,
    /// Back from sleep or hibernation.
    Resumed,
}
