//! Reuses the shared sys process operations. No native process code lives in this module.
use super::config::Action;
use crate::sys::{ProcessInfo, ThreadState, is_access_denied, needs_elevation, unsupported};
use crate::{JournalEntry, ModuleCtx};
use proto::{ApiError, ErrorCode};
use serde_json::{Value, json};
use std::collections::BTreeSet;
pub trait Actions: Send + Sync {
    /// A full executable path; implementations must resolve and recheck process identity.
    fn apply(&self, ctx: &ModuleCtx, path: &str, action: Action) -> Result<Value, ApiError>;
}
/// Used when exercising the unavailable-backend path with fakes.
pub struct PendingActions;
impl Actions for PendingActions {
    fn apply(&self, _ctx: &ModuleCtx, _path: &str, action: Action) -> Result<Value, ApiError> {
        match action {
            Action::Block => Err(needs_elevation(
                "exit firewall: administrator service required",
            )),
            Action::Freeze => Err(unsupported(
                "freeze process: shared sys implementation not available",
            )),
            Action::Kill => Err(unsupported(
                "terminate process: ordinary-user sys implementation not available",
            )),
        }
    }
}

pub struct SystemActions;
impl Actions for SystemActions {
    fn apply(&self, ctx: &ModuleCtx, path: &str, action: Action) -> Result<Value, ApiError> {
        if action == Action::Block {
            let path = super::config::path_key(path).map_err(ApiError::invalid_args)?;
            ctx.elevated().set_exit_blocked(&path, true)?;
            return Ok(json!({"state":"applied","blocked":true}));
        }
        let key = super::config::path_key(path).map_err(ApiError::invalid_args)?;
        let mut safety = ctx.arbitration();
        if !safety.permission_ready || !safety.tasks_ready {
            return Err(ApiError::new(
                ErrorCode::Denied,
                "permission/task safety state unavailable; process action not applied",
            ));
        }
        let snapshot = ctx.system().processes()?;
        let mut protected = safety.protected.clone();
        let infos: Vec<_> = snapshot
            .iter()
            .filter_map(|p| ctx.system().process_info(p.pid).ok())
            .collect();
        loop {
            let before = protected.len();
            for p in &snapshot {
                if protected.contains(&p.parent_pid)
                    && infos
                        .iter()
                        .find(|i| i.pid == p.pid)
                        .zip(infos.iter().find(|i| i.pid == p.parent_pid))
                        .is_some_and(|(c, p)| c.started_at >= p.started_at)
                {
                    protected.insert(p.pid);
                }
            }
            if before == protected.len() {
                break;
            }
        }
        let mut rules = safety.rules.clone();
        for rule in ctx.system().program_rules()? {
            let Ok(normalized) = super::config::path_key(&rule.path) else {
                continue;
            };
            if rule.source == "builtin" || !safety.excluded_defaults.contains(&normalized) {
                rules.push(rule);
            }
        }
        let blocked_paths: BTreeSet<_> = rules
            .iter()
            .filter_map(|r| super::config::path_key(&r.path).ok())
            .collect();
        let targets: Vec<_> = infos
            .into_iter()
            .filter(|p| {
                p.exe
                    .as_deref()
                    .and_then(|p| super::config::path_key(p).ok())
                    .as_ref()
                    == Some(&key)
            })
            .collect();
        if targets.iter().any(|p| {
            p.pid <= 4
                || p.pid == std::process::id()
                || p.own_user != Some(true)
                || protected.contains(&p.pid)
                || blocked_paths.contains(&key)
        }) {
            return Err(ApiError::new(
                ErrorCode::Denied,
                "cannot-close allowlist, protected task or unknown/other user prevents this process action",
            ));
        }
        let mut outcomes = Vec::new();
        for p in &targets {
            if !same_process(ctx, p)? {
                continue;
            }
            if action == Action::Freeze {
                let journal_key = format!("frozen:{}:{}", p.pid, p.started_at);
                if ctx.journal().open_entry(&journal_key)?.is_some()
                    || safety.frozen.get(&p.pid) == Some(&p.started_at)
                {
                    outcomes.push(json!({"pid":p.pid,"state":"already_frozen"}));
                    continue;
                }
                let (threads, mut administrator) =
                    match ctx.system().suspension_state(p.pid, p.started_at) {
                        Ok(threads) => (threads, false),
                        Err(e) if is_access_denied(&e) => {
                            (ctx.elevated().suspension_state(p.pid, p.started_at)?, true)
                        }
                        Err(e) => return Err(e),
                    };
                let entry = ctx.journal().record(
                    &journal_key,
                    &json!({"process":p,"threads":threads}),
                    Some(&json!({"administrator": administrator})),
                )?;
                safety.frozen.insert(p.pid, p.started_at);
                let frozen = if administrator {
                    ctx.elevated().freeze_process(p.pid, p.started_at, &threads)
                } else {
                    match ctx.system().freeze_process(p.pid, p.started_at, &threads) {
                        Err(e) if is_access_denied(&e) => {
                            // Persist the owner before contacting the service, including a
                            // crash or lost reply between its durable backup and our result.
                            administrator = true;
                            ctx.journal().record(
                                &journal_key,
                                &entry.original,
                                Some(&json!({"administrator":true})),
                            )?;
                            ctx.elevated().freeze_process(p.pid, p.started_at, &threads)
                        }
                        result => result,
                    }
                };
                if let Err(e) = frozen {
                    // A partial freeze is undone; any failed rollback retains its journal.
                    if thaw(ctx, p, &threads, administrator).is_ok() {
                        ctx.journal().mark_restored(entry.id)?;
                        safety.frozen.remove(&p.pid);
                    }
                    return Err(e);
                }
                outcomes.push(json!({"pid":p.pid,"state":"frozen"}));
            } else {
                let entry = ctx.journal().record_with_note(
                    &format!("kill:{}:{}", p.pid, p.started_at),
                    &json!({"process":p}),
                    Some(&json!(true)),
                    Some("termination cannot be undone"),
                )?;
                ctx.system()
                    .terminate_process(p.pid, p.started_at)
                    .or_else(|e| {
                        if is_access_denied(&e) {
                            ctx.elevated()
                                .terminate_process_checked(p.pid, p.started_at)
                        } else {
                            Err(e)
                        }
                    })?;
                ctx.journal().mark_restored(entry.id)?;
                if let Some(frozen) = ctx
                    .journal()
                    .open_entry(&format!("frozen:{}:{}", p.pid, p.started_at))?
                {
                    ctx.journal().mark_restored(frozen.id)?;
                    if safety.frozen.get(&p.pid) == Some(&p.started_at) {
                        safety.frozen.remove(&p.pid);
                    }
                }
                outcomes.push(json!({"pid":p.pid,"state":"killed"}));
            }
        }
        Ok(
            json!({"state":if outcomes.is_empty(){"not_running"}else{"applied"},"processes":outcomes}),
        )
    }
}

fn same_process(ctx: &ModuleCtx, p: &ProcessInfo) -> Result<bool, ApiError> {
    match ctx.system().process_info(p.pid) {
        Ok(now) if now.started_at != p.started_at => Ok(false),
        Ok(now) if now.exe.is_some() && now.exe == p.exe => Ok(true),
        Ok(_) => Err(ApiError::internal(
            "process image is unavailable or changed; recovery receipt retained",
        )),
        Err(e) if e.code == ErrorCode::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
pub fn recover(ctx: &ModuleCtx, entry: &JournalEntry) -> Result<(), ApiError> {
    if entry.key.starts_with("kill:") {
        return Ok(());
    }
    if !entry.key.starts_with("frozen:") {
        return Err(ApiError::internal("unknown egress journal key"));
    }
    let p: ProcessInfo = serde_json::from_value(entry.original["process"].clone())
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let threads: Vec<ThreadState> = serde_json::from_value(entry.original["threads"].clone())
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let mut safety = ctx.arbitration();
    if same_process(ctx, &p)? {
        let administrator = entry
            .applied
            .as_ref()
            .and_then(|v| v["administrator"].as_bool())
            .unwrap_or(false);
        thaw(ctx, &p, &threads, administrator)?;
    }
    if safety.frozen.get(&p.pid) == Some(&p.started_at) {
        safety.frozen.remove(&p.pid);
    }
    Ok(())
}
fn thaw(
    ctx: &ModuleCtx,
    p: &ProcessInfo,
    threads: &[ThreadState],
    administrator: bool,
) -> Result<(), ApiError> {
    if administrator {
        return ctx.elevated().thaw_process(p.pid, p.started_at, threads);
    }
    ctx.system()
        .thaw_process(p.pid, p.started_at, threads)
        .or_else(|e| {
            if is_access_denied(&e) {
                ctx.elevated().thaw_process(p.pid, p.started_at, threads)
            } else {
                Err(e)
            }
        })
}
/// Release only this module's suspensions for a recovered/removed full executable path.
pub fn release(ctx: &ModuleCtx, path: Option<&str>) -> Result<(), ApiError> {
    let mut error = None;
    for entry in ctx.journal().open_entries()? {
        if entry.key.starts_with("firewall:") {
            continue;
        }
        let matches = path.is_none_or(|path| {
            entry.original["process"]["exe"]
                .as_str()
                .and_then(|p| super::config::path_key(p).ok())
                == super::config::path_key(path).ok()
        });
        if matches {
            if let Err(e) = recover(ctx, &entry).and_then(|_| ctx.journal().mark_restored(entry.id))
            {
                error = Some(e);
            }
        }
    }
    error.map_or(Ok(()), Err)
}

/// New task/allowlist protection releases our existing suspension without a public query.
pub fn release_protected(ctx: &ModuleCtx) -> Result<(), ApiError> {
    let mut error = None;
    for entry in ctx.journal().open_entries()? {
        if !entry.key.starts_with("frozen:") {
            continue;
        }
        let p: ProcessInfo = serde_json::from_value(entry.original["process"].clone())
            .map_err(|e| ApiError::internal(e.to_string()))?;
        let protected = {
            let safety = ctx.arbitration();
            let key = p
                .exe
                .as_deref()
                .and_then(|p| super::config::path_key(p).ok());
            !safety.permission_ready
                || !safety.tasks_ready
                || safety.protected.contains(&p.pid)
                || safety
                    .rules
                    .iter()
                    .any(|r| super::config::path_key(&r.path).ok() == key)
        };
        if protected {
            if let Err(e) = recover(ctx, &entry).and_then(|_| ctx.journal().mark_restored(entry.id))
            {
                error = Some(e);
            }
        }
    }
    error.map_or(Ok(()), Err)
}
