//! Test helpers: a temp data folder with fake system calls, sessions for agent and trusted
//! callers, and a test module ("knob") that changes a fake system setting through the
//! recovery journal. Tests never touch the real system; only the SQLite library is real.

#![allow(dead_code)]

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use daemon::dispatch::{Dispatcher, Session};
use daemon::sqlite::SqliteLibrary;
use daemon::store::Store;
use daemon::sys::fake::{FakeElevated, FakeSystem};
use daemon::{
    ArgKind, ArgSpec, Caller, CommandSpec, DaemonConfig, JournalEntry, Migration, Module,
    ModuleCtx, ModuleRequest, ModuleSpec, RecoverOutcome, Section, TrustPolicy,
};
use proto::catalog::HelloResult;
use proto::{ApiError, Response};
use serde_json::{Value, json};

/// The page executable the test trust policy accepts.
pub const UI_EXE: &str = r"C:\test\away-guard-ui.exe";
pub const AGENT_EXE: &str = r"C:\test\some-agent.exe";

pub fn sqlite() -> Arc<dyn SqliteLibrary> {
    platform::Winsqlite3::load().expect("winsqlite3.dll is part of Windows")
}

pub struct Env {
    pub dir: tempfile::TempDir,
    pub config: DaemonConfig,
    pub system: Arc<FakeSystem>,
    pub elevated: Arc<FakeElevated>,
}

impl Env {
    pub fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let (sys, system, elevated) = daemon::sys::fake::sys();
        let config = DaemonConfig {
            endpoint: unique_endpoint("unit"),
            data_dir: dir.path().to_owned(),
            config_file: dir.path().join(proto::names::CONFIG_FILE),
            sys,
            sqlite: sqlite(),
            trust: TrustPolicy::trusting([UI_EXE]),
        };
        Env {
            dir,
            config,
            system,
            elevated,
        }
    }

    /// Writes config.json (module switches).
    pub fn write_config(&self, v: Value) {
        std::fs::write(&self.config.config_file, v.to_string()).unwrap();
    }

    pub fn db_path(&self) -> std::path::PathBuf {
        self.dir.path().join(proto::names::DB_FILE)
    }

    pub fn start(&self, modules: Vec<Box<dyn Module>>) -> Arc<Dispatcher> {
        daemon::start(&self.config, modules).unwrap()
    }

    /// The built-in modules plus `extra`.
    pub fn start_with(&self, extra: Vec<Box<dyn Module>>) -> Arc<Dispatcher> {
        let mut m = daemon::modules::builtin();
        m.extend(extra);
        self.start(m)
    }
}

pub fn unique_endpoint(tag: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!(
        r"\\.\pipe\away-guard-test-{tag}-{}-{}-{nanos}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    )
}

/// A connected session that has not done the handshake yet.
pub fn raw_session(d: &Dispatcher, exe: &str) -> Session {
    d.session_for(Caller {
        pid: Some(4242),
        exe: Some(exe.into()),
        name: None,
        kind: Default::default(),
    })
}

pub fn hello(d: &Dispatcher, s: &mut Session) -> HelloResult {
    let resp = send(
        d,
        s,
        1,
        "core.hello",
        json!({"protocols": [1], "client": {"name": "test"}}),
    );
    serde_json::from_value(resp.into_result().unwrap()).unwrap()
}

/// An agent (CLI, script) session after the handshake.
pub fn agent(d: &Dispatcher) -> Session {
    let mut s = raw_session(d, AGENT_EXE);
    hello(d, &mut s);
    s
}

/// A page/tray session after the handshake.
pub fn trusted(d: &Dispatcher) -> Session {
    let mut s = raw_session(d, UI_EXE);
    hello(d, &mut s);
    s
}

/// Sends one request line with protocol version `v`.
pub fn send(d: &Dispatcher, s: &mut Session, v: u32, cmd: &str, args: Value) -> Response {
    static ID: AtomicU64 = AtomicU64::new(1);
    let id = format!("t-{}", ID.fetch_add(1, Ordering::SeqCst));
    let line = json!({ "v": v, "id": id, "cmd": cmd, "args": args }).to_string();
    let resp = d.handle_line(s, line.as_bytes());
    if let Some(got) = &resp.id {
        assert_eq!(got, &id, "reply id");
    }
    resp
}

pub fn call(d: &Dispatcher, s: &mut Session, cmd: &str, args: Value) -> Response {
    send(d, s, 1, cmd, args)
}

/// Calls as a fresh agent session.
pub fn agent_call(d: &Dispatcher, cmd: &str, args: Value) -> Response {
    call(d, &mut agent(d), cmd, args)
}

pub fn err_code(resp: &Response) -> (proto::ErrorCode, Option<String>) {
    let e = resp
        .error
        .as_ref()
        .unwrap_or_else(|| panic!("expected an error, got {resp:?}"));
    (e.code, e.subcode.clone())
}

pub fn wait_events(d: &Dispatcher) {
    assert!(
        d.registry().bus().wait_idle(Duration::from_secs(5)),
        "event delivery did not settle"
    );
}

pub fn module_status<'a>(status: &'a Value, name: &str) -> &'a Value {
    status["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == name)
        .unwrap_or_else(|| panic!("{name} missing"))
}

// --- knob: a module that changes a fake system setting ------------------------------------

pub const KNOB: &str = "knob";

/// Stands in for the operating system: survives daemon "crashes".
#[derive(Default)]
pub struct Machine {
    pub knob: Mutex<i64>,
    pub recover_calls: AtomicU32,
    pub fail_recover: AtomicBool,
    pub panic_on_recover: AtomicBool,
    pub adopt_on_recover: AtomicBool,
    pub stop_calls: AtomicU32,
}

impl Machine {
    pub fn at(v: i64) -> Arc<Machine> {
        let m = Arc::new(Machine::default());
        *m.knob.lock().unwrap() = v;
        m
    }

    pub fn knob(&self) -> i64 {
        *self.knob.lock().unwrap()
    }

    fn set(&self, v: i64) {
        *self.knob.lock().unwrap() = v;
    }
}

const VALUE: ArgSpec = ArgSpec::new("value", ArgKind::Int, "New value").required();

const KNOB_COMMANDS: &[CommandSpec] = &[
    CommandSpec::new("set", "Change the setting (journal first)")
        .agent()
        .args(&[VALUE]),
    CommandSpec::new("reset", "Put the original back").agent(),
    CommandSpec::new("set_then_panic", "Record, then die before changing")
        .agent()
        .args(&[VALUE]),
    CommandSpec::new(
        "check_journal_first",
        "Prove the journal is on disk before the change",
    )
    .agent()
    .args(&[
        VALUE,
        ArgSpec::new("db", ArgKind::Str, "Database file").required(),
    ]),
    CommandSpec::new("peek", "Read the setting")
        .agent()
        .read_only(),
];

pub static KNOB_SPEC: ModuleSpec = ModuleSpec::new(
    KNOB,
    Section::Usage,
    "Test module that changes a fake setting",
)
.commands(KNOB_COMMANDS);

/// The same module in a later build whose first table step is broken: it faults at startup.
pub static KNOB_BROKEN_MIGRATION_SPEC: ModuleSpec = ModuleSpec::new(
    KNOB,
    Section::Usage,
    "Test module that changes a fake setting",
)
.commands(KNOB_COMMANDS)
.migrations(&[Migration::new(
    1,
    "CREATE TABLE knob_state (v INTEGER); THIS IS NOT SQL",
)]);

pub struct KnobModule {
    pub machine: Arc<Machine>,
    pub sqlite: Arc<dyn SqliteLibrary>,
    pub spec: &'static ModuleSpec,
}

pub fn knob(machine: &Arc<Machine>) -> Box<dyn Module> {
    knob_with_spec(machine, &KNOB_SPEC)
}

pub fn knob_with_spec(machine: &Arc<Machine>, spec: &'static ModuleSpec) -> Box<dyn Module> {
    Box::new(KnobModule {
        machine: machine.clone(),
        sqlite: sqlite(),
        spec,
    })
}

fn value(req: &ModuleRequest<'_>) -> i64 {
    req.args["value"]
        .as_i64()
        .expect("validated by the framework")
}

impl Module for KnobModule {
    fn spec(&self) -> &'static ModuleSpec {
        self.spec
    }

    fn stop(&mut self, _ctx: &ModuleCtx) -> Result<(), ApiError> {
        self.machine.stop_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        let m = &self.machine;
        match req.cmd {
            // Record first, then change.
            "set" => {
                let v = value(req);
                let entry = ctx
                    .journal()
                    .record(KNOB, &json!(m.knob()), Some(&json!(v)))?;
                m.set(v);
                Ok(json!({ "entry": entry.id }))
            }
            // Normal undo path: put the original back, then close the entry.
            "reset" => {
                if let Some(entry) = ctx.journal().open_entry(KNOB)? {
                    m.set(entry.original.as_i64().unwrap());
                    ctx.journal().mark_restored(entry.id)?;
                }
                Ok(json!({ "knob": m.knob() }))
            }
            // Dies between "record" and "change", like a crash at the worst moment.
            "set_then_panic" => {
                let v = value(req);
                ctx.journal()
                    .record(KNOB, &json!(m.knob()), Some(&json!(v)))?;
                panic!("simulated crash before applying {v}");
            }
            // A separate connection to the same file must already see the entry.
            "check_journal_first" => {
                let v = value(req);
                ctx.journal()
                    .record(KNOB, &json!(m.knob()), Some(&json!(v)))?;
                let db = req.args["db"].as_str().unwrap();
                let other = Store::open(Path::new(db), self.sqlite.as_ref()).unwrap();
                let seen = other
                    .journal_open(Some(KNOB))
                    .unwrap()
                    .iter()
                    .any(|e| e.key == KNOB);
                if !seen {
                    return Err(ApiError::internal(
                        "journal entry not visible before the change",
                    ));
                }
                m.set(v);
                Ok(json!({ "seen_before_change": seen }))
            }
            "peek" => Ok(json!({ "knob": m.knob() })),
            other => Err(ApiError::internal(format!("unrouted {other}"))),
        }
    }

    fn status(&self, _ctx: &ModuleCtx) -> Value {
        json!({ "implemented": true, "knob": self.machine.knob() })
    }

    fn recover(
        &mut self,
        entry: &JournalEntry,
        _ctx: &ModuleCtx,
    ) -> Result<RecoverOutcome, ApiError> {
        let m = &self.machine;
        m.recover_calls.fetch_add(1, Ordering::SeqCst);
        if m.fail_recover.load(Ordering::SeqCst) {
            return Err(ApiError::internal("setting is locked"));
        }
        if m.panic_on_recover.load(Ordering::SeqCst) {
            panic!("recover exploded");
        }
        if m.adopt_on_recover.load(Ordering::SeqCst) {
            return Ok(RecoverOutcome::Adopted);
        }
        assert_eq!(entry.key, KNOB);
        m.set(entry.original.as_i64().unwrap());
        Ok(RecoverOutcome::Restored)
    }
}
