//! Per-module versioned migrations: each module owns its `<module>_*` tables and their
//! upgrade steps; the database is upgraded in place, never rebuilt.

mod common;

use common::{Env, agent_call, err_code, module_status};
use daemon::sqlite::Value as Sql;
use daemon::store::Store;
use daemon::{ArgKind, ArgSpec, CommandSpec, JournalEntry, Migration, Module, ModuleCtx, ModuleRequest, ModuleSpec};
use daemon::{RecoverOutcome, Section};
use proto::{ApiError, ErrorCode};
use serde_json::{Value, json};

const COMMANDS: &[CommandSpec] = &[
    CommandSpec::new("add", "Add a note").agent().args(&[ArgSpec::new("text", ArgKind::Str, "").required()]),
    CommandSpec::new("list", "List notes").agent().read_only(),
];
const V1: Migration = Migration::new(1, "CREATE TABLE notes_items (id INTEGER PRIMARY KEY, text TEXT NOT NULL)");
const V2: Migration = Migration::new(
    2,
    "ALTER TABLE notes_items ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0; \
     CREATE INDEX notes_items_pinned ON notes_items (pinned)",
);

static NOTES_V1: ModuleSpec = ModuleSpec::new("notes", Section::AccountsStorage, "").commands(COMMANDS).migrations(&[V1]);
static NOTES_V2: ModuleSpec =
    ModuleSpec::new("notes", Section::AccountsStorage, "").commands(COMMANDS).migrations(&[V1, V2]);
static WRONG_PREFIX: ModuleSpec = ModuleSpec::new("sloppy", Section::Usage, "")
    .commands(COMMANDS)
    .migrations(&[Migration::new(1, "CREATE TABLE sloppy_ok (id INTEGER); CREATE TABLE other_table (id INTEGER)")]);
static BROKEN_SQL: ModuleSpec = ModuleSpec::new("broken", Section::Usage, "")
    .commands(COMMANDS)
    .migrations(&[V1_BROKEN_OK, Migration::new(2, "ALTER TABLE broken_items ADD COLUMN; garbage")]);
const V1_BROKEN_OK: Migration = Migration::new(1, "CREATE TABLE broken_items (id INTEGER PRIMARY KEY, text TEXT)");

struct Notes(&'static ModuleSpec);

impl Module for Notes {
    fn spec(&self) -> &'static ModuleSpec {
        self.0
    }

    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        let table = format!("{}_items", self.0.name);
        match req.cmd {
            "add" => {
                let text = req.args["text"].as_str().unwrap();
                ctx.db().execute(&format!("INSERT INTO {table} (text) VALUES (?)"), &[text.into()])?;
                Ok(json!({}))
            }
            "list" => {
                let rows = ctx.db().query(&format!("SELECT text FROM {table} ORDER BY id"), &[])?;
                Ok(json!(rows.iter().map(|r| r.text(0).unwrap()).collect::<Vec<_>>()))
            }
            other => Err(ApiError::internal(format!("unrouted {other}"))),
        }
    }

    fn recover(&mut self, _entry: &JournalEntry, _ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        Ok(RecoverOutcome::Restored)
    }
}

fn status(d: &daemon::dispatch::Dispatcher) -> Value {
    agent_call(d, "status", json!({})).into_result().unwrap()
}

fn table_exists(env: &Env, name: &str) -> bool {
    let store = Store::open(&env.db_path(), common::sqlite().as_ref()).unwrap();
    store
        .with_conn(|c| c.query_row("SELECT 1 FROM sqlite_master WHERE name = ?", &[name.into()]))
        .unwrap()
        .is_some()
}

#[test]
fn upgrade_keeps_existing_data() {
    let env = Env::new();
    let d = env.start_with(vec![Box::new(Notes(&NOTES_V1))]);
    assert_eq!(module_status(&status(&d), "notes")["schema_version"], 1);
    assert!(agent_call(&d, "notes.add", json!({"text": "before upgrade"})).ok);
    drop(d);

    let d = env.start_with(vec![Box::new(Notes(&NOTES_V2))]);
    let st = status(&d);
    assert_eq!(module_status(&st, "notes")["health"], "running");
    assert_eq!(module_status(&st, "notes")["schema_version"], 2);
    assert!(agent_call(&d, "notes.add", json!({"text": "after"})).ok);
    assert_eq!(agent_call(&d, "notes.list", json!({})).into_result().unwrap(), json!(["before upgrade", "after"]));
    let pinned = d.store().with_conn(|c| c.query("SELECT pinned FROM notes_items", &[])).unwrap();
    assert!(pinned.iter().all(|r| r.get(0) == &Sql::Int(0)));
    // Other components keep their own versions.
    assert_eq!(d.store().schema_version("core").unwrap(), 1);
    assert_eq!(d.store().schema_version("task").unwrap(), 2);
}

#[test]
fn tables_outside_the_module_prefix_are_refused_and_rolled_back() {
    let env = Env::new();
    let d = env.start_with(vec![Box::new(Notes(&WRONG_PREFIX)), Box::new(Notes(&NOTES_V1))]);
    let st = status(&d);
    let sloppy = module_status(&st, "sloppy");
    assert_eq!(sloppy["health"], "faulted");
    assert!(sloppy["fault"].as_str().unwrap().contains("other_table"), "{sloppy}");
    assert_eq!(sloppy["schema_version"], 0);
    assert_eq!(err_code(&agent_call(&d, "sloppy.list", json!({}))).0, ErrorCode::ModuleFaulted);
    // Unaffected module.
    assert!(agent_call(&d, "notes.add", json!({"text": "fine"})).ok);
    drop(d);
    assert!(!table_exists(&env, "other_table"));
    assert!(!table_exists(&env, "sloppy_ok"), "the whole step is rolled back");
}

#[test]
fn failing_step_faults_the_module_and_keeps_the_last_good_version() {
    let env = Env::new();
    let d = env.start_with(vec![Box::new(Notes(&BROKEN_SQL))]);
    let st = status(&d);
    let broken = module_status(&st, "broken");
    assert_eq!(broken["health"], "faulted");
    assert_eq!(broken["schema_version"], 1);
    assert!(agent_call(&d, "status", json!({})).ok);
}

#[test]
fn database_newer_than_the_module_is_not_touched() {
    let env = Env::new();
    drop(env.start_with(vec![Box::new(Notes(&NOTES_V2))]));
    // An older build of the module.
    let d = env.start_with(vec![Box::new(Notes(&NOTES_V1))]);
    let st = status(&d);
    let notes = module_status(&st, "notes");
    assert_eq!(notes["health"], "faulted");
    assert_eq!(notes["schema_version"], 2);
    assert!(agent_call(&d, "free", json!({})).ok);
}

#[test]
fn core_schema_newer_than_the_daemon_refuses_to_start() {
    let env = Env::new();
    drop(env.start(daemon::modules::builtin()));
    let store = Store::open(&env.db_path(), common::sqlite().as_ref()).unwrap();
    store.with_conn(|c| c.execute("UPDATE schema_versions SET version = 99 WHERE component = 'core'", &[])).unwrap();
    drop(store);
    match daemon::start(&env.config, daemon::modules::builtin()) {
        Err(daemon::DaemonError::Store(e)) => assert!(e.contains("newer"), "{e}"),
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("started on a newer database"),
    }
}
