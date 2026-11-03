//! 任务 (task): protected process trees.
//!
//! A task is a process tree (followed by the OS, see [`tree`]) plus dependencies outside it
//! ([`deps`]). It stays protected while any of its processes runs and something happens:
//! CPU, disk or network I/O, GPU, a conversation transcript being written, or its
//! dependencies working. After `idle_minutes` with none of that (and past any `renew`), it
//! counts as stuck and its protection ends. Anyone may end a protection; every end records
//! who and why.
//!
//! `prioritize` raises the OS priority of the tree, at most to high (never real-time),
//! journaled per process and put back when the protection ends.
//!
//! Other modules learn about tasks only from events. Every `task.*` event carries `active`
//! (how many tasks are protected now); `task.snapshot` carries every task's processes and
//! dependencies, for the resource and permission modules.

mod deps;
mod tree;

use std::collections::BTreeMap;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use proto::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::dlog;
use crate::module::{
    ArgKind, ArgSpec, Caller, CommandSpec, ErrorSpec, Event, EventSpec, Migration, Module,
    ModuleCtx, ModuleRequest, ModuleSpec, RecoverOutcome, Section,
};
use crate::modules::{NEEDS_ADMIN_NOTICE, settings};
use crate::store::JournalEntry;
use crate::sys::{ProcessPriority, is_access_denied, is_needs_elevation};
use deps::Dep;
use tree::{Others, Tree};

static TASK_ID: ArgSpec = ArgSpec::new("task_id", ArgKind::Int, "Task id from `tasks`")
    .required()
    .positional();
static TASK_ID_OPT: ArgSpec =
    ArgSpec::new("task_id", ArgKind::Int, "Task id from `tasks`").positional();
static SESSION_ID: ArgSpec =
    ArgSpec::new("session_id", ArgKind::Str, "Agent conversation id (hooks)");

static SPEC: ModuleSpec = ModuleSpec::new("task", Section::Usage, "Protected process trees")
    .commands(&[
        CommandSpec::new("list", "List protected tasks")
            .short("tasks")
            .agent()
            .read_only()
            .args(&[ArgSpec::new(
                "all",
                ArgKind::Bool,
                "Also the 20 most recently ended",
            )]),
        CommandSpec::new("protect", "Protect a process tree")
            .short("protect")
            .agent()
            .args(&[
                ArgSpec::new("pid", ArgKind::Uint, "Root process id"),
                ArgSpec::new(
                    "ancestor",
                    ArgKind::Uint,
                    "Instead of --pid: the caller's n-th parent (1 = the shell that ran this)",
                ),
                ArgSpec::new("label", ArgKind::Str, "Name shown in the task list"),
                ArgSpec::new(
                    "ttl_minutes",
                    ArgKind::Uint,
                    "Stay protected at least this long even without activity",
                ),
                ArgSpec::new(
                    "deps",
                    ArgKind::StrList,
                    "Dependency outside the tree, repeatable (wsl, docker, postgres.exe …)",
                ),
                ArgSpec::new("mem_limit_mb", ArgKind::Uint, "The task's own memory limit"),
                ArgSpec::new(
                    "transcript",
                    ArgKind::Str,
                    "Conversation transcript file; writing it counts as activity",
                ),
                SESSION_ID,
                ArgSpec::new(
                    "source",
                    ArgKind::Str,
                    "cli (default) or hook (automatic registration of conversations)",
                ),
            ]),
        CommandSpec::new("renew", "Extend a task's protection")
            .short("renew")
            .agent()
            .args(&[
                TASK_ID,
                ArgSpec::new(
                    "ttl_minutes",
                    ArgKind::Uint,
                    "Stay protected at least this long (default: the idle limit)",
                ),
            ]),
        CommandSpec::new("unprotect", "Stop protecting a task")
            .short("unprotect")
            .agent()
            .args(&[TASK_ID_OPT, SESSION_ID]),
        CommandSpec::new(
            "prioritize",
            "Let a task go first; raises its OS priority (at most high)",
        )
        .short("prioritize")
        .agent()
        .args(&[
            TASK_ID,
            ArgSpec::new(
                "level",
                ArgKind::Str,
                "high (default), above_normal, or normal to undo",
            ),
        ]),
        CommandSpec::new("check", "Check every task's activity now").agent(),
        CommandSpec::new("settings", "Task settings")
            .agent()
            .read_only(),
        CommandSpec::new(
            "configure",
            "Change task settings (idle limit, auto-registration …)",
        )
        .args(&[ArgSpec::new(
            "settings",
            ArgKind::Json,
            "Partial settings object, e.g. {\"idle_minutes\":15}",
        )
        .required()]),
    ])
    .publishes(&[
        EventSpec::new("protected", "A task is now protected"),
        EventSpec::new("unprotected", "A task's protection ended (who, why)"),
        EventSpec::new("renewed", "A task's protection was extended"),
        EventSpec::new("prioritized", "A task's priority changed"),
        EventSpec::new("active", "How many tasks are protected (sent at startup)"),
        EventSpec::new(
            "snapshot",
            "Every task's processes, dependencies and memory",
        ),
    ])
    .subscribes(&["core.started", "resume.continued"])
    .migrations(&[
        Migration::new(
            1,
            "CREATE TABLE task_protected (
                id               INTEGER PRIMARY KEY,
                label            TEXT,
                root_pid         INTEGER NOT NULL,
                root_exe         TEXT,
                root_created_at  INTEGER,
                deps             TEXT    NOT NULL DEFAULT '[]',
                priority         INTEGER NOT NULL DEFAULT 0,
                mem_limit_mb     INTEGER,
                status           TEXT    NOT NULL DEFAULT 'active',
                created_at       INTEGER NOT NULL,
                expires_at       INTEGER,
                ended_at         INTEGER,
                requested_by_pid INTEGER,
                requested_by_exe TEXT
            );
            CREATE INDEX task_protected_status ON task_protected (status);",
        ),
        // The first step was a draft; these complete it. `expires_at` is "quiet until"
        // (renew), `priority` 0 none, 1 above normal, 2 high.
        Migration::new(
            2,
            "ALTER TABLE task_protected ADD COLUMN source TEXT NOT NULL DEFAULT 'cli';
             ALTER TABLE task_protected ADD COLUMN session_id TEXT;
             ALTER TABLE task_protected ADD COLUMN transcript TEXT;
             ALTER TABLE task_protected ADD COLUMN members TEXT NOT NULL DEFAULT '[]';
             ALTER TABLE task_protected ADD COLUMN last_active_at INTEGER;
             ALTER TABLE task_protected ADD COLUMN end_reason TEXT;
             ALTER TABLE task_protected ADD COLUMN ended_by TEXT;
             ALTER TABLE task_protected ADD COLUMN requested_by_name TEXT;
             ALTER TABLE task_protected ADD COLUMN requested_by_kind TEXT;
             CREATE INDEX task_protected_session ON task_protected (session_id);",
        ),
    ])
    .error_codes(&[
        ErrorSpec::new(
            "NO_SUCH_TASK",
            ErrorCode::NotFound,
            "No protected task with that id or session",
        ),
        ErrorSpec::new(
            "NO_SUCH_PROCESS",
            ErrorCode::NotFound,
            "The process does not exist (any more)",
        ),
        ErrorSpec::new(
            "BAD_TARGET",
            ErrorCode::InvalidArgs,
            "Say which process or task",
        ),
        ErrorSpec::new(
            "BAD_LEVEL",
            ErrorCode::InvalidArgs,
            "Level must be high, above_normal or normal",
        ),
        ErrorSpec::new(
            "TTL_TOO_LONG",
            ErrorCode::InvalidArgs,
            "Longer than renew_max_minutes",
        ),
    ]);

// ---- settings ----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskSettings {
    pub check_seconds: u32,
    /// No activity for this long ends the protection.
    pub idle_minutes: u32,
    /// CPU use (percent of one core, averaged between checks) that counts as activity.
    pub cpu_percent_min: f64,
    /// Disk and network I/O between checks that counts as activity.
    pub io_kb_min: u64,
    pub gpu_percent_min: f64,
    /// Longest `ttl_minutes` for `protect` and `renew`.
    pub renew_max_minutes: u32,
    /// Agent conversations register themselves through a hook (`protect --source hook`).
    pub auto_register_conversations: bool,
}

impl Default for TaskSettings {
    fn default() -> Self {
        TaskSettings {
            check_seconds: 30,
            idle_minutes: 10,
            cpu_percent_min: 1.0,
            io_kb_min: 64,
            gpu_percent_min: 1.0,
            renew_max_minutes: 1440,
            auto_register_conversations: false,
        }
    }
}

impl TaskSettings {
    fn check(&self) -> Result<(), String> {
        if !(5..=600).contains(&self.check_seconds) {
            return Err("check_seconds must be 5..=600".into());
        }
        if !(1..=1440).contains(&self.idle_minutes) {
            return Err("idle_minutes must be 1..=1440".into());
        }
        if !(0.0..=100.0).contains(&self.cpu_percent_min)
            || !(0.0..=100.0).contains(&self.gpu_percent_min)
        {
            return Err("cpu_percent_min and gpu_percent_min must be 0..=100".into());
        }
        if !(1..=10_080).contains(&self.renew_max_minutes) {
            return Err("renew_max_minutes must be 1..=10080".into());
        }
        Ok(())
    }
}

// ---- state -------------------------------------------------------------------------------

/// One raised process priority.
struct Raised {
    /// Open journal entry; `None` when nothing was changed.
    entry: Option<i64>,
    started_at: i64,
    original: Option<ProcessPriority>,
    state: &'static str,
    detail: String,
}

struct Task {
    id: i64,
    label: String,
    root_pid: u32,
    root_exe: Option<String>,
    source: String,
    session_id: Option<String>,
    transcript: Option<String>,
    deps: Vec<Dep>,
    dep_pids: BTreeMap<String, Vec<u32>>,
    level: Option<ProcessPriority>,
    mem_limit_mb: Option<u64>,
    created_at: i64,
    quiet_until: i64,
    last_active_at: i64,
    last_activity: Vec<&'static str>,
    transcript_seen: Option<i64>,
    requested_by: Value,
    tree: Tree,
    others: Others,
    raised: BTreeMap<u32, Raised>,
}

fn level_rank(level: Option<ProcessPriority>) -> i64 {
    match level {
        Some(ProcessPriority::High) => 2,
        Some(ProcessPriority::AboveNormal) => 1,
        _ => 0,
    }
}

fn level_from_rank(rank: i64) -> Option<ProcessPriority> {
    match rank {
        2 => Some(ProcessPriority::High),
        1 => Some(ProcessPriority::AboveNormal),
        _ => None,
    }
}

fn level_name(level: Option<ProcessPriority>) -> &'static str {
    match level {
        Some(ProcessPriority::High) => "high",
        Some(ProcessPriority::AboveNormal) => "above_normal",
        _ => "normal",
    }
}

fn caller_json(c: &Caller) -> Value {
    json!({ "kind": c.kind.as_str(), "pid": c.pid, "exe": c.exe, "name": c.name })
}

fn daemon_by() -> Value {
    json!({ "kind": "daemon" })
}

fn members_json(m: &BTreeMap<u32, i64>) -> String {
    serde_json::to_string(&m.iter().map(|(p, a)| json!([p, a])).collect::<Vec<_>>())
        .unwrap_or_else(|_| "[]".into())
}

fn parse_members(s: &str) -> BTreeMap<u32, i64> {
    let v: Vec<(u32, i64)> = serde_json::from_str(s).unwrap_or_default();
    v.into_iter().collect()
}

impl Task {
    fn pids(&self) -> Vec<u32> {
        self.tree.members.keys().copied().collect()
    }

    fn dep_pid_list(&self) -> Vec<u32> {
        let mut all: Vec<u32> = self.dep_pids.values().flatten().copied().collect();
        all.sort_unstable();
        all.dedup();
        all
    }

    fn to_json(&self, now: i64, idle_ms: i64) -> Value {
        let ends_at = (self.last_active_at + idle_ms).max(self.quiet_until);
        json!({
            "task_id": self.id,
            "label": self.label,
            "root_pid": self.root_pid,
            "root_exe": self.root_exe,
            "source": self.source,
            "session_id": self.session_id,
            "transcript": self.transcript,
            "pids": self.pids(),
            "processes": self.tree.members.len(),
            "memory_mb": self.tree.memory_mb,
            "mem_limit_mb": self.mem_limit_mb,
            "over_mem_limit": self.mem_limit_mb.is_some_and(|l| self.tree.memory_mb > l),
            "deps": self.deps.iter().map(|d| json!({
                "name": d.name, "auto": d.auto, "pids": self.dep_pids.get(&d.name).cloned().unwrap_or_default(),
            })).collect::<Vec<_>>(),
            "tracking": if self.tree.walking().is_some() { "parent_ids" } else { "job" },
            "priority": level_name(self.level),
            "priority_changes": self.raised.iter().map(|(pid, r)| (pid.to_string(), json!({
                "state": r.state, "detail": r.detail,
            }))).collect::<serde_json::Map<_, _>>(),
            "created_at": self.created_at,
            "last_active_at": self.last_active_at,
            "last_activity": self.last_activity,
            "idle_for_s": ((now - self.last_active_at) / 1000).max(0),
            "quiet_until": self.quiet_until,
            "ends_if_idle_at": ends_at,
            "requested_by": self.requested_by,
        })
    }

    fn snapshot_json(&self) -> Value {
        json!({
            "task_id": self.id,
            "label": self.label,
            "priority": level_rank(self.level),
            "pids": self.pids(),
            "dep_pids": self.dep_pid_list(),
            "dependency_names": deps::executable_names(&self.deps),
            "given_deps": self.deps.iter().filter(|d| !d.auto).map(|d| d.name.as_str()).collect::<Vec<_>>(),
            "identities": self.tree.members,
            "memory_mb": self.tree.memory_mb,
            "mem_limit_mb": self.mem_limit_mb,
        })
    }
}

struct Inner {
    settings: TaskSettings,
    tasks: BTreeMap<i64, Task>,
    last_check: i64,
}

impl Inner {
    fn idle_ms(&self) -> i64 {
        i64::from(self.settings.idle_minutes) * 60_000
    }

    fn active(&self) -> usize {
        self.tasks.len()
    }

    fn publish(&self, ctx: &ModuleCtx, event: &str, mut data: Value) {
        data["active"] = json!(self.active());
        let _ = ctx.publish(event, data);
    }

    fn publish_snapshot(&self, ctx: &ModuleCtx) {
        let tasks: Vec<Value> = self.tasks.values().map(Task::snapshot_json).collect();
        {
            let mut a = ctx.arbitration();
            a.protected = tasks
                .iter()
                .flat_map(|t| {
                    t["pids"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .chain(t["dep_pids"].as_array().into_iter().flatten())
                        .filter_map(|p| p.as_u64().and_then(|p| u32::try_from(p).ok()))
                })
                .collect();
            a.tasks = tasks.clone();
            a.tasks_ready = true;
        }
        self.publish(ctx, "snapshot", json!({ "tasks": tasks }));
    }
}

enum Msg {
    Stop,
}

pub struct TaskModule {
    shared: Arc<Mutex<Inner>>,
    tx: Option<Sender<Msg>>,
    worker: Option<JoinHandle<()>>,
}

pub fn create() -> Box<dyn Module> {
    Box::new(TaskModule {
        shared: Arc::new(Mutex::new(Inner {
            settings: TaskSettings::default(),
            tasks: BTreeMap::new(),
            last_check: 0,
        })),
        tx: None,
        worker: None,
    })
}

fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---- priority ----------------------------------------------------------------------------

fn priority_key(pid: u32, started_at: i64) -> String {
    format!("priority:{pid}:{started_at}")
}

/// Sets a priority with the user's rights, or through the administrator layer when those
/// are not enough.
fn set_priority(ctx: &ModuleCtx, pid: u32, p: ProcessPriority) -> Result<&'static str, ApiError> {
    match ctx.system().set_process_priority(pid, p) {
        Ok(()) => Ok("applied"),
        Err(e) if is_access_denied(&e) => ctx
            .elevated()
            .set_process_priority(pid, p)
            .map(|()| "applied by the administrator layer"),
        Err(e) => Err(e),
    }
}

fn raise(ctx: &ModuleCtx, task: &mut Task, pid: u32, started_at: i64, level: ProcessPriority) {
    if task.raised.contains_key(&pid) {
        return;
    }
    let unchanged = |original, detail: &str| Raised {
        entry: None,
        started_at,
        original,
        state: "unchanged",
        detail: detail.into(),
    };
    let current = match ctx.system().process_priority(pid) {
        Ok(Some(c)) => c,
        Ok(None) => {
            task.raised
                .insert(pid, unchanged(None, "real-time: left as it is"));
            return;
        }
        Err(e) => {
            task.raised.insert(
                pid,
                Raised {
                    entry: None,
                    started_at,
                    original: None,
                    state: "failed",
                    detail: e.message,
                },
            );
            return;
        }
    };
    if current >= level {
        task.raised.insert(
            pid,
            unchanged(Some(current), "already at or above that priority"),
        );
        return;
    }
    let key = priority_key(pid, started_at);
    let entry = match ctx
        .journal()
        .record(&key, &json!(current), Some(&json!(level)))
    {
        Ok(e) => e,
        Err(e) => {
            task.raised.insert(
                pid,
                Raised {
                    entry: None,
                    started_at,
                    original: None,
                    state: "failed",
                    detail: e.message,
                },
            );
            return;
        }
    };
    let raised = match set_priority(ctx, pid, level) {
        Ok(how) => Raised {
            entry: Some(entry.id),
            started_at,
            original: Some(current),
            state: "applied",
            detail: how.into(),
        },
        Err(e) => {
            // Nothing changed: close the entry again.
            let _ = ctx.journal().mark_restored(entry.id);
            let (state, detail) = if is_needs_elevation(&e) {
                ("needs_admin", NEEDS_ADMIN_NOTICE.to_owned())
            } else {
                ("failed", e.message)
            };
            Raised {
                entry: None,
                started_at,
                original: Some(current),
                state,
                detail,
            }
        }
    };
    task.raised.insert(pid, raised);
}

/// Puts one process's priority back. False when it is still changed (retried at the next start).
fn restore(ctx: &ModuleCtx, pid: u32, r: &Raised) -> bool {
    let (Some(entry), Some(original)) = (r.entry, r.original) else {
        return true;
    };
    let same = ctx
        .system()
        .process_info(pid)
        .is_ok_and(|i| i.started_at == r.started_at);
    if same && let Err(e) = set_priority(ctx, pid, original) {
        dlog!(
            "WARN",
            "task: cannot put the priority of pid {pid} back: {}",
            e.message
        );
        return false;
    }
    let _ = ctx.journal().mark_restored(entry);
    true
}

fn restore_all(ctx: &ModuleCtx, task: &mut Task) {
    let raised = std::mem::take(&mut task.raised);
    for (pid, r) in &raised {
        restore(ctx, *pid, r);
    }
}

fn apply_priority(ctx: &ModuleCtx, task: &mut Task) {
    let Some(level) = task.level else { return };
    let members: Vec<(u32, i64)> = task.tree.members.iter().map(|(p, a)| (*p, *a)).collect();
    for (pid, at) in members {
        raise(ctx, task, pid, at, level);
    }
    // Processes that have exited need nothing put back.
    let gone: Vec<u32> = task
        .raised
        .keys()
        .copied()
        .filter(|p| !task.tree.members.contains_key(p))
        .collect();
    for pid in gone {
        if let Some(r) = task.raised.remove(&pid)
            && let Some(entry) = r.entry
        {
            let _ = ctx.journal().mark_restored(entry);
        }
    }
}

// ---- persistence -------------------------------------------------------------------------

const COLUMNS: &str = "id, label, root_pid, root_exe, deps, priority, mem_limit_mb, created_at, expires_at, \
     source, session_id, transcript, members, last_active_at, requested_by_pid, requested_by_exe, \
     requested_by_name, requested_by_kind";

fn save(ctx: &ModuleCtx, t: &Task) {
    let deps = serde_json::to_string(&t.deps).unwrap_or_else(|_| "[]".into());
    let r = ctx.db().execute(
        "UPDATE task_protected SET deps = ?, priority = ?, members = ?, last_active_at = ?, expires_at = ? WHERE id = ?",
        &[deps.into(), level_rank(t.level).into(), members_json(&t.tree.members).into(), t.last_active_at.into(), t.quiet_until.into(), t.id.into()],
    );
    if let Err(e) = r {
        dlog!("WARN", "task: cannot save task {}: {}", t.id, e.message);
    }
}

// ---- the work ----------------------------------------------------------------------------

/// Ends one protection: priorities back, tracking stopped (no process is ended), recorded.
fn end(g: &mut Inner, ctx: &ModuleCtx, id: i64, reason: &str, by: Value) -> Option<Value> {
    let mut task = g.tasks.remove(&id)?;
    restore_all(ctx, &mut task);
    let now = ctx.system().now_ms();
    let r = ctx.db().execute(
        "UPDATE task_protected SET status = 'ended', ended_at = ?, end_reason = ?, ended_by = ?, members = ? WHERE id = ?",
        &[now.into(), reason.into(), by.to_string().into(), members_json(&task.tree.members).into(), id.into()],
    );
    if let Err(e) = r {
        dlog!(
            "WARN",
            "task: cannot record the end of task {id}: {}",
            e.message
        );
    }
    dlog!(
        "INFO",
        "task: protection of #{id} {} ended ({reason})",
        task.label
    );
    let data = json!({ "task_id": id, "label": task.label, "root_pid": task.root_pid, "reason": reason, "by": by, "at": now });
    g.publish(ctx, "unprotected", data.clone());
    Some(data)
}

/// One activity check of every task.
fn check(g: &mut Inner, ctx: &ModuleCtx) {
    let sys = ctx.system();
    let now = sys.now_ms();
    let elapsed = (now - g.last_check).clamp(1000, i64::MAX);
    g.last_check = now;
    let procs = sys.processes().unwrap_or_default();
    let names: BTreeMap<u32, &str> = procs.iter().map(|p| (p.pid, p.name.as_str())).collect();
    let gpu = sys.gpu_usage().unwrap_or_default();
    let s = g.settings.clone();
    let cpu_min = s.cpu_percent_min / 100.0 * elapsed as f64;
    let io_min = s.io_kb_min * 1024;
    let idle_ms = g.idle_ms();
    let mut changed = false;
    let mut ended = Vec::new();
    for task in g.tasks.values_mut() {
        let sample = task.tree.sample(sys, &procs);
        if task.tree.members.is_empty() {
            ended.push((task.id, "exited"));
            continue;
        }
        for d in deps::detect(
            task.tree
                .members
                .keys()
                .filter_map(|p| names.get(p).copied()),
        ) {
            changed |= deps::add(&mut task.deps, d, true);
        }
        let dep_pids = deps::resolve(&task.deps, &procs);
        changed |= dep_pids != task.dep_pids;
        task.dep_pids = dep_pids;
        let dep_sample = task.others.sample(sys, &task.dep_pid_list());
        let mut activity = Vec::new();
        if sample.members_changed {
            activity.push("processes");
            changed = true;
        }
        if sample.cpu_ms as f64 >= cpu_min {
            activity.push("cpu");
        }
        if sample.io_bytes >= io_min {
            activity.push("io");
        }
        let watched = task.pids().into_iter().chain(task.dep_pid_list());
        if watched
            .into_iter()
            .any(|p| gpu.get(&p).is_some_and(|u| *u >= s.gpu_percent_min))
        {
            activity.push("gpu");
        }
        if dep_sample.cpu_ms as f64 >= cpu_min || dep_sample.io_bytes >= io_min {
            activity.push("dependencies");
        }
        if let Some(path) = &task.transcript {
            let modified = sys.file_modified_ms(path).ok().flatten();
            if modified.is_some() && modified > task.transcript_seen {
                activity.push("transcript");
            }
            task.transcript_seen = modified.max(task.transcript_seen);
        }
        if !activity.is_empty() {
            task.last_active_at = now;
        }
        task.last_activity = activity;
        apply_priority(ctx, task);
        if now - task.last_active_at >= idle_ms && now >= task.quiet_until {
            ended.push((task.id, "idle"));
            continue;
        }
        save(ctx, task);
    }
    for (id, reason) in ended {
        if reason == "idle" {
            dlog!(
                "WARN",
                "task: #{id} did nothing for {} minutes: treated as stuck",
                s.idle_minutes
            );
        }
        end(g, ctx, id, reason, daemon_by());
        changed = true;
    }
    if changed {
        g.publish_snapshot(ctx);
    }
}

/// The caller's `n`-th parent: 1 is the program that ran the CLI.
fn ancestor(ctx: &ModuleCtx, caller: Option<u32>, n: u64) -> Result<u32, ApiError> {
    let sys = ctx.system();
    let mut pid =
        caller.ok_or_else(|| ctx.error("BAD_TARGET", "the caller's process id is not known"))?;
    let parents: BTreeMap<u32, u32> = sys
        .processes()?
        .into_iter()
        .map(|p| (p.pid, p.parent_pid))
        .collect();
    let mut at = sys
        .process_info(pid)
        .map(|i| i.started_at)
        .unwrap_or(i64::MAX);
    for step in 1..=n {
        let broken = || {
            ctx.error(
                "NO_SUCH_PROCESS",
                format!("the caller has no running parent at level {step}"),
            )
        };
        let parent = *parents.get(&pid).ok_or_else(broken)?;
        let parent_at = sys.process_info(parent).map_err(|_| broken())?.started_at;
        // A parent always started first; otherwise its id was reused.
        if parent_at > at {
            return Err(broken());
        }
        (pid, at) = (parent, parent_at);
    }
    Ok(pid)
}

impl TaskModule {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        lock(&self.shared)
    }

    fn stop_worker(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Stop);
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }

    fn ttl_ms(&self, ctx: &ModuleCtx, args: &Value) -> Result<Option<i64>, ApiError> {
        let max = self.inner().settings.renew_max_minutes;
        match args["ttl_minutes"].as_u64() {
            None => Ok(None),
            Some(m) if m > u64::from(max) => Err(ctx.error(
                "TTL_TOO_LONG",
                format!("{m} minutes is over the limit of {max}"),
            )),
            Some(m) => Ok(Some(m as i64 * 60_000)),
        }
    }

    fn protect(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        let a = req.args;
        let source = a["source"].as_str().unwrap_or("cli");
        if !["cli", "hook"].contains(&source) {
            return Err(ApiError::invalid_args("source must be cli or hook"));
        }
        if source == "hook" && !self.inner().settings.auto_register_conversations {
            return Ok(json!({
                "registered": false,
                "reason": "automatic registration of conversations is off (setting auto_register_conversations)",
            }));
        }
        let ttl = self.ttl_ms(ctx, a)?;
        let pid = match (a["pid"].as_u64(), a["ancestor"].as_u64()) {
            (Some(p), None) => u32::try_from(p)
                .map_err(|_| ctx.error("NO_SUCH_PROCESS", format!("no process {p}")))?,
            (None, Some(n)) if n >= 1 => ancestor(ctx, req.caller.pid, n)?,
            _ => {
                return Err(ctx.error(
                    "BAD_TARGET",
                    "give --pid or --ancestor (at least 1), not both",
                ));
            }
        };
        if pid == 0 {
            return Err(ctx.error("BAD_TARGET", "pid 0 is not a program"));
        }
        let sys = ctx.system();
        let info = sys
            .process_info(pid)
            .map_err(|e| ctx.error("NO_SUCH_PROCESS", format!("process {pid}: {}", e.message)))?;
        let session = a["session_id"].as_str().map(str::to_owned);
        let now = sys.now_ms();
        let mut g = self.inner();
        let idle_ms = g.idle_ms();
        // Protecting the same process (or conversation) again extends the protection.
        let existing = g.tasks.values_mut().find(|t| {
            t.tree.members.get(&pid) == Some(&info.started_at)
                || (session.is_some() && t.session_id == session)
        });
        if let Some(t) = existing {
            for d in a["deps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                deps::add(&mut t.deps, d, false);
            }
            t.quiet_until = t.quiet_until.max(now + ttl.unwrap_or(0));
            t.last_active_at = now;
            if let Some(l) = a["mem_limit_mb"].as_u64() {
                t.mem_limit_mb = Some(l);
            }
            save(ctx, t);
            let result =
                json!({ "registered": true, "already": true, "task": t.to_json(now, idle_ms) });
            g.publish_snapshot(ctx);
            return Ok(result);
        }
        let known = BTreeMap::from([(pid, info.started_at)]);
        let tree = Tree::start(sys, pid, &known);
        let label = a["label"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                info.exe
                    .as_deref()
                    .and_then(|e| e.rsplit(['\\', '/']).next())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| format!("pid {pid}"));
        let mut task = Task {
            id: 0,
            label,
            root_pid: pid,
            root_exe: info.exe.clone(),
            source: source.into(),
            session_id: session,
            transcript: a["transcript"].as_str().map(str::to_owned),
            deps: Vec::new(),
            dep_pids: BTreeMap::new(),
            level: None,
            mem_limit_mb: a["mem_limit_mb"].as_u64(),
            created_at: now,
            quiet_until: now + ttl.unwrap_or(0),
            last_active_at: now,
            last_activity: Vec::new(),
            transcript_seen: None,
            requested_by: caller_json(req.caller),
            tree,
            others: Others::default(),
            raised: BTreeMap::new(),
        };
        for d in a["deps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            deps::add(&mut task.deps, d, false);
        }
        if let Some(path) = &task.transcript {
            task.transcript_seen = sys.file_modified_ms(path).ok().flatten();
        }
        let procs = sys.processes().unwrap_or_default();
        let names: BTreeMap<u32, &str> = procs.iter().map(|p| (p.pid, p.name.as_str())).collect();
        for d in deps::detect(
            task.tree
                .members
                .keys()
                .filter_map(|p| names.get(p).copied()),
        ) {
            deps::add(&mut task.deps, d, true);
        }
        task.dep_pids = deps::resolve(&task.deps, &procs);
        task.others.sample(sys, &task.dep_pid_list());
        let c = req.caller;
        task.id = ctx.db().transaction(|conn| {
            conn.execute(
                "INSERT INTO task_protected (label, root_pid, root_exe, root_created_at, deps, mem_limit_mb, created_at, \
                 expires_at, source, session_id, transcript, members, last_active_at, requested_by_pid, requested_by_exe, \
                 requested_by_name, requested_by_kind) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &[
                    task.label.as_str().into(),
                    pid.into(),
                    task.root_exe.clone().into(),
                    info.started_at.into(),
                    serde_json::to_string(&task.deps).unwrap_or_else(|_| "[]".into()).into(),
                    task.mem_limit_mb.map(|m| m as i64).into(),
                    now.into(),
                    task.quiet_until.into(),
                    source.into(),
                    task.session_id.clone().into(),
                    task.transcript.clone().into(),
                    members_json(&task.tree.members).into(),
                    now.into(),
                    c.pid.into(),
                    c.exe.clone().into(),
                    c.name.clone().into(),
                    c.kind.as_str().into(),
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })?;
        dlog!(
            "INFO",
            "task: protecting #{} {} (pid {pid}, {} processes)",
            task.id,
            task.label,
            task.tree.members.len()
        );
        let result =
            json!({ "registered": true, "already": false, "task": task.to_json(now, idle_ms) });
        let event = json!({ "task_id": task.id, "label": task.label, "root_pid": pid, "source": source, "by": caller_json(c) });
        g.tasks.insert(task.id, task);
        g.publish(ctx, "protected", event);
        g.publish_snapshot(ctx);
        Ok(result)
    }

    fn find_id(&self, ctx: &ModuleCtx, args: &Value) -> Result<Vec<i64>, ApiError> {
        let g = self.inner();
        match (args["task_id"].as_i64(), args["session_id"].as_str()) {
            (Some(id), _) if g.tasks.contains_key(&id) => Ok(vec![id]),
            (Some(id), _) => Err(ctx.error("NO_SUCH_TASK", format!("no protected task #{id}"))),
            (None, Some(s)) => Ok(g
                .tasks
                .values()
                .filter(|t| t.session_id.as_deref() == Some(s))
                .map(|t| t.id)
                .collect()),
            (None, None) => Err(ctx.error("BAD_TARGET", "give a task id or --session-id")),
        }
    }

    fn reload(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        let rows = ctx.db().query(
            &format!("SELECT {COLUMNS} FROM task_protected WHERE status = 'active' ORDER BY id"),
            &[],
        )?;
        let sys = ctx.system();
        let now = sys.now_ms();
        let mut g = self.inner();
        for r in rows {
            let parse = || -> crate::sqlite::Result<(i64, Task)> {
                let id = r.int(0)?;
                let mut known = parse_members(&r.text(12)?);
                let root_pid = r.int(2)? as u32;
                if known.is_empty() {
                    known.insert(root_pid, i64::MIN);
                }
                let tree = Tree::start(sys, root_pid, &known);
                let task = Task {
                    id,
                    label: r.opt_text(1)?.unwrap_or_default(),
                    root_pid,
                    root_exe: r.opt_text(3)?,
                    deps: serde_json::from_str(&r.text(4)?).unwrap_or_default(),
                    dep_pids: BTreeMap::new(),
                    level: level_from_rank(r.int(5)?),
                    mem_limit_mb: r.opt_int(6)?.map(|m| m as u64),
                    created_at: r.int(7)?,
                    quiet_until: r.opt_int(8)?.unwrap_or(0),
                    source: r.text(9)?,
                    session_id: r.opt_text(10)?,
                    transcript: r.opt_text(11)?,
                    // The daemon was not watching while it was down: the idle clock starts over.
                    last_active_at: now,
                    last_activity: Vec::new(),
                    transcript_seen: None,
                    requested_by: json!({
                        "pid": r.opt_int(14)?, "exe": r.opt_text(15)?, "name": r.opt_text(16)?, "kind": r.opt_text(17)?,
                    }),
                    tree,
                    others: Others::default(),
                    raised: BTreeMap::new(),
                };
                Ok((id, task))
            };
            match parse() {
                Ok((id, mut task)) => {
                    if task.tree.members.is_empty() {
                        g.tasks.insert(id, task);
                        end(
                            &mut g,
                            ctx,
                            id,
                            "exited",
                            json!({ "kind": "daemon", "note": "while the daemon was not running" }),
                        );
                        continue;
                    }
                    if let Some(path) = &task.transcript {
                        task.transcript_seen = sys.file_modified_ms(path).ok().flatten();
                    }
                    apply_priority(ctx, &mut task);
                    g.tasks.insert(id, task);
                }
                Err(e) => dlog!("WARN", "task: unreadable task row: {e}"),
            }
        }
        g.last_check = now;
        Ok(())
    }
}

impl Drop for TaskModule {
    fn drop(&mut self) {
        // Without a stop (crash, test teardown) the worker still ends. Tracking stops with it;
        // no process is ever ended.
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Stop);
        }
    }
}

impl Module for TaskModule {
    fn spec(&self) -> &'static ModuleSpec {
        &SPEC
    }

    fn start(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        let s: TaskSettings = settings::load(ctx)?;
        *self.inner() = Inner {
            settings: s,
            tasks: BTreeMap::new(),
            last_check: 0,
        };
        self.reload(ctx)?;
        {
            let g = self.inner();
            g.publish(ctx, "active", json!({}));
            g.publish_snapshot(ctx);
        }
        let (tx, rx) = mpsc::channel::<Msg>();
        let shared = self.shared.clone();
        let wctx = ctx.clone();
        let worker = std::thread::Builder::new()
            .name("task".into())
            .spawn(move || {
                loop {
                    if !wctx.is_active() {
                        break;
                    }
                    let wait = Duration::from_secs(u64::from(lock(&shared).settings.check_seconds));
                    match rx.recv_timeout(wait) {
                        Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) if wctx.is_active() => {
                            check(&mut lock(&shared), &wctx)
                        }
                        Err(RecvTimeoutError::Timeout) => break,
                    }
                }
            })
            .map_err(|e| ApiError::internal(format!("cannot start the task thread: {e}")))?;
        self.tx = Some(tx);
        self.worker = Some(worker);
        Ok(())
    }

    fn stop(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        self.stop_worker();
        // Protections survive a daemon restart (they are in the table); only the raised
        // priorities are put back now, and raised again on the next start.
        let mut g = self.inner();
        for task in g.tasks.values_mut() {
            restore_all(ctx, task);
        }
        Ok(())
    }

    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        let now = ctx.system().now_ms();
        match req.cmd {
            "list" => {
                let g = self.inner();
                let idle_ms = g.idle_ms();
                let tasks: Vec<Value> = g.tasks.values().map(|t| t.to_json(now, idle_ms)).collect();
                drop(g);
                let mut out = json!({ "tasks": tasks });
                if req.args["all"].as_bool() == Some(true) {
                    let rows = ctx.db().query(
                        "SELECT id, label, root_pid, created_at, ended_at, end_reason, ended_by FROM task_protected \
                         WHERE status = 'ended' ORDER BY ended_at DESC, id DESC LIMIT 20",
                        &[],
                    )?;
                    let ended: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            let by = r.opt_text(6).ok().flatten().and_then(|b| serde_json::from_str::<Value>(&b).ok());
                            json!({
                                "task_id": r.int(0).ok(), "label": r.opt_text(1).ok().flatten(), "root_pid": r.int(2).ok(),
                                "created_at": r.int(3).ok(), "ended_at": r.opt_int(4).ok().flatten(),
                                "reason": r.opt_text(5).ok().flatten(), "by": by,
                            })
                        })
                        .collect();
                    out["ended"] = json!(ended);
                }
                Ok(out)
            }
            "protect" => self.protect(req, ctx),
            "renew" => {
                let ids = self.find_id(ctx, req.args)?;
                let ttl = self.ttl_ms(ctx, req.args)?;
                let mut g = self.inner();
                let ttl = ttl.unwrap_or_else(|| g.idle_ms());
                let idle_ms = g.idle_ms();
                let t = g.tasks.get_mut(&ids[0]).expect("found above");
                t.quiet_until = t.quiet_until.max(now + ttl);
                t.last_active_at = now;
                save(ctx, t);
                let result = json!({ "task": t.to_json(now, idle_ms) });
                let event = json!({ "task_id": t.id, "until": t.quiet_until, "by": caller_json(req.caller) });
                g.publish(ctx, "renewed", event);
                Ok(result)
            }
            "unprotect" => {
                let ids = self.find_id(ctx, req.args)?;
                let mut g = self.inner();
                let ended: Vec<Value> = ids
                    .into_iter()
                    .filter_map(|id| end(&mut g, ctx, id, "cancelled", caller_json(req.caller)))
                    .collect();
                if !ended.is_empty() {
                    g.publish_snapshot(ctx);
                }
                Ok(json!({ "ended": ended }))
            }
            "prioritize" => {
                let ids = self.find_id(ctx, req.args)?;
                let level = match req.args["level"].as_str().unwrap_or("high") {
                    "high" => Some(ProcessPriority::High),
                    "above_normal" => Some(ProcessPriority::AboveNormal),
                    "normal" => None,
                    other => return Err(ctx.error("BAD_LEVEL", format!("unknown level '{other}'"))),
                };
                let mut g = self.inner();
                let idle_ms = g.idle_ms();
                let t = g.tasks.get_mut(&ids[0]).expect("found above");
                restore_all(ctx, t);
                t.level = level;
                apply_priority(ctx, t);
                save(ctx, t);
                let result = json!({ "task": t.to_json(now, idle_ms) });
                let event = json!({ "task_id": t.id, "level": level_name(level), "by": caller_json(req.caller) });
                g.publish(ctx, "prioritized", event);
                g.publish_snapshot(ctx);
                Ok(result)
            }
            "check" => {
                let mut g = self.inner();
                check(&mut g, ctx);
                let idle_ms = g.idle_ms();
                Ok(
                    json!({ "tasks": g.tasks.values().map(|t| t.to_json(now, idle_ms)).collect::<Vec<_>>() }),
                )
            }
            "settings" => Ok(json!({ "settings": self.inner().settings })),
            "configure" => {
                let next: TaskSettings =
                    settings::patch(ctx, &req.args["settings"], &[], TaskSettings::check)?;
                self.inner().settings = next.clone();
                Ok(json!({ "settings": next }))
            }
            other => Err(ApiError::internal(format!(
                "task has no handler for '{other}'"
            ))),
        }
    }

    fn on_event(&mut self, event: &Event, ctx: &ModuleCtx) -> Result<(), ApiError> {
        if event.topic == "core.started" {
            let g = self.inner();
            g.publish(ctx, "active", json!({}));
            g.publish_snapshot(ctx);
        } else if event.topic == "resume.continued" && event.data["protection"].is_object() {
            // The conversation was protected, so the process continuing it is too. One that
            // was not protected is not registered here: that stays the user's or agent's choice.
            let p = &event.data["protection"];
            let args = json!({ "pid": event.data["pid"], "label": p["label"], "deps": p["deps"], "mem_limit_mb": p["mem_limit_mb"] });
            let caller = Caller { name: Some("resume".into()), ..Caller::default() };
            let req = ModuleRequest { id: "", cmd: "protect", args: &args, caller: &caller };
            // It may already have ended; that is no fault of this module.
            if let Err(e) = self.protect(&req, ctx) {
                dlog!("WARN", "task: cannot protect the continuation: {}", e.message);
            }
        }
        Ok(())
    }

    fn status(&self, ctx: &ModuleCtx) -> Value {
        let g = self.inner();
        let now = ctx.system().now_ms();
        let idle_ms = g.idle_ms();
        let tasks: Vec<Value> = g.tasks.values().map(|t| t.to_json(now, idle_ms)).collect();
        let summary = if g.tasks.is_empty() {
            "no protected task".to_owned()
        } else {
            let each: Vec<String> = g
                .tasks
                .values()
                .map(|t| {
                    format!(
                        "#{} {} ({} processes, {} MB)",
                        t.id,
                        t.label,
                        t.tree.members.len(),
                        t.tree.memory_mb
                    )
                })
                .collect();
            format!("{} protected: {}", g.tasks.len(), each.join(", "))
        };
        let mut notices = Vec::new();
        for t in g.tasks.values() {
            if let Some(why) = t.tree.walking() {
                notices.push(format!("#{}: followed through parent ids, grandchildren of exited processes are missed ({why})", t.id));
            }
            for (pid, r) in &t.raised {
                if r.state == "needs_admin" || r.state == "failed" {
                    notices.push(format!("#{} priority of pid {pid}: {}", t.id, r.detail));
                }
            }
            if let Some(limit) = t.mem_limit_mb.filter(|l| t.tree.memory_mb > *l) {
                notices.push(format!(
                    "#{} uses {} MB, over its own limit of {limit} MB",
                    t.id, t.tree.memory_mb
                ));
            }
        }
        json!({
            "implemented": true,
            "active": g.tasks.len(),
            "tasks": tasks,
            "auto_register_conversations": g.settings.auto_register_conversations,
            "summary": summary,
            "notices": notices,
        })
    }

    fn recover(
        &mut self,
        entry: &JournalEntry,
        ctx: &ModuleCtx,
    ) -> Result<RecoverOutcome, ApiError> {
        let mut parts = entry.key.split(':');
        let (Some("priority"), Some(pid), Some(at)) = (parts.next(), parts.next(), parts.next())
        else {
            dlog!("WARN", "task: unknown journal key '{}'", entry.key);
            return Ok(RecoverOutcome::Restored);
        };
        let (Ok(pid), Ok(at)) = (pid.parse::<u32>(), at.parse::<i64>()) else {
            return Ok(RecoverOutcome::Restored);
        };
        let Ok(original) = serde_json::from_value::<ProcessPriority>(entry.original.clone()) else {
            return Ok(RecoverOutcome::Restored);
        };
        // The same process still running? Otherwise there is nothing to put back.
        if ctx
            .system()
            .process_info(pid)
            .is_ok_and(|i| i.started_at == at)
        {
            set_priority(ctx, pid, original)?;
        }
        Ok(RecoverOutcome::Restored)
    }
}
