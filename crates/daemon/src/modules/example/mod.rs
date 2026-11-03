//! 示例 (example): a complete, working module that exists to show how modules are added.
//! Off by default; turn it on with `{"modules":{"example":{"enabled":true}}}` in config.json.
//!
//! It uses every extension point: commands with typed arguments (one short name), its own
//! tables with two migration steps, an event it publishes, a subscription to all events, a
//! namespaced error code, and a system call through `ctx.system()`.

use proto::{ApiError, ErrorCode};
use serde_json::{Value, json};

use crate::module::{
    ArgKind, ArgSpec, CommandSpec, ErrorSpec, Event, EventSpec, Migration, Module, ModuleCtx, ModuleRequest,
    ModuleSpec, RecoverOutcome, Section,
};
use crate::store::{JournalEntry, now_ms};

static SPEC: ModuleSpec = ModuleSpec::new("example", Section::Common, "Example module (how to add one)")
    .off_by_default()
    .commands(&[
        CommandSpec::new("echo", "Echo text back and remember it").agent().args(&[
            ArgSpec::new("text", ArgKind::Str, "Text to echo").required().positional(),
            ArgSpec::new("shout", ArgKind::Bool, "Upper-case the reply"),
        ]),
        CommandSpec::new("history", "Texts echoed so far, newest first").agent().read_only().args(&[
            ArgSpec::new("limit", ArgKind::Uint, "How many (default 10)"),
        ]),
    ])
    .publishes(&[EventSpec::new("echoed", "Someone called example.echo")])
    .subscribes(&["*"])
    .migrations(&[
        Migration::new(1, "CREATE TABLE example_echoes (id INTEGER PRIMARY KEY, text TEXT NOT NULL, ts INTEGER NOT NULL)"),
        // A later change to the same table: a new step, never an edit of step 1.
        Migration::new(2, "ALTER TABLE example_echoes ADD COLUMN caller_pid INTEGER"),
    ])
    .error_codes(&[ErrorSpec::new("EMPTY_TEXT", ErrorCode::InvalidArgs, "The text to echo is empty")]);

#[derive(Default)]
pub struct Example {
    events_seen: u64,
    last_topic: Option<String>,
}

pub fn create() -> Box<dyn Module> {
    Box::new(Example::default())
}

impl Module for Example {
    fn spec(&self) -> &'static ModuleSpec {
        &SPEC
    }

    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        match req.cmd {
            "echo" => {
                let text = req.args["text"].as_str().unwrap_or_default();
                if text.trim().is_empty() {
                    return Err(ctx.error("EMPTY_TEXT", "nothing to echo"));
                }
                let reply = if req.args["shout"].as_bool() == Some(true) { text.to_uppercase() } else { text.to_owned() };
                ctx.db().execute(
                    "INSERT INTO example_echoes (text, ts, caller_pid) VALUES (?, ?, ?)",
                    &[text.into(), now_ms().into(), req.caller.pid.into()],
                )?;
                ctx.publish("echoed", json!({ "text": text }))?;
                Ok(json!({ "echo": reply, "uptime_ms": ctx.system().uptime_ms()? }))
            }
            "history" => {
                let limit = req.args["limit"].as_u64().unwrap_or(10).min(1000) as u32;
                let rows = ctx.db().query(
                    "SELECT text, ts, caller_pid FROM example_echoes ORDER BY id DESC LIMIT ?",
                    &[limit.into()],
                )?;
                let items: Vec<Value> = rows
                    .iter()
                    .map(|r| json!({ "text": r.text(0).ok(), "ts": r.int(1).ok(), "caller_pid": r.opt_int(2).ok().flatten() }))
                    .collect();
                Ok(json!({ "items": items }))
            }
            other => Err(ApiError::internal(format!("example has no handler for '{other}'"))),
        }
    }

    fn status(&self, _ctx: &ModuleCtx) -> Value {
        json!({ "implemented": true, "events_seen": self.events_seen, "last_topic": self.last_topic })
    }

    fn recover(&mut self, _entry: &JournalEntry, _ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        // Never changes system settings, so it never writes journal entries.
        Ok(RecoverOutcome::Restored)
    }

    fn on_event(&mut self, event: &Event, _ctx: &ModuleCtx) -> Result<(), ApiError> {
        self.events_seen += 1;
        self.last_topic = Some(event.topic.clone());
        Ok(())
    }
}
