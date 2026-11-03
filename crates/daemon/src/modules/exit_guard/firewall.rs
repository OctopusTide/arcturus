//! Journal and reconcile only this module's own per-program firewall policies.
use super::config::{Settings, path_key};
use crate::sys::{
    exit_firewall::{ExitFirewallPolicy, ExitRoute},
    is_needs_elevation,
};
use crate::{JournalEntry, ModuleCtx};
use proto::ApiError;
use serde_json::{Value, json};

pub fn reconcile(ctx: &ModuleCtx, settings: &Settings) -> Result<Value, ApiError> {
    let mut programs = Vec::new();
    // Removal is replayable after a crash between saving settings and applying them.
    for entry in ctx.journal().open_entries()? {
        if let Some(path) = entry.key.strip_prefix("firewall:") {
            if !settings
                .programs
                .iter()
                .any(|p| path_key(&p.path).ok().as_deref() == Some(path))
            {
                restore(ctx, &entry)?;
                ctx.journal().mark_restored(entry.id)?;
            }
        }
    }
    for program in &settings.programs {
        let path = path_key(&program.path).map_err(ApiError::invalid_args)?;
        let route = program
            .firewall_route
            .as_ref()
            .or(settings.firewall_route.as_ref())
            .cloned()
            .unwrap_or(ExitRoute::DenyAll);
        let mut blocked = true;
        let result = (|| {
            let current = ctx.elevated().exit_policy(&path)?;
            blocked = current
                .as_ref()
                .filter(|p| p.route == route)
                .map_or(true, |p| p.blocked);
            let desired = ExitFirewallPolicy {
                program: path.clone(),
                route: route.clone(),
                blocked,
            };
            ctx.journal().record(
                &format!("firewall:{path}"),
                &json!(current),
                Some(&json!(desired)),
            )?;
            // Never add a temporary deny-all lock merely because a check was requested.
            // The native backend updates changed VPN LUIDs atomically and skips identical filters.
            ctx.elevated().set_exit_policy(&desired)?;
            Ok::<(), ApiError>(())
        })();
        programs.push(match result {
            Ok(())=>json!({"path":program.path,"state":if route==ExitRoute::DenyAll{"needs_configuration"}else{"applied"},"applied":true,"blocked":blocked,"route":route}),
            Err(e)=>json!({"path":program.path,"state":if is_needs_elevation(&e){"needs_admin"}else{"failed"},"applied":false,"message":e.message}),
        });
    }
    let all = programs.iter().all(|p| p["applied"] == true);
    let state = if programs.is_empty() {
        "idle"
    } else if programs.iter().all(|p| p["state"] == "needs_admin") {
        "needs_admin"
    } else if all {
        "applied"
    } else {
        "partial"
    };
    Ok(json!({"state":state,"applied":!programs.is_empty()&&all,"programs":programs}))
}
pub fn finish(ctx: &ModuleCtx, program: &str, valid: bool) -> Result<Value, ApiError> {
    let path = path_key(program).map_err(ApiError::invalid_args)?;
    let policy = ctx
        .elevated()
        .exit_policy(&path)?
        .ok_or_else(|| ApiError::internal("egress firewall policy is missing"))?;
    let blocked = !valid || policy.route == ExitRoute::DenyAll;
    if policy.blocked != blocked {
        ctx.elevated().set_exit_blocked(&path, blocked)?;
    }
    Ok(
        json!({"state":if policy.route==ExitRoute::DenyAll{"needs_configuration"}else{"applied"},"blocked":blocked,"applied":true,"route":policy.route}),
    )
}
pub fn restore(ctx: &ModuleCtx, entry: &JournalEntry) -> Result<(), ApiError> {
    let path = entry
        .key
        .strip_prefix("firewall:")
        .ok_or_else(|| ApiError::internal("not a firewall journal entry"))?;
    // All helper policies belong to this product. Explicit removal/disable means
    // removing its filters, not resurrecting a stale blocked policy from a prior run.
    ctx.elevated().remove_exit_policy(path)
}
/// Stopping the daemon must not open a direct route. Persist the temporary safety lock.
pub fn lock_on_stop(ctx: &ModuleCtx) -> Result<(), ApiError> {
    let mut last = None;
    for entry in ctx.journal().open_entries()? {
        if let Some(path) = entry.key.strip_prefix("firewall:") {
            if let Err(e) = ctx.elevated().set_exit_blocked(path, true) {
                last = Some(e);
            }
        }
    }
    last.map_or(Ok(()), Err)
}
