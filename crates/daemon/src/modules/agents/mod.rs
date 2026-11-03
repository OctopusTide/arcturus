//! agent 适配层 (agents): the daemon side of `crates/agents`. Hook calls come in here (sent by
//! `away-guard-hook`, already in the unified format) and go out as bus events, so modules that
//! react to conversations (task protection, resume after an outage) never see an agent's own
//! format or name. Also lists the adapters and, on request, the installed agents.
//!
//! Events (`agents.<kind>`), all with `agent`, `session_id`, `native_event`, `cwd`,
//! `transcript`, `hook_pid` (the hook program, from the OS) and `agent_pid` / `agent_exe` (the
//! closest ancestor of the hook program that is the agent's executable, if found):
//! - `session_start` (+ `detail`: startup / resume / ...), `session_end` (+ `detail`: reason),
//!   `prompt_submit`;
//! - `turn_end` (+ `outcome`: `{"kind": "completed"}`, `{"kind": "unknown"}` or
//!   `{"kind": "api_error", "failure": {"class", "network", "code", "http_status"}}`;
//!   `continuing`: the agent is already continuing because a hook asked it to).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agents::hooks::HookKind;
use proto::{ApiError, ErrorCode};
use serde_json::{Map, Value, json};

use crate::dlog;
use crate::module::{
    ArgKind, ArgSpec, CommandSpec, ErrorSpec, EventSpec, Module, ModuleCtx, ModuleRequest, ModuleSpec,
    RecoverOutcome, Section,
};
use crate::store::{JournalEntry, now_ms};

static SPEC: ModuleSpec = ModuleSpec::new(
    "agents",
    Section::AccountsStorage,
    "Agent adapters: hooks in, unified events out",
)
.commands(&[
    CommandSpec::new(
        "list",
        "Supported agents, what their hooks can do, installs found by `detect`",
    )
    .agent()
    .read_only(),
    CommandSpec::new(
        "detect",
        "Look for installed agents and their versions (runs `<agent> --version`)",
    )
    .agent()
    .read_only(),
    CommandSpec::new(
        "hook",
        "One hook call, in the unified format (sent by away-guard-hook)",
    )
    .agent()
    .hidden()
    .args(&[
        ArgSpec::new("agent", ArgKind::Str, "Adapter id, e.g. claude-code").required(),
        ArgSpec::new("event", ArgKind::Json, "agents::HookEvent as JSON").required(),
    ]),
])
.publishes(&[
    EventSpec::new("session_start", "A conversation started or was resumed"),
    EventSpec::new("prompt_submit", "The user sent a message"),
    EventSpec::new(
        "turn_end",
        "The agent finished a turn (outcome says whether the model API failed)",
    ),
    EventSpec::new("session_end", "A conversation ended"),
])
.error_codes(&[
    ErrorSpec::new("UNKNOWN_AGENT", ErrorCode::NotFound, "No adapter with this id"),
    ErrorSpec::new(
        "BAD_EVENT",
        ErrorCode::InvalidArgs,
        "The hook event is not in the unified format",
    ),
]);

/// How far up the process tree to look for the agent.
const MAX_ANCESTORS: usize = 16;
const DETECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Detected {
    running: bool,
    at: Option<i64>,
    installs: Vec<agents::detect::Installation>,
}

#[derive(Default)]
pub struct AgentsModule {
    detected: Arc<Mutex<Detected>>,
    /// Per agent: hook calls received and the last one's kind and time.
    received: std::collections::BTreeMap<String, (u64, String, i64)>,
}

pub fn create() -> Box<dyn Module> {
    Box::new(AgentsModule::default())
}

impl AgentsModule {
    fn list(&self) -> Value {
        let detected = self.detected.lock().unwrap_or_else(|p| p.into_inner());
        let adapters: Vec<Value> = agents::all()
            .iter()
            .map(|a| {
                let i = a.info();
                let installs: Vec<&agents::detect::Installation> =
                    detected.installs.iter().filter(|x| x.agent == i.id).collect();
                json!({
                    "id": i.id,
                    "name": i.display_name,
                    "executables": i.executables,
                    "host_apps": i.host_apps,
                    "tested_versions": {"min": i.tested.min, "max": i.tested.max},
                    "transcripts": i.transcripts,
                    "hooks": i.hooks,
                    "hook_events": a.hooks().map(|h| h.events().iter().map(|(k, e)| json!({"kind": k.name(), "native": e})).collect::<Vec<_>>()),
                    "installs": installs,
                    "hooks_received": self.received.get(i.id).map(|(n, kind, at)| json!({"count": n, "last_kind": kind, "last_at": at})),
                })
            })
            .collect();
        json!({ "adapters": adapters, "detected_at": detected.at, "detecting": detected.running })
    }

    fn hook(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        let id = req.args["agent"].as_str().unwrap_or_default();
        let adapter =
            agents::get(id).ok_or_else(|| ctx.error("UNKNOWN_AGENT", format!("no adapter '{id}'")))?;
        let ev = req.args["event"]
            .as_object()
            .ok_or_else(|| ctx.error("BAD_EVENT", "event must be an object"))?;
        let kind = ev
            .get("kind")
            .and_then(Value::as_str)
            .and_then(HookKind::from_name)
            .ok_or_else(|| {
                ctx.error(
                    "BAD_EVENT",
                    "event.kind must be session_start, prompt_submit, turn_end or session_end",
                )
            })?;
        let session_id = ev
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ctx.error("BAD_EVENT", "event.session_id is missing"))?;
        let (agent_pid, agent_exe) = match req.caller.pid {
            Some(pid) => find_agent(ctx, pid, adapter.info().executables),
            None => (None, None),
        };
        if let Some(pid) = agent_pid {
            if let Ok(process) = ctx.system().process_info(pid) {
                if process.own_user == Some(true) && ev.get("resume_context_complete") == Some(&json!(true)) {
                    if let Some(values) = ev.get("resume_environment").and_then(Value::as_object) {
                        let mut environment = std::collections::BTreeMap::new();
                        for (key, value) in values {
                            let Some(value) = value.as_str() else {
                                return Err(ctx.error("BAD_EVENT", "invalid connection context"));
                            };
                            if !agents::safe_resume_env(key, value) {
                                return Err(ctx.error("BAD_EVENT", "unsafe connection context"));
                            }
                            environment.insert(key.clone(), value.to_owned());
                        }
                        let mut a = ctx.arbitration();
                        let key = (id.to_owned(), session_id.to_owned());
                        if a.resume_environments.len() < 256 || a.resume_environments.contains_key(&key) {
                            a.resume_environments.insert(key, (process, environment));
                        }
                    }
                } else {
                    ctx.arbitration()
                        .resume_environments
                        .remove(&(id.to_owned(), session_id.to_owned()));
                }
            }
        }
        if let Some(value) = ev.get("resume_broker") {
            if let Ok(broker) = serde_json::from_value::<crate::sys::ProcessInfo>(value.clone()) {
                // A broker we cannot trust is not used, but the turn still ended as it says:
                // the queue then tells the user instead of the failure going unnoticed.
                if broker_is_the_hooks_child(ctx, req.caller.pid, &broker) {
                    ctx.arbitration()
                        .resume_brokers
                        .insert((id.to_owned(), session_id.to_owned()), broker);
                } else {
                    dlog!("WARN", "agents: ignoring an unverified continuation broker (pid {})", broker.pid);
                }
            }
        }
        // Only the unified fields travel on: nothing else the hook program might have sent.
        let mut data = Map::new();
        data.insert("agent".into(), json!(id));
        data.insert("session_id".into(), json!(session_id));
        for (from, to) in [
            ("native_event", "native_event"),
            ("cwd", "cwd"),
            ("transcript_path", "transcript"),
            ("detail", "detail"),
            ("permission_mode", "permission_mode"),
        ] {
            data.insert(
                to.into(),
                ev.get(from)
                    .filter(|v| v.is_string())
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
        if kind == HookKind::TurnEnd {
            data.insert(
                "outcome".into(),
                ev.get("outcome")
                    .filter(|v| v.is_object())
                    .cloned()
                    .unwrap_or(json!({"kind": "unknown"})),
            );
            data.insert(
                "continuing".into(),
                ev.get("continuing")
                    .filter(|v| v.is_boolean())
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            data.insert(
                "in_a_window".into(),
                ev.get("in_a_window")
                    .filter(|v| v.is_boolean())
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
        data.insert(
            "automatic_continuation".into(),
            ev.get("automatic_continuation").cloned().unwrap_or(json!(false)),
        );
        data.insert("hook_pid".into(), json!(req.caller.pid));
        data.insert("agent_pid".into(), json!(agent_pid));
        data.insert("agent_exe".into(), json!(agent_exe));
        ctx.publish(kind.name(), Value::Object(data))?;
        let entry = self
            .received
            .entry(id.to_owned())
            .or_insert((0, String::new(), 0));
        *entry = (entry.0 + 1, kind.name().to_owned(), now_ms());
        Ok(json!({ "topic": format!("agents.{}", kind.name()), "agent_pid": agent_pid, "reply": null }))
    }

    fn detect(&self) -> Value {
        let mut d = self.detected.lock().unwrap_or_else(|p| p.into_inner());
        if d.running {
            return json!({ "started": false, "running": true });
        }
        d.running = true;
        let shared = self.detected.clone();
        let spawned = std::thread::Builder::new()
            .name("agents-detect".into())
            .spawn(move || {
                let env = agents::Env::from_process();
                let installs: Vec<_> = agents::all()
                    .iter()
                    .flat_map(|a| agents::detect::installations(*a, &env, DETECT_TIMEOUT))
                    .collect();
                let mut d = shared.lock().unwrap_or_else(|p| p.into_inner());
                *d = Detected {
                    running: false,
                    at: Some(now_ms()),
                    installs,
                };
            });
        if let Err(e) = spawned {
            d.running = false;
            dlog!("WARN", "agents: cannot start detection: {e}");
            return json!({ "started": false, "error": e.to_string() });
        }
        json!({ "started": true, "running": true, "note": "results appear in `agents list` within a few seconds" })
    }
}

/// The nearest ancestor of `pid` whose executable is one of `names` (process ids can be reused,
/// so this is a best effort).
/// The broker a hook names must be its own child, same user, same program, still that process.
fn broker_is_the_hooks_child(ctx: &ModuleCtx, hook: Option<u32>, broker: &crate::sys::ProcessInfo) -> bool {
    let sys = ctx.system();
    broker.own_user == Some(true)
        && sys.process_info(broker.pid).is_ok_and(|actual| actual == *broker)
        && broker.exe == hook.and_then(|pid| sys.process_image_path(pid))
        && sys
            .processes()
            .is_ok_and(|list| list.iter().any(|p| p.pid == broker.pid && Some(p.parent_pid) == hook))
}

fn find_agent(ctx: &ModuleCtx, pid: u32, names: &[&str]) -> (Option<u32>, Option<String>) {
    let Ok(procs) = ctx.system().processes() else {
        return (None, None);
    };
    let parent: std::collections::HashMap<u32, (u32, &str)> = procs
        .iter()
        .map(|p| (p.pid, (p.parent_pid, p.name.as_str())))
        .collect();
    let mut at = pid;
    for _ in 0..MAX_ANCESTORS {
        let Some(&(up, _)) = parent.get(&at) else {
            break;
        };
        if up == 0 || up == at {
            break;
        }
        at = up;
        if let Some(&(_, name)) = parent.get(&at)
            && names.iter().any(|n| n.eq_ignore_ascii_case(name))
        {
            let exe = ctx.system().process_image_path(at);
            return (Some(at), exe.or_else(|| Some(name.to_owned())));
        }
    }
    (None, None)
}

impl Module for AgentsModule {
    fn spec(&self) -> &'static ModuleSpec {
        &SPEC
    }

    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        match req.cmd {
            "list" => Ok(self.list()),
            "detect" => Ok(self.detect()),
            "hook" => self.hook(req, ctx),
            other => Err(ApiError::internal(format!("agents has no handler for '{other}'"))),
        }
    }

    fn status(&self, _ctx: &ModuleCtx) -> Value {
        let ids: Vec<&str> = agents::all().iter().map(|a| a.info().id).collect();
        let received: u64 = self.received.values().map(|(n, _, _)| n).sum();
        json!({
            "implemented": true,
            "summary": format!("adapters: {}; hook calls received: {received}", ids.join(", ")),
        })
    }

    fn recover(&mut self, _entry: &JournalEntry, _ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        // Changes nothing on the system, so it never writes journal entries.
        Ok(RecoverOutcome::Restored)
    }
}
