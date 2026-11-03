//! The agents module: hook calls in the unified format become `agents.*` events with the
//! agent's process found in the caller's process tree; only unified fields travel on.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use daemon::{Event, JournalEntry, Module, ModuleCtx, ModuleRequest, ModuleSpec, RecoverOutcome, Section};
use proto::{ApiError, ErrorCode};
use serde_json::{Value, json};

static WATCH: ModuleSpec = ModuleSpec::new("watch", Section::Common, "Records agents.* events").subscribes(&["agents.*"]);

struct Watch(Arc<Mutex<Vec<Event>>>);

impl Module for Watch {
    fn spec(&self) -> &'static ModuleSpec {
        &WATCH
    }
    fn handle(&mut self, _req: &ModuleRequest<'_>, _ctx: &ModuleCtx) -> Result<Value, ApiError> {
        Ok(Value::Null)
    }
    fn recover(&mut self, _e: &JournalEntry, _ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        Ok(RecoverOutcome::Restored)
    }
    fn on_event(&mut self, event: &Event, _ctx: &ModuleCtx) -> Result<(), ApiError> {
        self.0.lock().unwrap().push(event.clone());
        Ok(())
    }
}

#[test]
fn hook_calls_become_unified_events_with_the_agent_process() {
    let env = Env::new();
    // The CLI test session's pid is 4242: claude.exe -> shell -> away-guard-hook.
    env.system.spawn(4000, 1, "claude.exe").spawn(5000, 4000, "bash.exe").spawn(4242, 5000, "away-guard-hook.exe");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let d = env.start_with(vec![Box::new(Watch(seen.clone()))]);
    let event = json!({
        "agent": "claude-code", "kind": "turn_end", "native_event": "StopFailure", "session_id": "s-1",
        "cwd": r"D:\w", "transcript_path": r"C:\t.jsonl", "continuing": null,
        "outcome": {"kind": "api_error", "failure": {"class": "network", "network": true, "code": "server_error", "http_status": null}},
        "last_assistant_message": "must not travel", "prompt": "nor this"
    });
    let r = agent_call(&d, "agents.hook", json!({"agent": "claude-code", "event": event})).into_result().unwrap();
    assert_eq!(r["topic"], "agents.turn_end");
    assert_eq!(r["agent_pid"], 4000);
    wait_events(&d);
    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    let data = &got[0].data;
    assert_eq!(got[0].topic, "agents.turn_end");
    assert_eq!(data["agent_pid"], 4000);
    assert_eq!(data["hook_pid"], 4242);
    assert_eq!(data["transcript"], r"C:\t.jsonl");
    assert_eq!(data["outcome"]["failure"]["network"], true);
    let text = data.to_string();
    assert!(!text.contains("must not travel") && !text.contains("nor this"), "{text}");

    let list = agent_call(&d, "agents.list", json!({})).into_result().unwrap();
    let cc = list["adapters"].as_array().unwrap().iter().find(|a| a["id"] == "claude-code").unwrap();
    assert_eq!(cc["hooks_received"]["count"], 1);
    assert_eq!(cc["hooks"]["reports_api_errors"], true);
    assert!(list["adapters"].as_array().unwrap().iter().any(|a| a["id"] == "codex" && a["hooks"]["reports_api_errors"] == false));
}

#[test]
fn bad_hook_calls_are_refused_with_module_codes() {
    let env = Env::new();
    let d = env.start(daemon::modules::builtin());
    let r = agent_call(&d, "agents.hook", json!({"agent": "nope", "event": {"kind": "turn_end", "session_id": "s"}}));
    assert_eq!(err_code(&r), (ErrorCode::NotFound, Some("agents.UNKNOWN_AGENT".into())));
    let r = agent_call(&d, "agents.hook", json!({"agent": "codex", "event": {"kind": "teleport", "session_id": "s"}}));
    assert_eq!(err_code(&r), (ErrorCode::InvalidArgs, Some("agents.BAD_EVENT".into())));
    let r = agent_call(&d, "agents.hook", json!({"agent": "codex", "event": {"kind": "turn_end"}}));
    assert_eq!(err_code(&r), (ErrorCode::InvalidArgs, Some("agents.BAD_EVENT".into())));
    // Not on command lines: only the hook program sends it.
    let hello = hello(&d, &mut raw_session(&d, AGENT_EXE));
    let cmd = hello.commands.iter().find(|c| c.name == "agents.hook").unwrap();
    assert!(cmd.hidden && cmd.callable);
}
