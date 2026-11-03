//! Claude Code hooks. User settings `<config dir>/settings.json`, key `hooks`; our handler is
//! run without a shell (`command` + `args`), so no Git Bash or PowerShell quoting is involved:
//!
//! ```json
//! {"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "C:\\...\\away-guard-hook.exe",
//!                                  "args": ["--agent", "claude-code"], "timeout": 30}]}]}}
//! ```
//!
//! Stdin (documented, code.claude.com/docs/en/hooks): `session_id`, `transcript_path`, `cwd`,
//! `hook_event_name`; `Stop`: `stop_hook_active`; `StopFailure` (instead of `Stop` when the
//! turn ends on an API error): `error`, optional `error_details`, `last_assistant_message`;
//! `SessionStart`: `source`; `SessionEnd`: `reason`. Measured on 2.1.309: `UserPromptSubmit` and
//! `Stop` also carry `permission_mode`, `StopFailure` and `SessionEnd` do not; none carries the model.

use std::path::PathBuf;

use serde_json::Value;

use super::{ID, TESTED, classify, root};
use crate::env::Env;
use crate::file_name;
use crate::hooks::{
    HookError, HookEvent, HookInstall, HookKind, HookPlan, HookReply, Hooks, InstalledHook, PlannedFile, TurnOutcome,
    groups, http_status_in, read_optional,
};
use crate::ojson::OValue;

pub struct ClaudeHooks;

const EVENTS: &[(HookKind, &str)] = &[
    (HookKind::SessionStart, "SessionStart"),
    (HookKind::PromptSubmit, "UserPromptSubmit"),
    (HookKind::TurnEnd, "Stop"),
    (HookKind::TurnEnd, "StopFailure"),
    (HookKind::SessionEnd, "SessionEnd"),
];

fn kind_of(event: &str) -> Option<HookKind> {
    EVENTS.iter().find(|(_, e)| *e == event).map(|(k, _)| *k)
}

fn ours(program_name: &str) -> impl Fn(&OValue) -> bool + '_ {
    move |h: &OValue| {
        h.get("type").and_then(OValue::as_str) == Some("command")
            && h.get("command").and_then(OValue::as_str).is_some_and(|c| file_name(c).eq_ignore_ascii_case(program_name))
    }
}

fn describe(h: &OValue) -> String {
    let cmd = h.get("command").and_then(OValue::as_str).unwrap_or("?");
    let args: Vec<&str> = h.get("args").and_then(OValue::as_array).map(|a| a.iter().filter_map(OValue::as_str).collect()).unwrap_or_default();
    let timeout = h.get("timeout").map(|t| format!(" (timeout {} s)", t.to_value())).unwrap_or_default();
    format!("{cmd} {}{timeout}", args.join(" ")).replace("  ", " ")
}

fn handler(program: &str, timeout_s: u32) -> OValue {
    OValue::Object(vec![
        ("type".into(), OValue::str("command")),
        ("command".into(), OValue::str(program)),
        ("args".into(), OValue::Array(vec![OValue::str("--agent"), OValue::str(ID)])),
        ("timeout".into(), OValue::Number(timeout_s.into())),
    ])
}

impl Hooks for ClaudeHooks {
    fn events(&self) -> &'static [(HookKind, &'static str)] {
        EVENTS
    }

    fn config_file(&self, env: &Env) -> PathBuf {
        root(env).join("settings.json")
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
        check_version(req.agent_version.as_deref())?;
        let program = req.program.to_string_lossy().into_owned();
        let name = file_name(&program).to_owned();
        let mut wanted: Vec<(&str, OValue)> = Vec::new();
        for (kind, timeout) in &req.kinds {
            for (k, event) in EVENTS {
                if k == kind {
                    wanted.push((event, handler(&program, *timeout)));
                }
            }
        }
        if wanted.is_empty() {
            return Err(HookError::Unsupported("no hook kinds requested".to_owned()));
        }
        let notes = vec![
            "Not yet tried on this machine. Conversations already open may keep their old hooks until they are restarted."
                .to_owned(),
            "Interactive sessions run hooks only in folders Claude Code trusts.".to_owned(),
        ];
        plan(self.config_file(env), &name, wanted, notes)
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
        let (outcome, continuing, detail) = match event.as_str() {
            "Stop" => (Some(TurnOutcome::Completed), v.get("stop_hook_active").and_then(Value::as_bool), None),
            "StopFailure" => {
                let code = s("error").ok_or_else(|| HookError::UnknownFormat("StopFailure without error".to_owned()))?;
                let details = s("error_details");
                let status = details.as_deref().and_then(http_status_in);
                let text = [details.as_deref(), v.get("last_assistant_message").and_then(Value::as_str)]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ");
                let failure = classify(&code, status, Some(&text));
                (Some(TurnOutcome::ApiError { failure }), None, None)
            }
            "SessionStart" => (None, None, s("source")),
            "SessionEnd" => (None, None, s("reason")),
            _ => (None, None, None),
        };
        Ok(HookEvent {
            agent: ID,
            kind,
            native_event: event,
            session_id,
            cwd: s("cwd"),
            transcript_path: s("transcript_path"),
            outcome,
            continuing,
            detail,
            permission_mode: s("permission_mode"),
        })
    }

    fn continue_reply(&self, prompt: &str) -> Option<HookReply> {
        // A Stop hook continues the turn with {"decision":"block","reason":...}; Claude Code
        // allows at most 8 in a row. StopFailure output is ignored.
        Some(HookReply { stdout: serde_json::json!({"decision": "block", "reason": prompt}).to_string(), exit_code: 0 })
    }
}

/// Refuses an agent version outside the tested range (or not known).
pub(crate) fn check_version(version: Option<&str>) -> Result<(), HookError> {
    match version {
        None => Err(HookError::UnknownVersion("the installed Claude Code version is not known".to_owned())),
        Some(v) => TESTED.check(v).map(|_| ()).map_err(HookError::UnknownVersion),
    }
}

/// Plans `settings.json`: our existing handlers replaced by `wanted` (none = uninstall).
fn plan(path: PathBuf, program_name: &str, wanted: Vec<(&str, OValue)>, notes: Vec<String>) -> Result<HookPlan, HookError> {
    let before = read_optional(&path)?;
    let mut doc = groups::load(before.as_deref(), &path)?;
    let is_ours = ours(program_name);
    let existing: Vec<(String, Value)> =
        groups::find(&doc, &is_ours)?.into_iter().map(|(e, h)| (e, h.to_value())).collect();
    let mut target: Vec<(String, Value)> = wanted.iter().map(|(e, h)| ((*e).to_owned(), h.to_value())).collect();
    let mut have = existing.clone();
    have.sort_by(|a, b| a.0.cmp(&b.0));
    target.sort_by(|a, b| a.0.cmp(&b.0));
    if have == target {
        return Ok(HookPlan { agent: ID, files: Vec::new(), notes });
    }
    let mut summary = groups::remove(&mut doc, &is_ours, &describe);
    summary.extend(groups::add(&mut doc, wanted, &describe));
    Ok(HookPlan { agent: ID, files: vec![PlannedFile { path, before, after: doc.to_pretty(), summary }], notes })
}
