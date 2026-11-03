//! Codex hooks: `<CODEX_HOME>\hooks.json` (same layout as Claude Code's `hooks` key). Codex
//! runs `command` through PowerShell on Windows, so it is one string; a program path with
//! spaces needs `& '<path>'`, which Codex has a bug with (openai/codex#57050).
//!
//! Stdin for `Stop`: `session_id`, `transcript_path` (may be null), `cwd`, `hook_event_name`,
//! `stop_hook_active`, ... There is no failure event: Codex does not say why a turn ended,
//! and `Stop` is not guaranteed to run when a turn fails (openai/codex#22774).
//! New or changed hooks must be trusted in Codex (`/hooks`) before they run.

use std::path::PathBuf;

use serde_json::Value;

use super::{ID, TESTED, root};
use crate::env::Env;
use crate::file_name;
use crate::hooks::{
    HookError, HookEvent, HookInstall, HookKind, HookPlan, HookReply, Hooks, InstalledHook, PlannedFile, TurnOutcome,
    groups, read_optional,
};
use crate::ojson::OValue;

pub struct CodexHooks;

const EVENTS: &[(HookKind, &str)] = &[
    (HookKind::SessionStart, "SessionStart"),
    (HookKind::PromptSubmit, "UserPromptSubmit"),
    (HookKind::TurnEnd, "Stop"),
    (HookKind::SessionEnd, "SessionEnd"),
];

fn kind_of(event: &str) -> Option<HookKind> {
    EVENTS.iter().find(|(_, e)| *e == event).map(|(k, _)| *k)
}

/// The program a command string starts with: `C:\x\p.exe --a`, `"C:\x y\p.exe" --a` or
/// `& 'C:\x y\p.exe' --a`.
fn program_of(command: &str) -> &str {
    let c = command.trim_start();
    let c = c.strip_prefix('&').map(str::trim_start).unwrap_or(c);
    for q in ['\'', '"'] {
        if let Some(rest) = c.strip_prefix(q) {
            return rest.split(q).next().unwrap_or(rest);
        }
    }
    c.split_whitespace().next().unwrap_or(c)
}

fn ours(program_name: &str) -> impl Fn(&OValue) -> bool + '_ {
    move |h: &OValue| {
        h.get("type").and_then(OValue::as_str) == Some("command")
            && h.get("command").and_then(OValue::as_str).is_some_and(|c| file_name(program_of(c)).eq_ignore_ascii_case(program_name))
    }
}

fn describe(h: &OValue) -> String {
    let cmd = h.get("command").and_then(OValue::as_str).unwrap_or("?");
    let timeout = h.get("timeout").map(|t| format!(" (timeout {} s)", t.to_value())).unwrap_or_default();
    format!("{cmd}{timeout}")
}

fn command_line(program: &str) -> String {
    if program.contains(' ') { format!("& '{program}' --agent {ID}") } else { format!("{program} --agent {ID}") }
}

fn handler(program: &str, timeout_s: u32) -> OValue {
    OValue::Object(vec![
        ("type".into(), OValue::str("command")),
        ("command".into(), OValue::str(&command_line(program))),
        ("timeout".into(), OValue::Number(timeout_s.into())),
    ])
}

/// Whether `config.toml` turns hooks off (`[features] hooks = false`, or the old name
/// `codex_hooks`). A line scan, not a TOML parser: it only reads.
pub(crate) fn hooks_disabled(config_toml: &str) -> bool {
    let mut table = String::new();
    for line in config_toml.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            table = l.trim_matches(|c| c == '[' || c == ']').trim().to_owned();
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.split('#').next().unwrap_or("").trim();
        if table == "features" && matches!(k.trim(), "hooks" | "codex_hooks") && v == "false" {
            return true;
        }
    }
    false
}

impl Hooks for CodexHooks {
    fn events(&self) -> &'static [(HookKind, &'static str)] {
        EVENTS
    }

    fn config_file(&self, env: &Env) -> PathBuf {
        root(env).join("hooks.json")
    }

    fn installed(&self, env: &Env, program_name: &str) -> Result<Vec<InstalledHook>, HookError> {
        let path = self.config_file(env);
        let doc = groups::load(read_optional(&path)?.as_deref(), &path)?;
        Ok(groups::find(&doc, &ours(program_name))?
            .into_iter()
            .map(|(event, h)| InstalledHook {
                kind: kind_of(&event),
                native_event: event,
                command: describe(&h),
                timeout_s: h.get("timeout").and_then(|t| t.to_value().as_u64()),
            })
            .collect())
    }

    fn plan_install(&self, env: &Env, req: &HookInstall) -> Result<HookPlan, HookError> {
        match req.agent_version.as_deref() {
            None => return Err(HookError::UnknownVersion("the installed Codex version is not known".to_owned())),
            Some(v) => TESTED.check(v).map(|_| ()).map_err(HookError::UnknownVersion)?,
        }
        let program = req.program.to_string_lossy().into_owned();
        let mut wanted: Vec<(&str, OValue)> = Vec::new();
        for (kind, timeout) in &req.kinds {
            match EVENTS.iter().find(|(k, _)| k == kind) {
                Some((_, event)) => wanted.push((event, handler(&program, *timeout))),
                None => return Err(HookError::Unsupported(format!("Codex has no {} hook", kind.name()))),
            }
        }
        if wanted.is_empty() {
            return Err(HookError::Unsupported("no hook kinds requested".to_owned()));
        }
        let mut notes = vec![
            "Codex asks you to trust new or changed hooks (/hooks in the Codex terminal UI) before it runs them.".to_owned(),
            "Not yet tried on this machine: on Windows Codex runs hooks through PowerShell (pwsh).".to_owned(),
            "Codex does not report why a turn ended, and Stop may not run when a turn fails (openai/codex#22774).".to_owned(),
        ];
        if program.contains(' ') {
            notes.push("The hook program's path has spaces; Codex has a bug with quoted program paths (openai/codex#57050).".to_owned());
        }
        let toml = read_optional(&root(env).join("config.toml"))?.unwrap_or_default();
        if hooks_disabled(&toml) {
            notes.push("config.toml has [features] hooks = false: Codex will not run any hook until that is removed.".to_owned());
        }
        plan(self.config_file(env), file_name(&program), wanted, notes)
    }

    fn plan_uninstall(&self, env: &Env, program_name: &str) -> Result<HookPlan, HookError> {
        plan(self.config_file(env), program_name, Vec::new(), Vec::new())
    }

    fn parse(&self, input: &[u8]) -> Result<HookEvent, HookError> {
        let v: Value =
            serde_json::from_slice(input).map_err(|e| HookError::UnknownFormat(format!("hook input is not JSON ({e})")))?;
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let event = s("hook_event_name").ok_or_else(|| HookError::UnknownFormat("no hook_event_name".to_owned()))?;
        let kind = kind_of(&event).ok_or_else(|| HookError::UnknownEvent(event.clone()))?;
        let session_id = s("session_id").ok_or_else(|| HookError::UnknownFormat(format!("{event} without session_id")))?;
        let (outcome, continuing, detail) = match kind {
            HookKind::TurnEnd => (Some(TurnOutcome::Unknown), v.get("stop_hook_active").and_then(Value::as_bool), None),
            HookKind::SessionStart => (None, None, s("source")),
            HookKind::SessionEnd => (None, None, s("reason")),
            HookKind::PromptSubmit => (None, None, None),
        };
        Ok(HookEvent {
            agent: ID,
            kind,
            native_event: event,
            session_id,
            cwd: s("cwd").map(|c| super::transcript::path_of(&c)),
            transcript_path: s("transcript_path"),
            outcome,
            continuing,
            detail,
            permission_mode: s("permission_mode"),
        })
    }

    fn continue_reply(&self, prompt: &str) -> Option<HookReply> {
        // JSON on stdout with exit 0 (exit code 2 is unreliable under PowerShell, openai/codex#54072).
        Some(HookReply { stdout: serde_json::json!({"decision": "block", "reason": prompt}).to_string(), exit_code: 0 })
    }
}

fn plan(path: PathBuf, program_name: &str, wanted: Vec<(&str, OValue)>, notes: Vec<String>) -> Result<HookPlan, HookError> {
    let before = read_optional(&path)?;
    let mut doc = groups::load(before.as_deref(), &path)?;
    let is_ours = ours(program_name);
    let mut have: Vec<(String, Value)> = groups::find(&doc, &is_ours)?.into_iter().map(|(e, h)| (e, h.to_value())).collect();
    let mut target: Vec<(String, Value)> = wanted.iter().map(|(e, h)| ((*e).to_owned(), h.to_value())).collect();
    have.sort_by(|a, b| a.0.cmp(&b.0));
    target.sort_by(|a, b| a.0.cmp(&b.0));
    if have == target {
        return Ok(HookPlan { agent: ID, files: Vec::new(), notes });
    }
    let mut summary = groups::remove(&mut doc, &is_ours, &describe);
    summary.extend(groups::add(&mut doc, wanted, &describe));
    Ok(HookPlan { agent: ID, files: vec![PlannedFile { path, before, after: doc.to_pretty(), summary }], notes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_paths_and_feature_switch() {
        assert_eq!(program_of(r"C:\a\away-guard-hook.exe --agent codex"), r"C:\a\away-guard-hook.exe");
        assert_eq!(program_of(r"& 'C:\a b\away-guard-hook.exe' --agent codex"), r"C:\a b\away-guard-hook.exe");
        assert_eq!(program_of(r#""C:\a b\x.exe" y"#), r"C:\a b\x.exe");
        assert!(hooks_disabled("[features]\njs_repl = false\nhooks = false # off\n"));
        assert!(!hooks_disabled("[features]\njs_repl = false\n[other]\nhooks = false\n"));
        assert!(!hooks_disabled("[features]\nhooks = true\n"));
    }
}
