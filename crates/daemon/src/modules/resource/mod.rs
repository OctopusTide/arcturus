//! Memory pressure: notify in normal mode; journal, freeze, wait, then terminate while away.
use crate::arbitration::Arbitration;
use crate::module::{
    ArgKind, ArgSpec, CommandSpec, Event, EventSpec, Module, ModuleCtx, ModuleRequest, ModuleSpec,
    RecoverOutcome, Section,
};
use crate::modules::settings;
use crate::store::JournalEntry;
use crate::sys::programs as permission;
use crate::sys::{MemoryStatus, ProcessInfo, ThreadState};
use proto::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

static SPEC: ModuleSpec = ModuleSpec::new("resource", Section::Usage, "Memory and VRAM: freeze, then kill")
    .commands(&[
        CommandSpec::new(
            "free",
            "Request memory; returns actual MB and a request id for pending work",
        )
        .short("free")
        .agent()
        .args(&[ArgSpec::new(
            "mb",
            ArgKind::Uint,
            "Additional available MB wanted; default: reach the pressure threshold",
        )]),
        CommandSpec::new("result", "Read a free request's actual reclaimed MB and state")
            .agent()
            .read_only()
            .args(&[ArgSpec::new("request_id", ArgKind::Uint, "Id returned by free").required()]),
        CommandSpec::new("check", "Sample resources and run one policy step now").agent(),
        CommandSpec::new("settings", "Resource thresholds and wait time")
            .agent()
            .read_only(),
        CommandSpec::new("configure", "Change resource settings (page only)").args(&[ArgSpec::new(
            "settings",
            ArgKind::Json,
            "Partial settings object",
        )
        .required()]),
    ])
    .publishes(&[
        EventSpec::new("frozen", "A process tree was frozen"),
        EventSpec::new("killed", "A process tree was ended"),
        EventSpec::new("thawed", "Programs were resumed; power settings may be restored"),
        EventSpec::new("notice", "User action or attention is needed"),
    ])
    .subscribes(&[
        "mode.changed",
        "task.snapshot",
        "permission.changed",
        "power.overheat",
        "core.started",
    ]);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceSettings {
    pub check_seconds: u32,
    pub available_percent: u32,
    pub freeze_wait_seconds: u32,
    pub video_limit_mb: Option<u64>,
}
impl Default for ResourceSettings {
    fn default() -> Self {
        Self {
            check_seconds: 5,
            available_percent: 10,
            freeze_wait_seconds: 30,
            video_limit_mb: None,
        }
    }
}
impl ResourceSettings {
    fn check(&self) -> Result<(), String> {
        if !(1..=300).contains(&self.check_seconds)
            || !(1..=90).contains(&self.available_percent)
            || !(1..=3600).contains(&self.freeze_wait_seconds)
            || self.video_limit_mb == Some(0)
        {
            return Err("check_seconds: 1..=300; available_percent: 1..=90; freeze_wait_seconds: 1..=3600; video_limit_mb: positive or null".into());
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Receipt {
    process: ProcessInfo,
    threads: Vec<ThreadState>,
    journal_id: i64,
}
struct Frozen {
    root: u32,
    at: i64,
    members: Vec<Receipt>,
    thermal: bool,
}
#[derive(Clone, Serialize)]
struct FreeRequest {
    request_id: u64,
    requested_mb: u64,
    before_mb: u64,
    target_mb: u64,
    freed_mb: u64,
    expires_at: i64,
    state: String,
}
struct Inner {
    settings: ResourceSettings,
    frozen: Vec<Frozen>,
    requests: BTreeMap<u64, FreeRequest>,
    next_id: u64,
    last: Value,
    notice: Option<String>,
    thermal: bool,
}
struct Resource {
    shared: Arc<Mutex<Inner>>,
    tx: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}
fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
pub fn create() -> Box<dyn Module> {
    Box::new(Resource {
        shared: Arc::new(Mutex::new(Inner {
            settings: ResourceSettings::default(),
            frozen: Vec::new(),
            requests: BTreeMap::new(),
            next_id: 1,
            last: Value::Null,
            notice: None,
            thermal: false,
        })),
        tx: None,
        worker: None,
    })
}

struct Candidate {
    root: u32,
    members: Vec<ProcessInfo>,
    mb: u64,
    wechat: bool,
}

fn candidates(ctx: &ModuleCtx, a: &Arbitration) -> Result<Vec<Candidate>, ApiError> {
    let mut rules = a.rules.clone();
    for mut rule in ctx.system().program_rules()? {
        let Ok(path) = permission::normalize(&rule.path) else {
            continue;
        };
        rule.path = path;
        if rule.source == "builtin" || !a.excluded_defaults.contains(&rule.path) {
            rules.push(rule);
        }
    }
    let procs = ctx.system().processes()?;
    let entries: BTreeMap<_, _> = procs.iter().map(|p| (p.pid, p)).collect();
    let infos: BTreeMap<_, _> = procs
        .iter()
        .filter_map(|p| ctx.system().process_info(p.pid).ok().map(|i| (p.pid, i)))
        .collect();
    let mut protected = a.protected.clone();
    let dependency_names: BTreeSet<_> = a
        .tasks
        .iter()
        .flat_map(|t| {
            t["dependency_names"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|n| n.as_str())
        })
        .collect();
    for p in &procs {
        let lower = p.name.to_lowercase();
        if dependency_names.contains(lower.strip_suffix(".exe").unwrap_or(&lower)) {
            protected.insert(p.pid);
        }
    }
    // Rewalk tasks between their periodic samples, including newly spawned dependencies' children.
    loop {
        let old = protected.len();
        for p in &procs {
            if protected.contains(&p.parent_pid)
                && infos
                    .get(&p.pid)
                    .zip(infos.get(&p.parent_pid))
                    .is_some_and(|(child, parent)| child.started_at >= parent.started_at)
            {
                protected.insert(p.pid);
            }
        }
        if old == protected.len() {
            break;
        }
    }
    let wrapper = |p: &ProcessInfo| {
        if protected.contains(&p.pid) || p.own_user != Some(true) {
            return false;
        }
        let Some(path) = p.exe.as_deref().and_then(|p| permission::normalize(p).ok()) else {
            return false;
        };
        let name = agents::file_name(&path).to_ascii_lowercase();
        matches!(name.as_str(), "cmd.exe" | "conhost.exe" | "openconsole.exe")
            && rules.iter().any(|r| {
                r.path.eq_ignore_ascii_case(&path) && matches!(r.source.as_str(), "default" | "builtin")
            })
            && !rules
                .iter()
                .any(|r| r.path.eq_ignore_ascii_case(&path) && r.source == "user")
    };
    let mut groups: BTreeMap<u32, Vec<ProcessInfo>> = BTreeMap::new();
    for p in infos.values() {
        // A protected parent (terminal/OS shell) is a boundary, not permission to freeze it.
        if protected.contains(&p.pid) || permission::blocked(ctx.system(), p, &rules).is_some() {
            continue;
        }
        let mut root = p.pid;
        let mut seen = BTreeSet::new();
        while seen.insert(root) {
            let Some(parent) = entries.get(&root).and_then(|e| infos.get(&e.parent_pid)) else {
                break;
            };
            if protected.contains(&parent.pid)
                || (permission::blocked(ctx.system(), parent, &rules).is_some() && !wrapper(parent))
                || parent.started_at > infos[&root].started_at
            {
                break;
            }
            root = parent.pid;
        }
        if infos.get(&root).is_some_and(|p| wrapper(p)) {
            root = p.pid;
        }
        groups.entry(root).or_default().push(p.clone());
    }
    let mut out = Vec::new();
    for (root, members) in groups {
        let set: BTreeSet<_> = members.iter().map(|p| p.pid).collect();
        // Walk through excluded OS wrappers, but reject actual protected/unknown descendants.
        let mut reachable = set.clone();
        let mut unsafe_child = false;
        loop {
            let old = reachable.len();
            for child in &procs {
                if reachable.contains(&child.parent_pid) && !reachable.contains(&child.pid) {
                    if infos.get(&child.pid).is_some_and(|p| wrapper(p)) {
                        reachable.insert(child.pid);
                    } else {
                        unsafe_child = true;
                    }
                }
            }
            if reachable.len() == old {
                break;
            }
        }
        if unsafe_child {
            continue;
        }
        let Ok(usages) = members
            .iter()
            .map(|p| ctx.system().process_usage(p.pid).map(|u| u.memory_mb))
            .collect::<Result<Vec<_>, _>>()
        else {
            continue;
        };
        let mb = usages.into_iter().sum();
        let wechat = members.iter().any(|p| {
            p.exe.as_deref().is_some_and(|s| {
                matches!(
                    s.rsplit(['\\', '/'])
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .as_str(),
                    "wechat.exe" | "weixin.exe"
                )
            })
        });
        out.push(Candidate {
            root,
            members,
            mb,
            wechat,
        });
    }
    out.sort_by_key(|c| (c.wechat, std::cmp::Reverse(c.mb), c.root));
    Ok(out)
}

fn live(ctx: &ModuleCtx, p: &ProcessInfo) -> Result<bool, ApiError> {
    match ctx.system().process_info(p.pid) {
        Ok(i) if i.started_at != p.started_at => Ok(false),
        Ok(i) if i.exe.is_some() && i.exe == p.exe => Ok(true),
        Ok(_) => Err(ApiError::internal(
            "process image is unavailable or changed; recovery receipt retained",
        )),
        Err(e) if e.code == ErrorCode::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
fn thaw(ctx: &ModuleCtx, a: &mut Arbitration, frozen: &mut Frozen) -> Result<(), ApiError> {
    let mut error = None;
    frozen.members.retain(|r| {
        let result = live(ctx, &r.process)
            .and_then(|same| {
                if same {
                    ctx.system()
                        .thaw_process(r.process.pid, r.process.started_at, &r.threads)
                } else {
                    Ok(())
                }
            })
            .and_then(|_| ctx.journal().mark_restored(r.journal_id));
        match result {
            Ok(()) => {
                a.frozen.remove(&r.process.pid);
                a.resource_frozen.remove(&r.process.pid);
                false
            }
            Err(e) => {
                error = Some(e);
                true
            }
        }
    });
    if let Some(e) = error { Err(e) } else { Ok(()) }
}
fn thaw_all(g: &mut Inner, ctx: &ModuleCtx, a: &mut Arbitration) -> Result<(), ApiError> {
    let had_frozen = !g.frozen.is_empty();
    let mut error = None;
    for f in &mut g.frozen {
        if let Err(e) = thaw(ctx, a, f) {
            error = Some(e);
        }
    }
    g.frozen.retain(|f| !f.members.is_empty());
    if let Some(e) = error {
        Err(e)
    } else if had_frozen {
        ctx.publish("thawed", json!({"remaining":0}))
    } else {
        Ok(())
    }
}

fn freeze(g: &mut Inner, ctx: &ModuleCtx, a: &mut Arbitration, c: &Candidate) -> Result<(), ApiError> {
    let mut f = Frozen {
        root: c.root,
        at: ctx.system().now_ms(),
        members: Vec::new(),
        thermal: g.thermal,
    };
    // Root first stops ordinary spawning; a second tree snapshot below rejects late children.
    let mut members = c.members.clone();
    members.sort_by_key(|p| (p.pid != c.root, p.pid));
    for p in members {
        let result = (|| {
            if !live(ctx, &p)? {
                return Err(ApiError::new(
                    ErrorCode::NotFound,
                    "process changed before freezing",
                ));
            }
            let threads = ctx.system().suspension_state(p.pid, p.started_at)?;
            let key = format!("frozen:{}:{}", p.pid, p.started_at);
            let entry =
                ctx.journal()
                    .record(&key, &json!({"process":p,"threads":threads}), Some(&json!(true)))?;
            a.frozen.insert(p.pid, p.started_at);
            a.resource_frozen.insert(p.pid, p.started_at);
            f.members.push(Receipt {
                process: p.clone(),
                threads: threads.clone(),
                journal_id: entry.id,
            });
            ctx.system().freeze_process(p.pid, p.started_at, &threads)
        })();
        if let Err(e) = result {
            let _ = thaw(ctx, a, &mut f);
            if !f.members.is_empty() {
                g.frozen.push(f);
            }
            return Err(e);
        }
    }
    let set: BTreeSet<_> = f.members.iter().map(|r| r.process.pid).collect();
    let stable = ctx.system().processes().map(|ps| {
        !ps.iter()
            .any(|p| set.contains(&p.parent_pid) && !set.contains(&p.pid))
    });
    if !matches!(stable, Ok(true)) {
        let _ = thaw(ctx, a, &mut f);
        if !f.members.is_empty() {
            g.frozen.push(f);
        }
        return Err(ApiError::internal(
            "process tree changed while freezing; rolled back",
        ));
    }
    ctx.publish("frozen",json!({"root_pid":c.root,"program":c.members.iter().find(|p|p.pid==c.root).and_then(|p|p.exe.as_ref()),"pids":set,"memory_mb":c.mb}))?;
    g.frozen.push(f);
    Ok(())
}

fn notify(g: &mut Inner, ctx: &ModuleCtx, reason: &str, details: Value) {
    if g.notice.as_deref() != Some(reason) {
        let _ = ctx.publish("notice", json!({"reason":reason,"details":details}));
        g.notice = Some(reason.into());
    }
}
fn threshold(g: &Inner, m: &MemoryStatus) -> u64 {
    m.total_mb
        .saturating_mul(u64::from(g.settings.available_percent))
        .div_ceil(100)
}
fn progress(g: &mut Inner, available: u64, now: i64) {
    for r in g.requests.values_mut().filter(|r| r.state == "pending") {
        if now >= r.expires_at {
            r.state = "expired".into();
            continue;
        }
        r.freed_mb = available.saturating_sub(r.before_mb);
        if available >= r.target_mb {
            r.state = "completed".into();
        }
    }
}

fn step(g: &mut Inner, ctx: &ModuleCtx) -> Result<Value, ApiError> {
    if !ctx.is_active() {
        return Ok(g.last.clone());
    }
    let mut a = ctx.arbitration();
    let memory = ctx.system().memory()?;
    let video = ctx.system().video_memory();
    progress(g, memory.available_mb, ctx.system().now_ms());
    let pending_target = g
        .requests
        .values()
        .filter(|r| r.state == "pending")
        .map(|r| r.target_mb)
        .max()
        .unwrap_or(0);
    let target = threshold(g, &memory).max(pending_target);
    let pressure = memory.total_mb > 0 && memory.available_mb < target;
    let cs = candidates(ctx, &a)?;
    let over_limit: Vec<_> = a
        .tasks
        .iter()
        .filter(|t| {
            let mut ids: BTreeSet<u32> = t["pids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_u64().map(|n| n as u32))
                .collect();
            let processes = ctx.system().processes().unwrap_or_default();
            loop {
                let before = ids.len();
                for p in &processes {
                    if ids.contains(&p.parent_pid) {
                        ids.insert(p.pid);
                    }
                }
                if before == ids.len() {
                    break;
                }
            }
            let current: u64 = ids
                .into_iter()
                .filter_map(|p| ctx.system().process_usage(p).ok())
                .map(|u| u.memory_mb)
                .sum();
            t["mem_limit_mb"].as_u64().is_some_and(|limit| current > limit)
        })
        .cloned()
        .collect();
    g.last = json!({"memory":memory,"video":video.as_ref().ok(),"video_error":video.as_ref().err(),
        "automatic":a.automatic,"candidates":cs.iter().map(|c|json!({"root_pid":c.root,"program":c.members.iter().find(|p|p.pid==c.root).and_then(|p|p.exe.as_ref()),"memory_mb":c.mb,"pids":c.members.iter().map(|p|p.pid).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        "task_limits_exceeded":over_limit});
    if !a.automatic {
        thaw_all(g, ctx, &mut a)?;
        for r in g.requests.values_mut().filter(|r| r.state == "pending") {
            r.state = "needs_user".into();
        }
        if pressure {
            notify(g, ctx, "memory_needs_user", g.last.clone());
        } else if video
            .as_ref()
            .is_ok_and(|v| g.settings.video_limit_mb.is_some_and(|limit| v.used_mb > limit))
        {
            notify(g, ctx, "video_needs_user", g.last.clone());
        }
        return Ok(g.last.clone());
    }
    if !a.permission_ready || !a.tasks_ready {
        notify(g, ctx, "safety_state_unavailable", Value::Null);
        for r in g.requests.values_mut().filter(|r| r.state == "pending") {
            r.state = "blocked".into();
        }
        return Ok(g.last.clone());
    }
    // New protection takes effect immediately, including for trees already frozen.
    let eligible: BTreeSet<_> = cs.iter().flat_map(|c| c.members.iter().map(|p| p.pid)).collect();
    let mut thaw_error = None;
    for f in &mut g.frozen {
        let exact_tree = cs.iter().find(|c| c.root == f.root).is_some_and(|c| {
            c.members.len() == f.members.len()
                && c.members.iter().all(|p| {
                    f.members
                        .iter()
                        .any(|r| r.process.pid == p.pid && r.process.started_at == p.started_at)
                })
        });
        if !exact_tree
            || f.members.iter().any(|r| !eligible.contains(&r.process.pid))
            || (f.thermal && !g.thermal && !pressure)
        {
            if let Err(e) = thaw(ctx, &mut a, f) {
                thaw_error = Some(e);
            }
        }
    }
    g.frozen.retain(|f| !f.members.is_empty());
    if let Some(e) = thaw_error {
        return Err(e);
    }
    if !over_limit.is_empty() {
        notify(g, ctx, "protected_task_memory_limit", json!(over_limit));
        for r in g.requests.values_mut().filter(|r| r.state == "pending") {
            r.state = "task_limit".into();
        }
        return Ok(g.last.clone());
    }
    let now = ctx.system().now_ms();
    if pressure {
        // A tree must still be fully eligible at the time of termination, after the wait.
        let due = g
            .frozen
            .iter()
            .position(|f| now.saturating_sub(f.at) >= i64::from(g.settings.freeze_wait_seconds) * 1000);
        if let Some(index) = due {
            let mut f = g.frozen.remove(index);
            let result = (|| {
                for r in f.members.iter().rev() {
                    if live(ctx, &r.process)? {
                        // Irreversible actions still have a durable record before the change.
                        let entry = ctx.journal().record_with_note(
                            &format!("kill:{}:{}", r.process.pid, r.process.started_at),
                            &json!({"process":r.process}),
                            Some(&json!(true)),
                            Some("termination cannot be undone"),
                        )?;
                        ctx.system()
                            .terminate_process(r.process.pid, r.process.started_at)?;
                        ctx.journal().mark_restored(entry.id)?;
                        ctx.publish(
                            "killed",
                            json!({"root_pid":f.root,"pid":r.process.pid,"program":r.process.exe,"reason":"memory"}),
                        )?;
                    }
                }
                Ok::<_, ApiError>(())
            })();
            let restored = thaw(ctx, &mut a, &mut f);
            if !f.members.is_empty() {
                g.frozen.push(f);
            }
            result?;
            restored?;
        } else {
            for c in cs.iter().filter(|c| c.mb > 0) {
                if c.members.iter().any(|p| a.frozen.contains_key(&p.pid)) {
                    continue;
                }
                match freeze(g, ctx, &mut a, c) {
                    Ok(()) => break,
                    Err(e) => notify(g, ctx, "freeze_failed", json!({"root_pid":c.root,"error":e})),
                }
            }
            if g.frozen.is_empty() && !cs.is_empty() {
                for r in g.requests.values_mut().filter(|r| r.state == "pending") {
                    r.state = "blocked".into();
                }
            }
        }
    } else if g.thermal {
        if let Some(c) = cs
            .iter()
            .filter(|c| c.members.iter().all(|p| !a.frozen.contains_key(&p.pid)))
            .filter(|c| {
                ctx.system()
                    .process_priority(c.root)
                    .ok()
                    .flatten()
                    .is_some_and(|p| p < crate::sys::ProcessPriority::Normal)
            })
            .min_by_key(|c| {
                (
                    ctx.system().process_priority(c.root).ok().flatten(),
                    std::cmp::Reverse(c.mb),
                )
            })
        {
            freeze(g, ctx, &mut a, c)?;
        }
    } else {
        g.notice = None;
    }
    // VRAM has no paging policy: opt-in cap terminates only unprotected consumers, while away.
    if let (Some(limit), Ok(v)) = (g.settings.video_limit_mb, video) {
        if v.used_mb > limit {
            if let Some(c) = cs
                .iter()
                .filter(|c| {
                    c.members
                        .iter()
                        .any(|p| v.per_pid_mb.get(&p.pid).copied().unwrap_or(0) > 0)
                })
                .max_by_key(|c| {
                    c.members
                        .iter()
                        .map(|p| v.per_pid_mb.get(&p.pid).copied().unwrap_or(0))
                        .sum::<u64>()
                })
            {
                for p in c.members.iter().rev() {
                    if live(ctx, p)? {
                        let entry = ctx.journal().record_with_note(
                            &format!("kill:{}:{}", p.pid, p.started_at),
                            &json!({"process":p}),
                            Some(&json!(true)),
                            Some("termination cannot be undone"),
                        )?;
                        ctx.system().terminate_process(p.pid, p.started_at)?;
                        ctx.journal().mark_restored(entry.id)?;
                        ctx.publish(
                            "killed",
                            json!({"root_pid":c.root,"pid":p.pid,"program":p.exe,"reason":"video_limit"}),
                        )?;
                    }
                }
            }
        }
    }
    let after = ctx.system().memory()?;
    progress(g, after.available_mb, ctx.system().now_ms());
    if pressure && g.frozen.is_empty() && cs.is_empty() {
        notify(g, ctx, "no_eligible_programs", g.last.clone());
        for r in g.requests.values_mut().filter(|r| r.state == "pending") {
            r.state = "insufficient".into();
        }
    }
    Ok(g.last.clone())
}

impl Resource {
    fn stop_worker(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
impl Drop for Resource {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
    }
}
impl Module for Resource {
    fn spec(&self) -> &'static ModuleSpec {
        &SPEC
    }
    fn start(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        lock(&self.shared).settings = settings::load(ctx)?;
        let (tx, rx) = mpsc::channel();
        let shared = self.shared.clone();
        let ctx = ctx.clone();
        self.worker = Some(
            std::thread::Builder::new()
                .name("resource".into())
                .spawn(move || {
                    loop {
                        let wait = Duration::from_secs(u64::from(lock(&shared).settings.check_seconds));
                        match rx.recv_timeout(wait) {
                            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                let mut g = lock(&shared);
                                if let Err(e) = step(&mut g, &ctx) {
                                    notify(&mut g, &ctx, "resource_action_failed", json!(e));
                                }
                            }
                        }
                    }
                })
                .map_err(|e| ApiError::internal(format!("resource worker: {e}")))?,
        );
        self.tx = Some(tx);
        Ok(())
    }
    fn stop(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        self.stop_worker();
        thaw_all(&mut lock(&self.shared), ctx, &mut ctx.arbitration())
    }
    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        match req.cmd {
            "settings" => Ok(json!(lock(&self.shared).settings)),
            "configure" => {
                let s = settings::patch(ctx, &req.args["settings"], &[], ResourceSettings::check)?;
                lock(&self.shared).settings = s.clone();
                Ok(json!({"settings":s}))
            }
            "check" => step(&mut lock(&self.shared), ctx),
            "result" => {
                let g = lock(&self.shared);
                g.requests
                    .get(&req.args["request_id"].as_u64().unwrap())
                    .map(|r| json!(r))
                    .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "no such free request"))
            }
            "free" => {
                let memory = ctx.system().memory()?;
                let mut g = lock(&self.shared);
                let mb = req.args["mb"]
                    .as_u64()
                    .unwrap_or_else(|| threshold(&g, &memory).saturating_sub(memory.available_mb));
                if mb > memory.total_mb {
                    return Err(ApiError::invalid_args("requested MB exceeds physical memory"));
                }
                let id = g.next_id;
                g.next_id += 1;
                if g.requests.len() >= 100 {
                    if let Some(old) = g
                        .requests
                        .iter()
                        .find(|(_, r)| r.state != "pending")
                        .map(|(id, _)| *id)
                    {
                        g.requests.remove(&old);
                    } else {
                        return Err(ApiError::invalid_args("too many pending requests"));
                    }
                }
                g.requests.insert(
                    id,
                    FreeRequest {
                        request_id: id,
                        requested_mb: mb,
                        before_mb: memory.available_mb,
                        target_mb: memory.available_mb.saturating_add(mb).min(memory.total_mb),
                        expires_at: ctx.system().now_ms().saturating_add(5 * 60_000),
                        freed_mb: 0,
                        state: "pending".into(),
                    },
                );
                step(&mut g, ctx)?;
                Ok(json!(g.requests[&id]))
            }
            _ => Err(ApiError::internal("unrouted resource command")),
        }
    }
    fn status(&self, _: &ModuleCtx) -> Value {
        let g = lock(&self.shared);
        json!({"implemented":true,"settings":g.settings,"reading":g.last,"notice":g.notice,
        "frozen":g.frozen.iter().map(|f|json!({"root_pid":f.root,"since":f.at,"pids":f.members.iter().map(|r|r.process.pid).collect::<Vec<_>>()})).collect::<Vec<_>>(),"requests":g.requests.values().collect::<Vec<_>>()})
    }
    fn on_event(&mut self, e: &Event, ctx: &ModuleCtx) -> Result<(), ApiError> {
        let mut g = lock(&self.shared);
        if e.topic == "power.overheat" {
            g.thermal = e.data["level"].as_u64().unwrap_or(0) >= 2;
        }
        // A recoverable thaw failure must not fault the whole module and stop its retries.
        if let Err(e) = step(&mut g, ctx) {
            let _ = ctx.publish("notice", json!({"reason":"recovery_pending","message":e.message}));
        }
        Ok(())
    }
    fn recover(&mut self, e: &JournalEntry, ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        if e.key.starts_with("kill:") {
            return Ok(RecoverOutcome::Restored);
        }
        if !e.key.starts_with("frozen:") {
            return Err(ApiError::internal("unknown resource journal key"));
        }
        let p: ProcessInfo = serde_json::from_value(e.original["process"].clone())
            .map_err(|e| ApiError::internal(e.to_string()))?;
        let threads: Vec<ThreadState> = serde_json::from_value(e.original["threads"].clone())
            .map_err(|e| ApiError::internal(e.to_string()))?;
        if live(ctx, &p)? {
            ctx.system().thaw_process(p.pid, p.started_at, &threads)?;
        }
        ctx.arbitration().frozen.remove(&p.pid);
        ctx.arbitration().resource_frozen.remove(&p.pid);
        Ok(RecoverOutcome::Restored)
    }
}
