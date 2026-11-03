//! Fault isolation, the event bus, config switches, namespaced error codes and the
//! narrow administrator layer, with small test modules.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Env, agent_call, err_code, module_status, wait_events};
use daemon::{ArgKind, ArgSpec, CommandSpec, ErrorSpec, Event, EventSpec, JournalEntry, Module, ModuleCtx};
use daemon::{ModuleRequest, ModuleSpec, RecoverOutcome, Section};
use proto::{ApiError, ErrorCode, subcode};
use serde_json::{Value, json};

#[derive(Default)]
struct Probe {
    fail_start: AtomicBool,
    panic_start: AtomicBool,
    panic_on_event: AtomicBool,
    seen: Mutex<Vec<String>>,
}

static CRASHY: ModuleSpec = ModuleSpec::new("crashy", Section::Usage, "Misbehaves on request")
    .commands(&[
        CommandSpec::new("ok", "Works").agent(),
        CommandSpec::new("boom", "Panics").agent(),
        CommandSpec::new("slow", "Holds its lock").agent().args(&[ArgSpec::new("ms", ArgKind::Uint, "").required()]),
        CommandSpec::new("ping", "Publishes crashy.pinged").agent(),
        CommandSpec::new("publish_undeclared", "Publishes an event it did not declare").agent(),
        CommandSpec::new("declared", "Fails with its own error code").agent(),
        CommandSpec::new("foreign_subcode", "Fails with another module's error code").agent(),
        CommandSpec::new("elevated", "Asks the administrator layer for something").agent(),
    ])
    .publishes(&[EventSpec::new("pinged", "ping was called")])
    .error_codes(&[ErrorSpec::new("TOO_HOT", ErrorCode::Conflict, "Machine is too hot")]);

static LISTENER: ModuleSpec = ModuleSpec::new("listener", Section::Protection, "Records events")
    .subscribes(&["crashy.*", "core.module_faulted"]);

struct Crashy(Arc<Probe>);
struct Listener(Arc<Probe>);

impl Module for Crashy {
    fn spec(&self) -> &'static ModuleSpec {
        &CRASHY
    }

    fn start(&mut self, _ctx: &ModuleCtx) -> Result<(), ApiError> {
        if self.0.panic_start.load(Ordering::SeqCst) {
            panic!("start exploded");
        }
        if self.0.fail_start.load(Ordering::SeqCst) {
            return Err(ApiError::internal("no device"));
        }
        Ok(())
    }

    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        match req.cmd {
            "ok" => Ok(json!({ "ok": true })),
            "boom" => panic!("kaboom"),
            "slow" => {
                std::thread::sleep(Duration::from_millis(req.args["ms"].as_u64().unwrap()));
                Ok(json!({}))
            }
            "ping" => ctx.publish("pinged", json!({ "n": 1 })).map(|()| json!({})),
            "publish_undeclared" => ctx.publish("nope", json!({})).map(|()| json!({})),
            "declared" => Err(ctx.error("TOO_HOT", "too hot to continue")),
            "foreign_subcode" => Err(ApiError::new(ErrorCode::Conflict, "x").with_subcode("task.ALREADY_PROTECTED")),
            "elevated" => {
                ctx.elevated().terminate_process(9)?;
                Ok(json!({ "backend": ctx.elevated().backend() }))
            }
            other => Err(ApiError::internal(format!("unrouted {other}"))),
        }
    }

    fn recover(&mut self, _entry: &JournalEntry, _ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        Ok(RecoverOutcome::Restored)
    }
}

impl Module for Listener {
    fn spec(&self) -> &'static ModuleSpec {
        &LISTENER
    }

    fn handle(&mut self, _req: &ModuleRequest<'_>, _ctx: &ModuleCtx) -> Result<Value, ApiError> {
        unreachable!("listener has no commands")
    }

    fn recover(&mut self, _entry: &JournalEntry, _ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        Ok(RecoverOutcome::Restored)
    }

    fn on_event(&mut self, event: &Event, _ctx: &ModuleCtx) -> Result<(), ApiError> {
        if self.0.panic_on_event.load(Ordering::SeqCst) && event.topic == "crashy.pinged" {
            panic!("cannot digest {}", event.topic);
        }
        self.0.seen.lock().unwrap().push(event.topic.clone());
        Ok(())
    }
}

fn boot(env: &Env, probe: &Arc<Probe>) -> Arc<daemon::dispatch::Dispatcher> {
    env.start_with(vec![Box::new(Crashy(probe.clone())), Box::new(Listener(probe.clone()))])
}

fn status(d: &daemon::dispatch::Dispatcher) -> Value {
    agent_call(d, "status", json!({})).into_result().unwrap()
}

#[test]
fn panic_in_a_command_faults_only_that_module() {
    let env = Env::new();
    let probe = Arc::new(Probe::default());
    let d = boot(&env, &probe);

    let resp = agent_call(&d, "crashy.boom", json!({}));
    assert_eq!(err_code(&resp).0, ErrorCode::ModuleFaulted);
    assert_eq!(resp.exit_code(), 20);
    // Stays off until restart.
    assert_eq!(err_code(&agent_call(&d, "crashy.ok", json!({}))).0, ErrorCode::ModuleFaulted);

    // Everything else keeps working.
    assert!(agent_call(&d, "tasks", json!({})).ok);
    let st = status(&d);
    let crashy = module_status(&st, "crashy");
    assert_eq!(crashy["health"], "faulted");
    assert!(crashy["fault"].as_str().unwrap().contains("kaboom"));
    assert_eq!(module_status(&st, "listener")["health"], "running");
    wait_events(&d);
    assert_eq!(*probe.seen.lock().unwrap(), ["core.module_faulted"]);
}

#[test]
fn failed_or_panicking_start_faults_only_that_module() {
    for panic in [false, true] {
        let env = Env::new();
        let probe = Arc::new(Probe::default());
        if panic {
            probe.panic_start.store(true, Ordering::SeqCst);
        } else {
            probe.fail_start.store(true, Ordering::SeqCst);
        }
        let d = boot(&env, &probe);
        let st = status(&d);
        let crashy = module_status(&st, "crashy");
        assert_eq!(crashy["health"], "faulted");
        let why = crashy["fault"].as_str().unwrap();
        assert!(why.contains(if panic { "start exploded" } else { "no device" }), "{why}");
        assert_eq!(module_status(&st, "listener")["health"], "running");
        assert_eq!(err_code(&agent_call(&d, "crashy.ok", json!({}))).0, ErrorCode::ModuleFaulted);
        assert!(agent_call(&d, "free", json!({})).ok);
    }
}

#[test]
fn panic_in_an_event_handler_faults_the_subscriber_only() {
    let env = Env::new();
    let probe = Arc::new(Probe::default());
    probe.panic_on_event.store(true, Ordering::SeqCst);
    let d = boot(&env, &probe);
    assert!(agent_call(&d, "crashy.ping", json!({})).ok);
    wait_events(&d);
    let st = status(&d);
    assert_eq!(module_status(&st, "listener")["health"], "faulted");
    assert_eq!(module_status(&st, "crashy")["health"], "running");
    assert!(agent_call(&d, "crashy.ok", json!({})).ok);
}

#[test]
fn modules_talk_through_events_only() {
    let env = Env::new();
    let probe = Arc::new(Probe::default());
    let d = boot(&env, &probe);
    assert!(agent_call(&d, "crashy.ping", json!({})).ok);
    assert!(agent_call(&d, "crashy.ping", json!({})).ok);
    wait_events(&d);
    assert_eq!(*probe.seen.lock().unwrap(), ["crashy.pinged", "crashy.pinged"]);
    // Publishing something not declared in the spec is refused.
    assert_eq!(err_code(&agent_call(&d, "crashy.publish_undeclared", json!({}))).0, ErrorCode::Internal);
}

#[test]
fn a_busy_module_does_not_block_status() {
    let env = Env::new();
    let probe = Arc::new(Probe::default());
    let d = boot(&env, &probe);
    let busy = {
        let d = d.clone();
        std::thread::spawn(move || agent_call(&d, "crashy.slow", json!({"ms": 3000})))
    };
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    let st = status(&d);
    assert!(t.elapsed() < Duration::from_millis(1500), "status took {:?}", t.elapsed());
    assert_eq!(module_status(&st, "crashy")["state"], json!({"busy": true}));
    assert!(busy.join().unwrap().ok);
}

#[test]
fn error_subcodes_are_namespaced_by_module() {
    let env = Env::new();
    let probe = Arc::new(Probe::default());
    let d = boot(&env, &probe);
    let resp = agent_call(&d, "crashy.declared", json!({}));
    assert_eq!(err_code(&resp), (ErrorCode::Conflict, Some("crashy.TOO_HOT".into())));
    assert_eq!(resp.error.unwrap().module.as_deref(), Some("crashy"));
    // Using another module's subcode is a bug in the module, reported as such.
    assert_eq!(err_code(&agent_call(&d, "crashy.foreign_subcode", json!({}))).0, ErrorCode::Internal);
    let h = common::hello(&d, &mut common::raw_session(&d, common::AGENT_EXE));
    let m = h.modules.iter().find(|m| m.name == "crashy").unwrap();
    assert_eq!(m.error_codes, ["crashy.TOO_HOT"]);
    assert_eq!(m.events, ["crashy.pinged"]);
}

#[test]
fn config_switches_modules_on_and_off() {
    let env = Env::new();
    env.write_config(json!({"modules": {"crashy": {"enabled": false}, "example": {"enabled": true}}}));
    let probe = Arc::new(Probe::default());
    let d = boot(&env, &probe);
    let resp = agent_call(&d, "crashy.ok", json!({}));
    assert_eq!(err_code(&resp).0, ErrorCode::ModuleDisabled);
    assert_eq!(resp.exit_code(), 14);
    let st = status(&d);
    assert_eq!(module_status(&st, "crashy")["health"], "disabled");
    assert_eq!(module_status(&st, "example")["health"], "running");
}

#[test]
fn broken_config_file_holds_every_module_but_the_daemon_serves() {
    let env = Env::new();
    // Would switch crashy off, if it could be read: nothing may start on defaults instead.
    std::fs::write(&env.config.config_file, r#"{"modules": {"crashy": {"enabled": false},}}"#).unwrap();
    let probe = Arc::new(Probe::default());
    let d = boot(&env, &probe);
    let h = common::hello(&d, &mut common::raw_session(&d, common::AGENT_EXE));
    for m in h.modules.iter().filter(|m| m.name != "core") {
        assert_eq!((m.health.as_str(), m.enabled), ("config_error", false), "{}", m.name);
    }
    let resp = agent_call(&d, "crashy.ok", json!({}));
    assert_eq!(err_code(&resp), (ErrorCode::ModuleDisabled, Some(subcode::CONFIG_INVALID.into())));
    assert!(resp.error.unwrap().message.contains("config"));
    assert!(status(&d)["daemon"]["config_error"].as_str().is_some());
    // Not running, so no events either.
    assert!(err_code(&agent_call(&d, "crashy.ping", json!({}))).0 == ErrorCode::ModuleDisabled);
    wait_events(&d);
    assert!(probe.seen.lock().unwrap().is_empty());
}

#[test]
fn administrator_operations_go_through_the_narrow_layer() {
    // Fake backend: the call is recorded, nothing real happens.
    let env = Env::new();
    let probe = Arc::new(Probe::default());
    let d = boot(&env, &probe);
    let resp = agent_call(&d, "crashy.elevated", json!({})).into_result().unwrap();
    assert_eq!(resp["backend"], "fake");
    assert_eq!(*env.elevated.calls.lock().unwrap(), ["terminate_process(9)"]);
    drop(d);

    // Today's real backend runs as a normal user and refuses without doing anything.
    let mut env = Env::new();
    env.config.sys.elevated = Arc::new(platform::UserModeElevated);
    let d = boot(&env, &probe);
    let resp = agent_call(&d, "crashy.elevated", json!({}));
    assert_eq!(err_code(&resp), (ErrorCode::Denied, Some(subcode::NEEDS_ELEVATION.into())));
}

#[test]
fn bad_module_specs_are_refused_at_registration() {
    static DUP_SHORT: ModuleSpec = ModuleSpec::new("dupshort", Section::Usage, "")
        .commands(&[CommandSpec::new("x", "").short("protect").agent()]);
    static RESERVED_ARG: ModuleSpec = ModuleSpec::new("reserved", Section::Usage, "")
        .commands(&[CommandSpec::new("x", "").agent().args(&[ArgSpec::new("json", ArgKind::Bool, "")])]);
    static SHORT_IS_MODULE: ModuleSpec = ModuleSpec::new("clash", Section::Usage, "")
        .commands(&[CommandSpec::new("x", "").short("task").agent()]);
    static BAD_NAME: ModuleSpec = ModuleSpec::new("Bad-Name", Section::Usage, "");

    struct Bare(&'static ModuleSpec);
    impl Module for Bare {
        fn spec(&self) -> &'static ModuleSpec {
            self.0
        }
        fn handle(&mut self, _req: &ModuleRequest<'_>, _ctx: &ModuleCtx) -> Result<Value, ApiError> {
            Ok(json!({}))
        }
        fn recover(&mut self, _entry: &JournalEntry, _ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
            Ok(RecoverOutcome::Restored)
        }
    }

    for spec in [&DUP_SHORT, &RESERVED_ARG, &SHORT_IS_MODULE, &BAD_NAME, &CRASHY] {
        let env = Env::new();
        let mut modules = daemon::modules::builtin();
        modules.push(Box::new(Bare(spec)));
        if spec.name == "crashy" {
            modules.push(Box::new(Bare(spec))); // registered twice
        }
        match daemon::start(&env.config, modules) {
            Err(daemon::DaemonError::Registry(_)) => {}
            Err(e) => panic!("{}: wrong error: {e}", spec.name),
            Ok(_) => panic!("{}: accepted", spec.name),
        }
    }
}
