//! The only privileged operations. Every reversible change has a durable service backup
//! as well as the daemon's journal; loss of the owning daemon restores it automatically.
use super::super::{ffi, process, resource};
use super::transport::Config;
use daemon::sys::{ProcessInfo, ProcessPriority};
use proto::admin::Operation;
use proto::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};
use windows_sys::Win32::NetworkManagement::IpHelper::*;
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows_sys::Win32::System::Registry::*;

const UPDATE_KEY: &str = r"SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate";
const UPDATE_NAMES: &[(&str, &str)] = &[
    (UPDATE_KEY, "PauseFeatureUpdatesStartTime"),
    (UPDATE_KEY, "PauseQualityUpdatesStartTime"),
    (
        r"SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU",
        "NoAutoRebootWithLoggedOnUsers",
    ),
];
#[derive(Clone, Serialize, Deserialize)]
struct RegValue {
    kind: u32,
    bytes: Vec<u8>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Metric {
    family: u16,
    value: u32,
    automatic: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Priority {
    pid: u32,
    started_at: i64,
    original: ProcessPriority,
}
#[derive(Clone, Serialize, Deserialize)]
struct Suspension {
    process: ProcessInfo,
    threads: Vec<daemon::sys::ThreadState>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    owner: Option<ProcessInfo>,
    until: Option<i64>,
    pause_cycle: Option<i64>,
    updates: Option<Vec<Option<RegValue>>>,
    adapters: Option<BTreeMap<String, bool>>,
    metrics: BTreeMap<String, Vec<Metric>>,
    priorities: Vec<Priority>,
    suspensions: Vec<Suspension>,
}
#[derive(Default, Serialize, Deserialize)]
struct RecoveryFile {
    #[serde(flatten)]
    current: State,
    #[serde(default)]
    pending: Vec<State>,
}
impl State {
    fn has_changes(&self) -> bool {
        self.updates.is_some()
            || self.adapters.is_some()
            || !self.metrics.is_empty()
            || !self.priorities.is_empty()
            || !self.suspensions.is_empty()
    }
}
pub struct Backend {
    directory: PathBuf,
    state: State,
    recovering: bool,
    pending: Vec<State>,
    retry_at: Instant,
    retry_delay: Duration,
    quarantine: Option<String>,
    last_error: Option<String>,
}
fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::internal(e.to_string())
}
impl Backend {
    pub fn load(directory: &Path) -> Result<Self, String> {
        let path = directory.join("recovery.json");
        let mut quarantine = None;
        let file: RecoveryFile = if path.exists() {
            let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
            match serde_json::from_slice(&bytes) {
                Ok(state) => state,
                Err(e) => {
                    let name = format!(
                        "recovery.corrupt.{}.{}.json",
                        daemon::store::now_ms(),
                        std::process::id()
                    );
                    std::fs::rename(&path, directory.join(&name)).map_err(|e| e.to_string())?;
                    quarantine = Some(format!(
                        "{name}: {e}; repair the retained journal before enabling changes"
                    ));
                    RecoveryFile::default()
                }
            }
        } else {
            RecoveryFile::default()
        };
        // A quarantined file remains a durable interlock across restarts.
        if quarantine.is_none() {
            for entry in std::fs::read_dir(directory).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("recovery.corrupt.")
                {
                    quarantine = Some("quarantined recovery journal requires administrator repair".into());
                    break;
                }
            }
        }
        Ok(Self {
            directory: directory.into(),
            state: file.current,
            pending: file.pending,
            recovering: true,
            retry_at: Instant::now(),
            retry_delay: Duration::from_secs(1),
            quarantine,
            last_error: None,
        })
    }
    fn save(&self) -> Result<(), ApiError> {
        if let Some(error) = &self.quarantine {
            return Err(internal(error));
        }
        let path = self.directory.join("recovery.json");
        let tmp = self.directory.join("recovery.new");
        let mut file = std::fs::File::create(&tmp).map_err(internal)?;
        file.write_all(
            &serde_json::to_vec(&RecoveryFile {
                current: self.state.clone(),
                pending: self.pending.clone(),
            })
            .map_err(internal)?,
        )
        .map_err(internal)?;
        file.sync_all().map_err(internal)?;
        drop(file);
        if unsafe {
            MoveFileExW(
                ffi::to_wide(&tmp).as_ptr(),
                ffi::to_wide(&path).as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(ffi::os_error("save administrator recovery"));
        }
        Ok(())
    }
    fn claim(&mut self, pid: u32) -> Result<(), ApiError> {
        let process = process::process_info(pid)?;
        if let Some(owner) = &self.state.owner {
            if owner.pid != pid || owner.started_at != process.started_at {
                return Err(ApiError::new(
                    ErrorCode::Denied,
                    "another daemon owns the administrator changes",
                ));
            }
        }
        self.state.owner = Some(process);
        self.save()
    }
    pub fn maintain(&mut self) -> Result<(), String> {
        if Instant::now() < self.retry_at {
            return Ok(());
        }
        let result = if self.recovering
            || self
                .state
                .owner
                .as_ref()
                .is_some_and(|p| matches!(same(p), Ok(false)))
        {
            self.restore()
        } else {
            let mut errors = Vec::new();
            if !self.pending.is_empty() {
                if let Err(e) = self.retry_recovery() {
                    errors.push(e);
                }
            }
            if self
                .state
                .until
                .is_some_and(|until| until <= daemon::store::now_ms())
            {
                if let Err(e) = self.restore_updates() {
                    errors.push(e.message);
                }
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            }
        };
        if let Err(e) = &result {
            self.last_error = Some(e.clone());
            self.retry_at = Instant::now() + self.retry_delay;
            self.retry_delay = (self.retry_delay * 2).min(Duration::from_secs(60));
        } else {
            self.retry_delay = Duration::from_secs(1);
        }
        result
    }
    pub fn restore(&mut self) -> Result<(), String> {
        if let Some(error) = &self.quarantine {
            return Err(error.clone());
        }
        let cycle = self.state.pause_cycle;
        let old = std::mem::replace(
            &mut self.state,
            State {
                pause_cycle: cycle,
                ..State::default()
            },
        );
        if old.has_changes() {
            self.pending.push(old);
        }
        self.recovering = false;
        self.retry_recovery()
    }
    fn retry_recovery(&mut self) -> Result<(), String> {
        let count = self.pending.len();
        let active = std::mem::take(&mut self.state);
        self.pending.push(active);
        let mut errors = Vec::new();
        for _ in 0..count {
            self.state = self.pending.remove(0);
            if let Err(e) = self.restore_current() {
                errors.push(e);
            }
            if self.state.has_changes() {
                self.pending.push(std::mem::take(&mut self.state));
            }
        }
        self.state = self.pending.remove(0);
        self.recovering = false;
        if let Err(e) = self.save() {
            errors.push(e.message);
        }
        self.last_error = (!errors.is_empty()).then(|| errors.join("; "));
        self.last_error.clone().map_or(Ok(()), Err)
    }
    fn conflicts_with_recovery(&self, op: &Operation) -> bool {
        self.pending.iter().any(|s| match op {
            Operation::SetUpdatePause { .. } => s.updates.is_some(),
            Operation::SetAdapterPowerSaving { state } => s
                .adapters
                .as_ref()
                .is_some_and(|old| state.keys().any(|k| old.contains_key(k))),
            Operation::InterfaceMetric { interface, .. } => s.metrics.contains_key(interface),
            Operation::Freeze { pid, started_at, .. } | Operation::Priority { pid, started_at, .. } => {
                s.suspensions
                    .iter()
                    .any(|p| p.process.pid == *pid && p.process.started_at == *started_at)
                    || s.priorities
                        .iter()
                        .any(|p| p.pid == *pid && p.started_at == *started_at)
            }
            _ => false,
        })
    }
    fn restore_updates(&mut self) -> Result<(), ApiError> {
        if let Some(values) = self.state.updates.clone() {
            let mut errors = Vec::new();
            for ((key, name), value) in UPDATE_NAMES.iter().zip(&values) {
                if let Err(e) = write_registry(key, name, value.as_ref()) {
                    errors.push(e.message);
                }
            }
            if !errors.is_empty() {
                return Err(internal(errors.join("; ")));
            }
            self.state.updates = None;
            self.state.until = None;
            self.save()?;
        }
        Ok(())
    }
    fn restore_current(&mut self) -> Result<(), String> {
        if let Some(e) = &self.quarantine {
            self.last_error = Some(e.clone());
            return Err(e.clone());
        }
        let mut errors = Vec::new();
        for receipt in self.state.suspensions.clone() {
            let result = same(&receipt.process).and_then(|live| {
                if live {
                    resource::thaw_process(receipt.process.pid, receipt.process.started_at, &receipt.threads)
                } else {
                    Ok(())
                }
            });
            match result {
                Ok(()) => {
                    self.state.suspensions.retain(|r| {
                        r.process.pid != receipt.process.pid
                            || r.process.started_at != receipt.process.started_at
                    });
                    if let Err(e) = self.save() {
                        errors.push(e.message);
                    }
                }
                Err(e) => errors.push(e.message),
            }
        }
        if let Err(e) = self.restore_updates() {
            errors.push(e.message);
        }
        if let Some(adapters) = self.state.adapters.clone() {
            // Restore each adapter independently; a removed adapter cannot prevent others.
            let mut pending = adapters.clone();
            for (name, value) in adapters {
                match write_adapters(&BTreeMap::from([(name.clone(), value)])) {
                    Ok(()) => {
                        pending.remove(&name);
                    }
                    Err(e) => errors.push(e.message),
                }
            }
            self.state.adapters = (!pending.is_empty()).then_some(pending);
            if let Err(e) = self.save() {
                errors.push(e.message);
            }
        }
        for (id, original) in self.state.metrics.clone() {
            match write_metric(&id, &original) {
                Ok(()) => {
                    self.state.metrics.remove(&id);
                    if let Err(e) = self.save() {
                        errors.push(e.message);
                    }
                }
                Err(e) => errors.push(e.message),
            }
        }
        for p in self.state.priorities.clone() {
            let result = match process::process_info(p.pid) {
                Ok(i) if i.started_at == p.started_at => {
                    resource::set_priority_checked(p.pid, p.started_at, p.original)
                }
                Ok(_) => Ok(()),
                Err(e) if e.code == ErrorCode::NotFound => Ok(()),
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => {
                    self.state
                        .priorities
                        .retain(|r| r.pid != p.pid || r.started_at != p.started_at);
                    if let Err(e) = self.save() {
                        errors.push(e.message);
                    }
                }
                Err(e) => errors.push(e.message),
            }
        }
        self.recovering = !errors.is_empty();
        if !self.recovering {
            self.state.owner = None;
            if let Err(e) = self.save() {
                errors.push(e.message);
                self.recovering = true;
            }
        }
        self.last_error = (!errors.is_empty()).then(|| errors.join("; "));
        self.last_error.clone().map_or(Ok(()), Err)
    }
    pub fn execute(&mut self, config: &Config, caller: u32, op: &Operation) -> Result<Value, ApiError> {
        op.validate(daemon::store::now_ms())?;
        let _ = self.maintain();
        if self.conflicts_with_recovery(op) {
            return Err(internal("this target still has a pending recovery receipt"));
        }
        if let Some(error) = &self.quarantine {
            if !matches!(
                op,
                Operation::Ping {}
                    | Operation::UpdatePause {}
                    | Operation::AdapterPowerSaving {}
                    | Operation::PowerRequests {}
                    | Operation::ExitFirewall { .. }
            ) {
                return Err(internal(error));
            }
        }
        let result = match op {
            Operation::RestoreInterfaceMetric { interface } => {
                self.claim(caller)?;
                if let Some(original) = self
                    .state
                    .metrics
                    .get(interface)
                    .or_else(|| self.pending.iter().find_map(|s| s.metrics.get(interface)))
                    .cloned()
                {
                    write_metric(interface, &original)?;
                    self.state.metrics.remove(interface);
                    for pending in &mut self.pending {
                        pending.metrics.remove(interface);
                    }
                    self.pending.retain(State::has_changes);
                    self.save()?;
                }
                Ok(Value::Null)
            }
            Operation::SuspensionState { pid, started_at } => {
                target(config, caller, *pid, *started_at)?;
                Ok(json!(resource::suspension_state(*pid, *started_at)?))
            }
            Operation::Freeze {
                pid,
                started_at,
                threads,
            } => {
                target(config, caller, *pid, *started_at)?;
                self.claim(caller)?;
                if self
                    .state
                    .suspensions
                    .iter()
                    .any(|r| r.process.pid == *pid && r.process.started_at == *started_at)
                {
                    return Err(ApiError::new(
                        ErrorCode::Conflict,
                        "service already owns this suspension",
                    ));
                }
                let threads: Vec<daemon::sys::ThreadState> =
                    serde_json::from_value(json!(threads)).map_err(internal)?;
                self.state.suspensions.push(Suspension {
                    process: process::process_info(*pid)?,
                    threads: threads.clone(),
                });
                self.save()?;
                resource::freeze_process(*pid, *started_at, &threads)?;
                Ok(Value::Null)
            }
            Operation::Thaw { pid, started_at, .. } => {
                if let Some(receipt) = self
                    .state
                    .suspensions
                    .iter()
                    .find(|r| r.process.pid == *pid && r.process.started_at == *started_at)
                    .or_else(|| {
                        self.pending
                            .iter()
                            .flat_map(|s| &s.suspensions)
                            .find(|r| r.process.pid == *pid && r.process.started_at == *started_at)
                    })
                    .cloned()
                {
                    if same(&receipt.process)? {
                        target(config, caller, *pid, *started_at)?;
                        resource::thaw_process(*pid, *started_at, &receipt.threads)?;
                    }
                    self.state
                        .suspensions
                        .retain(|r| r.process.pid != *pid || r.process.started_at != *started_at);
                    for pending in &mut self.pending {
                        pending
                            .suspensions
                            .retain(|r| r.process.pid != *pid || r.process.started_at != *started_at);
                    }
                    self.pending.retain(State::has_changes);
                    self.save()?;
                }
                Ok(Value::Null)
            }
            Operation::ExitFirewall { request } => {
                let request: daemon::sys::exit_firewall::ExitAdminRequest =
                    serde_json::from_value(request.clone()).map_err(internal)?;
                self.audit(caller, op, "before")?;
                super::super::exit_firewall::dispatch(&config.user_sid, request)
            }
            Operation::Ping {} => Ok(
                json!({"backend":"service","version":proto::admin::VERSION,"recovery_error":self.last_error}),
            ),
            Operation::UpdatePause {} => Ok(json!(self.state.until)),
            Operation::AdapterPowerSaving {} => Ok(json!(read_adapters()?)),
            Operation::PowerRequests {} => {
                let path = system_directory().join("powercfg.exe");
                let output = run_child(Command::new(path).arg("/requests"), None)?;
                Ok(json!(output.lines().map(str::to_owned).collect::<Vec<_>>()))
            }
            Operation::SetUpdatePause { until } => {
                self.claim(caller)?;
                if until.is_none_or(|t| t <= daemon::store::now_ms()) {
                    self.restore_updates()?;
                } else {
                    let now = daemon::store::now_ms();
                    let start = self
                        .state
                        .pause_cycle
                        .filter(|start| now < start.saturating_add(36 * 86_400_000))
                        .unwrap_or(now);
                    let deadline = start.saturating_add(35 * 86_400_000);
                    if now >= deadline || until.is_some_and(|until| until > deadline) {
                        return Err(ApiError::new(
                            ErrorCode::Denied,
                            "update pause cycle cannot be extended; one unpaused day is required",
                        ));
                    }
                    self.state.pause_cycle = Some(start);
                    if self.state.updates.is_none() {
                        self.state.updates = Some(
                            UPDATE_NAMES
                                .iter()
                                .map(|(k, n)| read_registry(k, n))
                                .collect::<Result<_, _>>()?,
                        );
                    }
                    self.state.until = *until;
                    self.save()?;
                    let date = daemon::log::utc_time(daemon::store::now_ms())[..10].to_owned();
                    let bytes: Vec<u8> = ffi::to_wide(&date).iter().flat_map(|n| n.to_le_bytes()).collect();
                    for (key, name) in &UPDATE_NAMES[..2] {
                        write_registry(
                            key,
                            name,
                            Some(&RegValue {
                                kind: REG_SZ,
                                bytes: bytes.clone(),
                            }),
                        )?;
                    }
                    write_registry(
                        UPDATE_NAMES[2].0,
                        UPDATE_NAMES[2].1,
                        Some(&RegValue {
                            kind: REG_DWORD,
                            bytes: 1u32.to_le_bytes().to_vec(),
                        }),
                    )?;
                }
                Ok(Value::Null)
            }
            Operation::SetAdapterPowerSaving { state } => {
                self.claim(caller)?;
                let current = read_adapters()?;
                if state.keys().any(|k| !current.contains_key(k)) {
                    return Err(ApiError::invalid_args("unknown or unsupported network adapter"));
                }
                if self.state.adapters.is_none() {
                    self.state.adapters = Some(current);
                }
                self.save()?;
                write_adapters(state)?;
                if self.state.adapters.as_ref() == Some(state) {
                    self.state.adapters = None;
                    self.save()?;
                }
                Ok(Value::Null)
            }
            Operation::InterfaceMetric { interface, metric } => {
                self.claim(caller)?;
                let original = read_metric(interface)?;
                self.state
                    .metrics
                    .entry(interface.clone())
                    .or_insert(original.clone());
                self.save()?;
                write_metric(
                    interface,
                    &original
                        .iter()
                        .map(|m| Metric {
                            family: m.family,
                            value: *metric,
                            automatic: false,
                        })
                        .collect::<Vec<_>>(),
                )?;
                Ok(Value::Null)
            }
            Operation::Terminate { pid, started_at } => {
                target(config, caller, *pid, *started_at)?;
                self.audit(caller, op, "before")?;
                resource::terminate_process(*pid, *started_at)?;
                Ok(Value::Null)
            }
            Operation::Priority {
                pid,
                started_at,
                level,
            } => {
                target(config, caller, *pid, *started_at)?;
                self.claim(caller)?;
                let level: ProcessPriority = serde_json::from_value(json!(level)).map_err(internal)?;
                if !self
                    .state
                    .priorities
                    .iter()
                    .any(|p| p.pid == *pid && p.started_at == *started_at)
                {
                    let original = process::process_priority(*pid)?.ok_or_else(|| {
                        ApiError::new(ErrorCode::Denied, "cannot change real-time priority")
                    })?;
                    self.state.priorities.push(Priority {
                        pid: *pid,
                        started_at: *started_at,
                        original,
                    });
                    self.save()?;
                }
                resource::set_priority_checked(*pid, *started_at, level)?;
                self.state
                    .priorities
                    .retain(|p| p.pid != *pid || p.started_at != *started_at || p.original != level);
                self.save()?;
                Ok(Value::Null)
            }
        };
        if !matches!(
            op,
            Operation::Ping {}
                | Operation::UpdatePause {}
                | Operation::AdapterPowerSaving {}
                | Operation::PowerRequests {}
        ) {
            self.audit(caller, op, if result.is_ok() { "ok" } else { "failed" })?;
        }
        result
    }
    fn audit(&self, pid: u32, op: &Operation, outcome: &str) -> Result<(), ApiError> {
        let path = self.directory.join("audit.ndjson");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
            let old = self.directory.join("audit.previous.ndjson");
            if unsafe {
                MoveFileExW(
                    ffi::to_wide(&path).as_ptr(),
                    ffi::to_wide(&old).as_ptr(),
                    MOVEFILE_REPLACE_EXISTING,
                )
            } == 0
            {
                return Err(ffi::os_error("rotate service audit"));
            }
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(internal)?;
        writeln!(
            f,
            "{}",
            json!({"ts":daemon::store::now_ms(),"caller_pid":pid,"operation":op,"outcome":outcome})
        )
        .map_err(internal)?;
        f.sync_all().map_err(internal)
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        // Covers unwinding as well as transport setup failures.
        if self.state.owner.is_some() || self.recovering || !self.pending.is_empty() {
            let _ = self.restore();
        }
    }
}
fn same(p: &ProcessInfo) -> Result<bool, ApiError> {
    match process::process_info(p.pid) {
        Ok(now) => Ok(now.started_at == p.started_at),
        Err(e) if e.code == ErrorCode::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
fn eligible_integrity(elevated: bool, target: u32, caller: u32) -> bool {
    !elevated && target <= 0x2000 && target <= caller
}
fn target(config: &Config, caller: u32, pid: u32, at: i64) -> Result<(), ApiError> {
    let info = process::process_info(pid)?;
    if pid <= 4
        || pid == std::process::id()
        || info.started_at != at
        || ffi::process_user_sid(pid).ok().as_deref() != Some(&config.user_sid)
    {
        return Err(ApiError::new(
            ErrorCode::Denied,
            "target identity or user is not eligible",
        ));
    }
    let (elevated, integrity) =
        ffi::process_security(pid).map_err(|_| ApiError::new(ErrorCode::Denied, "unknown target token"))?;
    let (_, caller_integrity) = ffi::process_security(caller)
        .map_err(|_| ApiError::new(ErrorCode::Denied, "unknown caller token"))?;
    if !eligible_integrity(elevated, integrity, caller_integrity) {
        return Err(ApiError::new(
            ErrorCode::Denied,
            "elevated or higher-integrity targets cannot be changed",
        ));
    }
    let path = info
        .exe
        .as_deref()
        .ok_or_else(|| ApiError::new(ErrorCode::Denied, "unknown process image"))?;
    let key = daemon::sys::programs::normalize(path)?;
    if resource::program_rules()?.iter().any(|r| {
        r.source == "builtin" && daemon::sys::programs::normalize(&r.path).ok().as_deref() == Some(&key)
    }) {
        return Err(ApiError::new(
            ErrorCode::Denied,
            "system, security and own programs cannot be changed",
        ));
    }
    Ok(())
}
pub fn system_directory() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
}

fn read_registry(path: &str, name: &str) -> Result<Option<RegValue>, ApiError> {
    let mut key = null_mut();
    let code = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            ffi::to_wide(path).as_ptr(),
            0,
            KEY_READ | KEY_WOW64_64KEY,
            &mut key,
        )
    };
    if code == 2 {
        return Ok(None);
    }
    if code != 0 {
        return Err(internal(format!("open update policy: {code}")));
    }
    let mut kind = 0;
    let mut size = 0;
    let code = unsafe {
        RegQueryValueExW(
            key,
            ffi::to_wide(name).as_ptr(),
            null(),
            &mut kind,
            null_mut(),
            &mut size,
        )
    };
    if code == 2 {
        unsafe {
            RegCloseKey(key);
        }
        return Ok(None);
    }
    if code != 0 || size > 4096 {
        unsafe {
            RegCloseKey(key);
        }
        return Err(internal("unreadable update policy"));
    }
    let mut bytes = vec![0u8; size as usize];
    let code = unsafe {
        RegQueryValueExW(
            key,
            ffi::to_wide(name).as_ptr(),
            null(),
            &mut kind,
            bytes.as_mut_ptr(),
            &mut size,
        )
    };
    unsafe {
        RegCloseKey(key);
    }
    if code != 0 {
        return Err(internal(format!("read update policy: {code}")));
    }
    bytes.truncate(size as usize);
    Ok(Some(RegValue { kind, bytes }))
}
fn write_registry(path: &str, name: &str, value: Option<&RegValue>) -> Result<(), ApiError> {
    let mut key = null_mut();
    let code = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            ffi::to_wide(path).as_ptr(),
            0,
            null(),
            0,
            KEY_SET_VALUE | KEY_WOW64_64KEY,
            null(),
            &mut key,
            null_mut(),
        )
    };
    if code != 0 {
        return Err(internal(format!(
            "write update policy needs administrator rights: {code}"
        )));
    }
    let code = unsafe {
        match value {
            Some(v) => RegSetValueExW(
                key,
                ffi::to_wide(name).as_ptr(),
                0,
                v.kind,
                v.bytes.as_ptr(),
                v.bytes.len() as u32,
            ),
            None => RegDeleteValueW(key, ffi::to_wide(name).as_ptr()),
        }
    };
    unsafe {
        RegFlushKey(key);
        RegCloseKey(key);
    }
    if code != 0 && code != 2 {
        return Err(internal(format!("write update policy: {code}")));
    }
    Ok(())
}

fn guid(id: &str) -> Result<windows_sys::core::GUID, ApiError> {
    let id = id.trim_matches(['{', '}']).replace('-', "");
    let bytes = (0..16)
        .map(|i| u8::from_str_radix(&id[i * 2..i * 2 + 2], 16).map_err(internal))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(windows_sys::core::GUID {
        data1: u32::from_be_bytes(bytes[..4].try_into().unwrap()),
        data2: u16::from_be_bytes(bytes[4..6].try_into().unwrap()),
        data3: u16::from_be_bytes(bytes[6..8].try_into().unwrap()),
        data4: bytes[8..].try_into().unwrap(),
    })
}
fn read_metric(id: &str) -> Result<Vec<Metric>, ApiError> {
    let mut luid = unsafe { std::mem::zeroed() };
    let code = unsafe { ConvertInterfaceGuidToLuid(&guid(id)?, &mut luid) };
    if code != 0 {
        return Err(ApiError::invalid_args("network interface does not exist"));
    }
    let mut out = Vec::new();
    for family in [AF_INET, AF_INET6] {
        let mut row = unsafe { std::mem::zeroed::<MIB_IPINTERFACE_ROW>() };
        unsafe {
            InitializeIpInterfaceEntry(&mut row);
        }
        row.Family = family;
        row.InterfaceLuid = luid;
        let code = unsafe { GetIpInterfaceEntry(&mut row) };
        if code == 0 {
            out.push(Metric {
                family,
                value: row.Metric,
                automatic: row.UseAutomaticMetric,
            });
        } else if code != 1168 {
            return Err(internal(format!("read interface metric: {code}")));
        }
    }
    if out.is_empty() {
        return Err(ApiError::invalid_args("interface has no IP configuration"));
    }
    Ok(out)
}
fn write_metric(id: &str, metrics: &[Metric]) -> Result<(), ApiError> {
    let mut luid = unsafe { std::mem::zeroed() };
    if unsafe { ConvertInterfaceGuidToLuid(&guid(id)?, &mut luid) } != 0 {
        return Err(ApiError::invalid_args("network interface no longer exists"));
    }
    for metric in metrics {
        let mut row = unsafe { std::mem::zeroed::<MIB_IPINTERFACE_ROW>() };
        unsafe {
            InitializeIpInterfaceEntry(&mut row);
        }
        row.Family = metric.family;
        row.InterfaceLuid = luid;
        let code = unsafe { GetIpInterfaceEntry(&mut row) };
        if code != 0 {
            return Err(internal(format!("read interface before metric: {code}")));
        }
        row.Metric = metric.value;
        row.UseAutomaticMetric = metric.automatic;
        let code = unsafe { SetIpInterfaceEntry(&mut row) };
        if code != 0 {
            return Err(internal(format!("set interface metric: {code}")));
        }
    }
    Ok(())
}
const READ_ADAPTERS: &str = r#"$ErrorActionPreference='Stop';$ids=@(Get-CimInstance Win32_NetworkAdapter | Where-Object {$_.PNPDeviceID} | ForEach-Object {$_.PNPDeviceID});$result=@{};Get-CimInstance -Namespace root/wmi -ClassName MSPower_DeviceEnable | ForEach-Object {$p=$_;if(@($ids | Where-Object {$p.InstanceName.StartsWith($_,[StringComparison]::OrdinalIgnoreCase)}).Count -gt 0){$result[$p.InstanceName]=[bool]$p.Enable}};ConvertTo-Json -InputObject $result -Compress"#;
const WRITE_ADAPTERS: &str = r#"$ErrorActionPreference='Stop';$wanted=[Console]::In.ReadToEnd() | ConvertFrom-Json;$ids=@(Get-CimInstance Win32_NetworkAdapter | Where-Object {$_.PNPDeviceID} | ForEach-Object {$_.PNPDeviceID});$items=@(Get-CimInstance -Namespace root/wmi -ClassName MSPower_DeviceEnable);foreach($entry in $wanted.PSObject.Properties){$item=@($items|Where-Object {$_.InstanceName -eq $entry.Name});if($item.Count -ne 1 -or @($ids|Where-Object {$entry.Name.StartsWith($_,[StringComparison]::OrdinalIgnoreCase)}).Count -eq 0){throw 'Unknown network adapter'};Set-CimInstance -InputObject $item[0] -Property @{Enable=[bool]$entry.Value} | Out-Null};'null'"#;
fn powershell(script: &str, input: Option<&Value>) -> Result<String, ApiError> {
    run_child(
        Command::new(system_directory().join(r"WindowsPowerShell\v1.0\powershell.exe")).args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ]),
        input,
    )
}
fn read_adapters() -> Result<BTreeMap<String, bool>, ApiError> {
    serde_json::from_str(powershell(READ_ADAPTERS, None)?.trim()).map_err(internal)
}
fn write_adapters(state: &BTreeMap<String, bool>) -> Result<(), ApiError> {
    powershell(WRITE_ADAPTERS, Some(&json!(state)))?;
    Ok(())
}
fn run_child(command: &mut Command, input: Option<&Value>) -> Result<String, ApiError> {
    let mut child = command
        .creation_flags(0x08000000)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(internal)?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .map_err(internal)?;
    } else {
        drop(child.stdin.take());
    }
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        out.take(65537).read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        err.take(65537).read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().map_err(internal)? {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(internal("administrator fixed operation timed out"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = out
        .join()
        .map_err(|_| internal("operation reader failed"))?
        .map_err(internal)?;
    let stderr = err
        .join()
        .map_err(|_| internal("operation error reader failed"))?
        .map_err(internal)?;
    if stdout.len() > 65536 || stderr.len() > 65536 {
        return Err(internal("administrator operation output exceeded limit"));
    }
    if !status.success() {
        return Err(internal(format!(
            "fixed administrator operation failed: {}",
            String::from_utf8_lossy(&stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&stdout)
        .trim_start_matches('\u{feff}')
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_file_is_durable_and_corruption_is_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut backend = Backend::load(dir.path()).unwrap();
        backend.state.until = Some(12345);
        backend.save().unwrap();
        assert_eq!(Backend::load(dir.path()).unwrap().state.until, Some(12345));
        backend.recovering = false;
        std::fs::write(dir.path().join("recovery.json"), b"broken").unwrap();
        assert!(Backend::load(dir.path()).unwrap().quarantine.is_some());
    }
    #[test]
    fn system_and_stale_targets_are_rejected_without_changing_them() {
        let sid = ffi::current_user_sid().unwrap();
        let config = Config {
            user_sid: sid,
            directory: PathBuf::from(r"D:\test"),
        };
        assert!(target(&config, std::process::id(), std::process::id(), 1).is_err());
        assert!(target(&config, std::process::id(), 4, 1).is_err());
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[test]
    fn elevation_and_higher_integrity_are_never_delegated() {
        assert!(eligible_integrity(false, 0x2000, 0x2000));
        assert!(!eligible_integrity(true, 0x2000, 0x2000));
        assert!(!eligible_integrity(false, 0x3000, 0x3000));
        assert!(!eligible_integrity(false, 0x2000, 0x1000));
    }
    #[test]
    fn corrupt_journal_is_retained_and_service_stays_read_only_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("recovery.json"), b"{broken").unwrap();
        {
            let mut backend = Backend::load(dir.path()).unwrap();
            assert!(backend.restore().is_err());
            assert!(backend.save().is_err());
        }
        let mut backend = Backend::load(dir.path()).unwrap();
        assert!(backend.restore().is_err());
        let retained: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(retained.len(), 1);
        assert_eq!(
            std::fs::read(retained[0].as_ref().unwrap().path()).unwrap(),
            b"{broken"
        );
    }
    #[test]
    fn failed_interface_restore_does_not_skip_stale_process_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let mut backend = Backend::load(dir.path()).unwrap();
        backend.state.metrics.insert(
            "invalid-interface".into(),
            vec![Metric {
                family: AF_INET,
                value: 50,
                automatic: true,
            }],
        );
        backend.state.priorities.push(Priority {
            pid: u32::MAX,
            started_at: 1,
            original: ProcessPriority::Normal,
        });
        assert!(backend.restore().is_err());
        assert!(backend.state.priorities.is_empty());
        assert_eq!(backend.pending.iter().map(|s| s.metrics.len()).sum::<usize>(), 1);
    }
    #[test]
    fn failed_recovery_backs_off_and_never_restores_new_active_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let mut backend = Backend::load(dir.path()).unwrap();
        backend.state.metrics.insert(
            "invalid-interface".into(),
            vec![Metric {
                family: AF_INET,
                value: 50,
                automatic: true,
            }],
        );
        assert!(backend.maintain().is_err());
        assert!(backend.retry_at > Instant::now());
        let next = backend.retry_at;
        assert!(backend.maintain().is_ok());
        assert_eq!(backend.retry_at, next, "no tight-loop restore attempts");
        backend.state.priorities.push(Priority {
            pid: u32::MAX,
            started_at: 123,
            original: ProcessPriority::Normal,
        });
        backend.retry_at = Instant::now();
        assert!(backend.maintain().is_err());
        assert_eq!(
            backend.state.priorities.len(),
            1,
            "old recovery must not undo active-owner changes"
        );
        assert!(!backend.conflicts_with_recovery(&Operation::ExitFirewall {
            request: json!({"op":"get","program":r"D:\apps\worker.exe"})
        }));
        let config = Config {
            user_sid: ffi::current_user_sid().unwrap(),
            directory: dir.path().into(),
        };
        let e = backend
            .execute(
                &config,
                std::process::id(),
                &Operation::ExitFirewall {
                    request: json!({"op":"unknown"}),
                },
            )
            .unwrap_err();
        assert!(
            e.message.contains("unknown variant"),
            "operation reaches its handler: {}",
            e.message
        );
    }
}
