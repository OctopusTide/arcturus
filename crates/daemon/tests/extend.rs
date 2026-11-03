//! Adding modules: the shipped example module, and a module defined only in this test, both
//! reach the catalog, routing, permission check and status with no framework change.

mod common;

use common::{
    AGENT_EXE, Env, agent_call, err_code, hello, module_status, raw_session, wait_events,
};
use daemon::{
    ArgKind, ArgSpec, CommandSpec, JournalEntry, Module, ModuleCtx, ModuleRequest, ModuleSpec,
};
use daemon::{RecoverOutcome, Section};
use proto::{ApiError, ErrorCode};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

#[test]
fn example_module_is_off_by_default() {
    let env = Env::new();
    let d = env.start(daemon::modules::builtin());
    let resp = agent_call(&d, "example.echo", json!({"text": "hi"}));
    assert_eq!(err_code(&resp).0, ErrorCode::ModuleDisabled);
    // Its tables exist anyway, so switching it on later needs no rebuild.
    assert_eq!(d.store().schema_version("example").unwrap(), 2);
}

#[test]
fn example_module_works_once_switched_on() {
    let env = Env::new();
    env.system.uptime_ms.store(12_345, Ordering::SeqCst);
    env.write_config(json!({"modules": {"example": {"enabled": true}}}));
    let d = env.start(vec![daemon::modules::example::create()]);

    let r = agent_call(&d, "example.echo", json!({"text": "hello", "shout": true}))
        .into_result()
        .unwrap();
    assert_eq!(
        r,
        json!({"echo": "HELLO", "uptime_ms": 12_345}),
        "uptime comes from the fake system layer"
    );
    assert!(agent_call(&d, "example.echo", json!({"text": "again"})).ok);

    let h = agent_call(&d, "example.history", json!({"limit": 5}))
        .into_result()
        .unwrap();
    assert_eq!(h["items"][0]["text"], "again");
    assert_eq!(h["items"][1]["text"], "hello");
    assert_eq!(h["items"][0]["caller_pid"], 4242);

    let resp = agent_call(&d, "example.echo", json!({"text": "  "}));
    assert_eq!(
        err_code(&resp),
        (ErrorCode::InvalidArgs, Some("example.EMPTY_TEXT".into()))
    );

    wait_events(&d);
    let st = agent_call(&d, "status", json!({})).into_result().unwrap();
    let ex = module_status(&st, "example");
    assert_eq!(ex["section"], "common");
    assert!(ex["state"]["events_seen"].as_u64().unwrap() >= 2, "{ex}");
    assert_eq!(ex["state"]["last_topic"], "example.echoed");
}

static WEATHER: ModuleSpec =
    ModuleSpec::new("weather", Section::Protection, "Defined only in this test").commands(&[
        CommandSpec::new("forecast", "Tomorrow's weather")
            .short("forecast")
            .agent()
            .read_only()
            .args(&[ArgSpec::new("days", ArgKind::Uint, "How many days").required()]),
        CommandSpec::new("set_station", "Page-only setting").args(&[ArgSpec::new(
            "id",
            ArgKind::Str,
            "",
        )
        .required()]),
    ]);

struct Weather;

impl Module for Weather {
    fn spec(&self) -> &'static ModuleSpec {
        &WEATHER
    }

    fn handle(&mut self, req: &ModuleRequest<'_>, _ctx: &ModuleCtx) -> Result<Value, ApiError> {
        Ok(json!({ "days": req.args["days"], "sky": "clear" }))
    }

    fn recover(
        &mut self,
        _entry: &JournalEntry,
        _ctx: &ModuleCtx,
    ) -> Result<RecoverOutcome, ApiError> {
        Ok(RecoverOutcome::Restored)
    }
}

#[test]
fn a_new_module_needs_no_framework_change() {
    let env = Env::new();
    let d = env.start_with(vec![Box::new(Weather)]);
    let h = hello(&d, &mut raw_session(&d, AGENT_EXE));
    let c = h.command("forecast").unwrap();
    assert_eq!(
        (c.name.as_str(), c.section),
        ("weather.forecast", Some(Section::Protection))
    );
    assert!(c.callable);
    assert_eq!(c.args[0].kind, ArgKind::Uint);
    assert!(!h.command("weather.set_station").unwrap().callable);

    let r = agent_call(&d, "forecast", json!({"days": 2}))
        .into_result()
        .unwrap();
    assert_eq!(r["sky"], "clear");
    assert_eq!(
        err_code(&agent_call(&d, "forecast", json!({}))).0,
        ErrorCode::InvalidArgs
    );
    assert_eq!(
        err_code(&agent_call(&d, "weather.set_station", json!({"id": "x"}))).0,
        ErrorCode::Denied
    );
    let st = agent_call(&d, "status", json!({})).into_result().unwrap();
    assert_eq!(module_status(&st, "weather")["section"], "protection");
}
