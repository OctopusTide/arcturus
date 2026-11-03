//! In-memory system implementations for tests. They record what they were asked to do and
//! never touch the real system.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use proto::{ApiError, ErrorCode};

use super::{
    ConnectivityHint, Elevated, Guard, MemoryStatus, PowerCapabilities, PowerMode, PowerSetting, PowerStatus,
    PowerValue, ProcessEntry, ProcessInfo, ProcessPriority, Suspend, Sys, System, SystemEvent, Temperature,
    TreeTracker, Usage, access_denied, needs_elevation,
};

pub const FAKE_PLAN: &str = "fake-plan";

type Sink = Arc<dyn Fn(SystemEvent) + Send + Sync>;

pub struct FakeSystem {
    pub resume_calls: Mutex<Vec<super::AgentResume>>,
    pub resume_error: Mutex<Option<ApiError>>,
    pub net: super::network::fake::FakeNetwork,
    /// pid → executable path, as `process_image_path` will report it.
    pub processes: Mutex<HashMap<u32, String>>,
    pub uptime_ms: AtomicU64,
    pub power: Mutex<PowerStatus>,
    pub temps: Mutex<Vec<Temperature>>,
    pub memory: Mutex<MemoryStatus>,
    pub connectivity: Mutex<ConnectivityHint>,
    pub capabilities: Mutex<PowerCapabilities>,
    pub restart_pending: AtomicBool,
    /// The "power plan": current value of every setting.
    pub settings: Mutex<BTreeMap<PowerSetting, PowerValue>>,
    pub power_mode: Mutex<String>,
    pub power_writes: Mutex<Vec<(PowerSetting, BTreeSet<u32>)>>,
    pub notifications: Mutex<Vec<(String, String)>>,
    /// Live keep-awake requests: (id, reason, display).
    pub awake: Arc<Mutex<Vec<(u64, String, bool)>>>,
    /// One-shot actions in order: "lock_screen", "display_off", "suspend(hibernate)" …
    pub actions: Mutex<Vec<String>>,
    sinks: Arc<Mutex<Vec<(u64, Sink)>>>,
    next_id: Arc<AtomicU64>,
    /// The process table `processes`, `track_tree` … work on (see [`FakeSystem::spawn`]).
    procs: Arc<Mutex<BTreeMap<u32, FakeProc>>>,
    jobs: Arc<Mutex<BTreeMap<u64, FakeJob>>>,
    pub gpu: Mutex<BTreeMap<u32, f64>>,
    /// path → last write, Unix ms.
    pub files: Mutex<BTreeMap<String, i64>>,
    /// Added to the real clock by `now_ms`.
    pub clock_offset_ms: AtomicI64,
    /// `track_tree` fails for every process, as if job objects were unavailable.
    pub refuse_jobs: AtomicBool,
    /// path → signer, for `file_signer`.
    pub signers: Mutex<BTreeMap<String, String>>,
    pub rules: Mutex<Vec<crate::arbitration::ProgramRule>>,
    pub suspended: Mutex<BTreeSet<u32>>,
    pub video: Mutex<super::VideoMemory>,
    pub freeze_reclaims_mb: AtomicU64,
}

/// One process in the fake process table.
#[derive(Debug, Clone)]
pub struct FakeProc {
    pub parent: u32,
    pub name: String,
    /// `C:\fake\<name>` unless set; `None`: the path cannot be read.
    pub exe: Option<String>,
    /// Runs under another account (a service, another user).
    pub other_user: bool,
    pub started_at: i64,
    pub usage: Usage,
    pub priority: Option<ProcessPriority>,
    /// Runs with administrator rights: its priority cannot be changed and a job cannot take it.
    pub elevated: bool,
}

#[derive(Debug, Default)]
struct FakeJob {
    members: BTreeSet<u32>,
    /// What members used before they exited.
    exited: Usage,
}

fn add(a: &mut Usage, b: &Usage) {
    a.cpu_ms += b.cpu_ms;
    a.io_ops += b.io_ops;
    a.io_bytes += b.io_bytes;
}

impl Default for FakeSystem {
    fn default() -> Self {
        let plan = |ac, dc| PowerValue {
            plan: FAKE_PLAN.into(),
            ac,
            dc,
        };
        let settings = BTreeMap::from([
            (PowerSetting::LidAction, plan(1, 1)),
            (PowerSetting::CpuMaxPercent, plan(100, 100)),
            (PowerSetting::DisplayBrightness, plan(80, 60)),
            (PowerSetting::SaverThreshold, plan(20, 20)),
            (PowerSetting::UsbSuspend, plan(1, 1)),
            (PowerSetting::WifiPowerSaving, plan(0, 2)),
        ]);
        FakeSystem {
            resume_calls: Mutex::default(),
            resume_error: Mutex::default(),
            power_writes: Mutex::default(),
            notifications: Mutex::default(),
            net: Default::default(),
            processes: Mutex::default(),
            uptime_ms: AtomicU64::new(3_600_000),
            power: Mutex::new(PowerStatus {
                on_ac: Some(true),
                battery_present: true,
                battery_percent: Some(80),
                ..Default::default()
            }),
            temps: Mutex::new(vec![Temperature {
                zone: "fake".into(),
                celsius: 45.0,
            }]),
            memory: Mutex::new(MemoryStatus {
                total_mb: 16_384,
                available_mb: 8_192,
                commit_limit_mb: 32_768,
                commit_used_mb: 12_000,
            }),
            connectivity: Mutex::new(ConnectivityHint {
                level: super::Connectivity::Internet,
                metered: false,
            }),
            capabilities: Mutex::new(PowerCapabilities {
                sleep: true,
                hibernate: true,
                modern_standby: true,
                lid: true,
            }),
            restart_pending: AtomicBool::new(false),
            settings: Mutex::new(settings),
            power_mode: Mutex::new(PowerMode::BALANCED.into()),
            awake: Arc::default(),
            actions: Mutex::default(),
            sinks: Arc::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            procs: Arc::default(),
            jobs: Arc::default(),
            gpu: Mutex::default(),
            files: Mutex::default(),
            clock_offset_ms: AtomicI64::new(0),
            refuse_jobs: AtomicBool::new(false),
            signers: Mutex::default(),
            rules: Mutex::default(),
            suspended: Mutex::default(),
            video: Mutex::default(),
            freeze_reclaims_mb: AtomicU64::new(0),
        }
    }
}

// ---- the fake process table --------------------------------------------------------------

impl FakeSystem {
    /// Starts a fake process. Like the OS, a child of a tracked process joins its tree.
    pub fn spawn(&self, pid: u32, parent: u32, name: &str) -> &Self {
        let started_at = self.now_ms();
        self.procs.lock().unwrap().insert(
            pid,
            FakeProc {
                parent,
                name: name.into(),
                exe: Some(format!(r"C:\fake\{name}")),
                other_user: false,
                started_at,
                usage: Usage::default(),
                priority: Some(ProcessPriority::Normal),
                elevated: false,
            },
        );
        for job in self.jobs.lock().unwrap().values_mut() {
            if job.members.contains(&parent) {
                job.members.insert(pid);
            }
        }
        self
    }

    /// The process exits; what it used stays counted in its tree.
    pub fn exit(&self, pid: u32) {
        let Some(p) = self.procs.lock().unwrap().remove(&pid) else {
            return;
        };
        for job in self.jobs.lock().unwrap().values_mut() {
            if job.members.remove(&pid) {
                add(&mut job.exited, &p.usage);
            }
        }
        self.gpu.lock().unwrap().remove(&pid);
    }

    pub fn alive(&self, pid: u32) -> bool {
        self.procs.lock().unwrap().contains_key(&pid)
    }

    fn with_proc(&self, pid: u32, f: impl FnOnce(&mut FakeProc)) {
        f(self
            .procs
            .lock()
            .unwrap()
            .get_mut(&pid)
            .expect("no such fake process"));
    }

    pub fn use_cpu(&self, pid: u32, ms: u64) {
        self.with_proc(pid, |p| p.usage.cpu_ms += ms);
    }

    pub fn use_io(&self, pid: u32, bytes: u64) {
        self.with_proc(pid, |p| {
            p.usage.io_ops += 1;
            p.usage.io_bytes += bytes;
        });
    }

    pub fn set_memory_mb(&self, pid: u32, mb: u64) {
        self.with_proc(pid, |p| p.usage.memory_mb = mb);
    }

    pub fn set_elevated(&self, pid: u32) {
        self.with_proc(pid, |p| p.elevated = true);
    }

    pub fn set_exe(&self, pid: u32, exe: Option<&str>) {
        self.with_proc(pid, |p| p.exe = exe.map(str::to_owned));
    }

    pub fn set_other_user(&self, pid: u32) {
        self.with_proc(pid, |p| p.other_user = true);
    }

    pub fn set_signer(&self, path: &str, signer: &str) {
        self.signers
            .lock()
            .unwrap()
            .insert(path.to_lowercase(), signer.into());
    }

    pub fn set_priority(&self, pid: u32, priority: Option<ProcessPriority>) {
        self.with_proc(pid, |p| p.priority = priority);
    }

    pub fn priority(&self, pid: u32) -> Option<ProcessPriority> {
        self.procs.lock().unwrap().get(&pid).and_then(|p| p.priority)
    }

    pub fn set_gpu(&self, pid: u32, percent: f64) {
        self.gpu.lock().unwrap().insert(pid, percent);
    }

    /// Writes the file now (by the fake clock).
    pub fn touch(&self, path: &str) {
        let now = self.now_ms();
        self.files.lock().unwrap().insert(path.into(), now);
    }

    pub fn advance_ms(&self, ms: i64) {
        self.clock_offset_ms.fetch_add(ms, Ordering::SeqCst);
    }

    /// Trees being tracked right now.
    pub fn trackers(&self) -> usize {
        self.jobs.lock().unwrap().len()
    }
}

struct FakeTracker {
    id: u64,
    jobs: Arc<Mutex<BTreeMap<u64, FakeJob>>>,
    procs: Arc<Mutex<BTreeMap<u32, FakeProc>>>,
}

impl TreeTracker for FakeTracker {
    fn members(&self) -> Result<Vec<u32>, ApiError> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .get(&self.id)
            .map(|j| j.members.iter().copied().collect())
            .unwrap_or_default())
    }

    fn usage(&self) -> Result<Usage, ApiError> {
        // Same lock order as everywhere else: processes, then jobs.
        let procs = self.procs.lock().unwrap();
        let jobs = self.jobs.lock().unwrap();
        let Some(job) = jobs.get(&self.id) else {
            return Ok(Usage::default());
        };
        let mut total = job.exited;
        for p in job.members.iter().filter_map(|pid| procs.get(pid)) {
            add(&mut total, &p.usage);
            total.memory_mb += p.usage.memory_mb;
        }
        Ok(total)
    }
}

impl Drop for FakeTracker {
    fn drop(&mut self) {
        // Stops tracking only: the processes carry on.
        self.jobs.lock().unwrap().remove(&self.id);
    }
}

impl FakeSystem {
    pub fn with_process(self, pid: u32, exe: &str) -> Self {
        self.processes.lock().unwrap().insert(pid, exe.to_owned());
        self
    }

    /// Delivers `event` to every `watch` subscriber, as the OS would.
    pub fn emit(&self, event: SystemEvent) {
        let sinks: Vec<Sink> = self
            .sinks
            .lock()
            .unwrap()
            .iter()
            .map(|(_, s)| s.clone())
            .collect();
        for sink in sinks {
            sink(event);
        }
    }

    pub fn watchers(&self) -> usize {
        self.sinks.lock().unwrap().len()
    }

    pub fn setting(&self, s: PowerSetting) -> PowerValue {
        self.settings.lock().unwrap()[&s].clone()
    }

    pub fn awake_reasons(&self) -> Vec<(String, bool)> {
        self.awake
            .lock()
            .unwrap()
            .iter()
            .map(|(_, r, d)| (r.clone(), *d))
            .collect()
    }

    pub fn actions(&self) -> Vec<String> {
        self.actions.lock().unwrap().clone()
    }

    pub fn set_power(&self, f: impl FnOnce(&mut PowerStatus)) {
        f(&mut self.power.lock().unwrap());
    }

    pub fn set_temp(&self, celsius: f64) {
        *self.temps.lock().unwrap() = vec![Temperature {
            zone: "fake".into(),
            celsius,
        }];
    }

    fn act(&self, what: impl Into<String>) {
        self.actions.lock().unwrap().push(what.into());
    }
}

struct FakeGuard(Box<dyn FnMut() + Send>);

impl Guard for FakeGuard {}

impl Drop for FakeGuard {
    fn drop(&mut self) {
        (self.0)();
    }
}

impl System for FakeSystem {
    fn notify_user(&self, title: &str, message: &str) -> Result<(), ApiError> {
        self.notifications
            .lock()
            .unwrap()
            .push((title.into(), message.into()));
        Ok(())
    }
    fn new_session_id(&self) -> Result<String, ApiError> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Ok(format!(
            "00000000-0000-4000-8000-{:012x}",
            NEXT.fetch_add(1, Ordering::SeqCst)
        ))
    }
    fn resume_agent(&self, request: &super::AgentResume) -> Result<ProcessInfo, ApiError> {
        if let Some(e) = self.resume_error.lock().unwrap().clone() {
            return Err(e);
        }
        let pid = 900_000 + self.resume_calls.lock().unwrap().len() as u32;
        self.resume_calls.lock().unwrap().push(request.clone());
        self.spawn(pid, 0, agents::file_name(&request.executable));
        self.set_exe(pid, Some(&request.executable));
        self.processes
            .lock()
            .unwrap()
            .insert(pid, request.executable.clone());
        self.process_info(pid)
    }
    fn network(&self) -> &dyn super::network::Network {
        &self.net
    }
    fn program_rules(&self) -> Result<Vec<crate::arbitration::ProgramRule>, ApiError> {
        Ok(self.rules.lock().unwrap().clone())
    }
    fn video_memory(&self) -> Result<super::VideoMemory, ApiError> {
        Ok(self.video.lock().unwrap().clone())
    }
    fn suspension_state(&self, pid: u32, at: i64) -> Result<Vec<super::ThreadState>, ApiError> {
        let p = self.process_info(pid)?;
        if p.started_at != at {
            return Err(ApiError::new(ErrorCode::NotFound, "process id reused"));
        }
        Ok(vec![super::ThreadState {
            tid: pid,
            started_at: at as u64,
            suspend_count: u32::from(self.suspended.lock().unwrap().contains(&pid)),
        }])
    }
    fn freeze_process(&self, pid: u32, at: i64, _: &[super::ThreadState]) -> Result<(), ApiError> {
        if self.process_info(pid)?.started_at != at {
            return Err(ApiError::new(ErrorCode::NotFound, "pid reused"));
        }
        if self.procs.lock().unwrap()[&pid].elevated {
            return Err(access_denied("freeze_process"));
        }
        self.suspended.lock().unwrap().insert(pid);
        self.act(format!("freeze({pid})"));
        self.memory.lock().unwrap().available_mb += self.freeze_reclaims_mb.load(Ordering::SeqCst);
        Ok(())
    }
    fn thaw_process(&self, pid: u32, at: i64, _: &[super::ThreadState]) -> Result<(), ApiError> {
        if self.process_info(pid)?.started_at != at {
            return Ok(());
        }
        self.suspended.lock().unwrap().remove(&pid);
        self.act(format!("thaw({pid})"));
        Ok(())
    }
    fn terminate_process(&self, pid: u32, at: i64) -> Result<(), ApiError> {
        if self.process_info(pid)?.started_at != at {
            return Err(ApiError::new(ErrorCode::NotFound, "pid reused"));
        }
        if self.procs.lock().unwrap()[&pid].elevated {
            return Err(access_denied("terminate_process"));
        }
        let mb = self.process_usage(pid)?.memory_mb;
        self.exit(pid);
        self.suspended.lock().unwrap().remove(&pid);
        self.act(format!("terminate({pid})"));
        self.memory.lock().unwrap().available_mb += mb;
        Ok(())
    }
    fn process_image_path(&self, pid: u32) -> Option<String> {
        self.processes.lock().unwrap().get(&pid).cloned()
    }

    fn uptime_ms(&self) -> Result<u64, ApiError> {
        Ok(self.uptime_ms.load(Ordering::SeqCst))
    }

    fn power_status(&self) -> Result<PowerStatus, ApiError> {
        Ok(self.power.lock().unwrap().clone())
    }

    fn temperatures(&self) -> Result<Vec<Temperature>, ApiError> {
        Ok(self.temps.lock().unwrap().clone())
    }

    fn memory(&self) -> Result<MemoryStatus, ApiError> {
        Ok(self.memory.lock().unwrap().clone())
    }

    fn connectivity(&self) -> Result<ConnectivityHint, ApiError> {
        Ok(*self.connectivity.lock().unwrap())
    }

    fn power_capabilities(&self) -> Result<PowerCapabilities, ApiError> {
        Ok(*self.capabilities.lock().unwrap())
    }

    fn restart_pending(&self) -> Result<bool, ApiError> {
        Ok(self.restart_pending.load(Ordering::SeqCst))
    }

    fn power_setting(&self, setting: PowerSetting) -> Result<PowerValue, ApiError> {
        Ok(self.setting(setting))
    }

    fn set_power_setting(&self, setting: PowerSetting, value: &PowerValue) -> Result<(), ApiError> {
        self.power_writes
            .lock()
            .unwrap()
            .push((setting, self.suspended.lock().unwrap().clone()));
        self.settings.lock().unwrap().insert(setting, value.clone());
        Ok(())
    }

    fn power_mode(&self) -> Result<String, ApiError> {
        Ok(self.power_mode.lock().unwrap().clone())
    }

    fn set_power_mode(&self, mode: &str) -> Result<(), ApiError> {
        *self.power_mode.lock().unwrap() = mode.to_owned();
        Ok(())
    }

    fn keep_awake(&self, reason: &str, display: bool) -> Result<Box<dyn Guard>, ApiError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.awake.lock().unwrap().push((id, reason.to_owned(), display));
        let awake = self.awake.clone();
        Ok(Box::new(FakeGuard(Box::new(move || {
            awake.lock().unwrap().retain(|(i, _, _)| *i != id)
        }))))
    }

    fn lock_screen(&self) -> Result<(), ApiError> {
        self.act("lock_screen");
        Ok(())
    }

    fn display_off(&self) -> Result<(), ApiError> {
        self.act("display_off");
        Ok(())
    }

    fn suspend(&self, how: Suspend) -> Result<(), ApiError> {
        self.act(format!("suspend({})", how.name()));
        Ok(())
    }

    fn watch(&self, sink: Arc<dyn Fn(SystemEvent) + Send + Sync>) -> Result<Box<dyn Guard>, ApiError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.sinks.lock().unwrap().push((id, sink));
        let sinks = self.sinks.clone();
        Ok(Box::new(FakeGuard(Box::new(move || {
            sinks.lock().unwrap().retain(|(i, _)| *i != id)
        }))))
    }

    fn processes(&self) -> Result<Vec<ProcessEntry>, ApiError> {
        let procs = self.procs.lock().unwrap();
        Ok(procs
            .iter()
            .map(|(pid, p)| ProcessEntry {
                pid: *pid,
                parent_pid: p.parent,
                name: p.name.clone(),
            })
            .collect())
    }

    fn process_info(&self, pid: u32) -> Result<ProcessInfo, ApiError> {
        let procs = self.procs.lock().unwrap();
        let p = procs
            .get(&pid)
            .ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("no process {pid}")))?;
        Ok(ProcessInfo {
            pid,
            exe: p.exe.clone(),
            started_at: p.started_at,
            own_user: Some(!p.other_user),
        })
    }

    fn process_usage(&self, pid: u32) -> Result<Usage, ApiError> {
        let procs = self.procs.lock().unwrap();
        procs
            .get(&pid)
            .map(|p| p.usage)
            .ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("no process {pid}")))
    }

    fn track_tree(&self, root: u32, members: &[u32]) -> Result<Box<dyn TreeTracker>, ApiError> {
        let procs = self.procs.lock().unwrap();
        match procs.get(&root) {
            None => return Err(ApiError::new(ErrorCode::NotFound, format!("no process {root}"))),
            Some(p) if p.elevated || self.refuse_jobs.load(Ordering::SeqCst) => {
                return Err(access_denied("track_tree"));
            }
            Some(_) => {}
        }
        let members: BTreeSet<u32> = std::iter::once(root)
            .chain(members.iter().copied())
            .filter(|p| procs.get(p).is_some_and(|p| !p.elevated))
            .collect();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.jobs.lock().unwrap().insert(
            id,
            FakeJob {
                members,
                exited: Usage::default(),
            },
        );
        Ok(Box::new(FakeTracker {
            id,
            jobs: self.jobs.clone(),
            procs: self.procs.clone(),
        }))
    }

    fn gpu_usage(&self) -> Result<BTreeMap<u32, f64>, ApiError> {
        Ok(self.gpu.lock().unwrap().clone())
    }

    fn process_priority(&self, pid: u32) -> Result<Option<ProcessPriority>, ApiError> {
        let procs = self.procs.lock().unwrap();
        procs
            .get(&pid)
            .map(|p| p.priority)
            .ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("no process {pid}")))
    }

    fn set_process_priority(&self, pid: u32, priority: ProcessPriority) -> Result<(), ApiError> {
        let mut procs = self.procs.lock().unwrap();
        let p = procs
            .get_mut(&pid)
            .ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("no process {pid}")))?;
        if p.elevated {
            return Err(access_denied("set_process_priority"));
        }
        p.priority = Some(priority);
        Ok(())
    }

    fn file_modified_ms(&self, path: &str) -> Result<Option<i64>, ApiError> {
        Ok(self.files.lock().unwrap().get(path).copied())
    }

    fn file_signer(&self, path: &str) -> Result<Option<String>, ApiError> {
        Ok(self.signers.lock().unwrap().get(&path.to_lowercase()).cloned())
    }

    fn now_ms(&self) -> i64 {
        crate::store::now_ms() + self.clock_offset_ms.load(Ordering::SeqCst)
    }
}

/// Records every privileged call and succeeds, keeping the privileged settings in memory.
#[derive(Default)]
pub struct FakeElevated {
    pub exit_policies: Mutex<BTreeMap<String, super::exit_firewall::ExitFirewallPolicy>>,
    pub calls: Mutex<Vec<String>>,
    pub update_pause: Mutex<Option<i64>>,
    pub adapters: Mutex<BTreeMap<String, bool>>,
}

impl FakeElevated {
    fn call(&self, what: String) {
        self.calls.lock().unwrap().push(what);
    }
}

impl Elevated for FakeElevated {
    fn restore_interface_metric(
        &self,
        interface: &str,
        metric: u32,
        automatic: bool,
    ) -> Result<(), ApiError> {
        self.call(format!(
            "restore_interface_metric({interface}, {metric}, {automatic})"
        ));
        Ok(())
    }
    fn exit_policy(
        &self,
        program: &str,
    ) -> Result<Option<super::exit_firewall::ExitFirewallPolicy>, ApiError> {
        Ok(self
            .exit_policies
            .lock()
            .unwrap()
            .get(&program.to_lowercase())
            .cloned())
    }
    fn set_exit_policy(&self, p: &super::exit_firewall::ExitFirewallPolicy) -> Result<(), ApiError> {
        p.validate()?;
        self.call(format!("set_exit_policy({}, {})", p.program, p.blocked));
        self.exit_policies
            .lock()
            .unwrap()
            .insert(p.program.to_lowercase(), p.clone());
        Ok(())
    }
    fn remove_exit_policy(&self, p: &str) -> Result<(), ApiError> {
        self.call(format!("remove_exit_policy({p})"));
        self.exit_policies.lock().unwrap().remove(&p.to_lowercase());
        Ok(())
    }
    fn set_exit_blocked(&self, p: &str, blocked: bool) -> Result<(), ApiError> {
        self.call(format!("set_exit_blocked({p}, {blocked})"));
        let mut all = self.exit_policies.lock().unwrap();
        let v = all
            .get_mut(&p.to_lowercase())
            .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "no egress policy"))?;
        v.blocked = blocked;
        Ok(())
    }
    fn backend(&self) -> &'static str {
        "fake"
    }

    fn set_interface_metric(&self, interface: &str, metric: u32) -> Result<(), ApiError> {
        self.call(format!("set_interface_metric({interface}, {metric})"));
        Ok(())
    }

    fn terminate_process(&self, pid: u32) -> Result<(), ApiError> {
        self.call(format!("terminate_process({pid})"));
        Ok(())
    }

    fn update_pause(&self) -> Result<Option<i64>, ApiError> {
        Ok(*self.update_pause.lock().unwrap())
    }

    fn set_update_pause(&self, until: Option<i64>) -> Result<(), ApiError> {
        self.call(format!(
            "set_update_pause({})",
            until.map_or("none".into(), |u| u.to_string())
        ));
        *self.update_pause.lock().unwrap() = until;
        Ok(())
    }

    fn adapter_power_saving(&self) -> Result<BTreeMap<String, bool>, ApiError> {
        Ok(self.adapters.lock().unwrap().clone())
    }

    fn set_adapter_power_saving(&self, state: &BTreeMap<String, bool>) -> Result<(), ApiError> {
        self.call(format!("set_adapter_power_saving({state:?})"));
        *self.adapters.lock().unwrap() = state.clone();
        Ok(())
    }

    fn power_requests(&self) -> Result<Vec<String>, ApiError> {
        Ok(vec!["fake request".into()])
    }

    fn set_process_priority(&self, pid: u32, priority: ProcessPriority) -> Result<(), ApiError> {
        self.call(format!("set_process_priority({pid}, {priority:?})"));
        Ok(())
    }
}

/// The administrator layer of a user-mode daemon, in memory: refuses everything, like the
/// real one, and counts the attempts.
#[derive(Default)]
pub struct RefusingElevated {
    pub attempts: Mutex<Vec<&'static str>>,
}

impl RefusingElevated {
    fn refuse<T>(&self, op: &'static str) -> Result<T, ApiError> {
        self.attempts.lock().unwrap().push(op);
        Err(needs_elevation(op))
    }
}

impl Elevated for RefusingElevated {
    fn backend(&self) -> &'static str {
        "user"
    }

    fn set_interface_metric(&self, _interface: &str, _metric: u32) -> Result<(), ApiError> {
        self.refuse("set_interface_metric")
    }

    fn terminate_process(&self, _pid: u32) -> Result<(), ApiError> {
        self.refuse("terminate_process")
    }

    fn update_pause(&self) -> Result<Option<i64>, ApiError> {
        self.refuse("update_pause")
    }

    fn set_update_pause(&self, _until: Option<i64>) -> Result<(), ApiError> {
        self.refuse("set_update_pause")
    }

    fn adapter_power_saving(&self) -> Result<BTreeMap<String, bool>, ApiError> {
        self.refuse("adapter_power_saving")
    }

    fn set_adapter_power_saving(&self, _state: &BTreeMap<String, bool>) -> Result<(), ApiError> {
        self.refuse("set_adapter_power_saving")
    }

    fn power_requests(&self) -> Result<Vec<String>, ApiError> {
        self.refuse("power_requests")
    }

    fn set_process_priority(&self, _pid: u32, _priority: ProcessPriority) -> Result<(), ApiError> {
        self.refuse("set_process_priority")
    }
}

/// A [`Sys`] made of fakes, plus handles to inspect them.
pub fn sys() -> (Sys, Arc<FakeSystem>, Arc<FakeElevated>) {
    let system = Arc::new(FakeSystem::default());
    let elevated = Arc::new(FakeElevated::default());
    (
        Sys {
            system: system.clone(),
            elevated: elevated.clone(),
        },
        system,
        elevated,
    )
}
