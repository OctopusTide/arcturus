//! The recovery journal end to end, with the knob module standing in for a real one:
//! record before change, crash, restart, recover.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{Env, KNOB_BROKEN_MIGRATION_SPEC, Machine, agent_call, err_code, knob, knob_with_spec};
use daemon::dispatch::Dispatcher;
use proto::{ErrorCode, subcode};
use serde_json::json;

fn boot(env: &Env, m: &Arc<Machine>) -> Arc<Dispatcher> {
    env.start_with(vec![knob(m)])
}

#[test]
fn crash_then_restart_puts_the_original_back() {
    let env = Env::new();
    let m = Machine::at(7);

    let d = boot(&env, &m);
    assert!(agent_call(&d, "knob.set", json!({"value": 99})).ok);
    // Changing the same setting again must not overwrite the recorded original.
    assert!(agent_call(&d, "knob.set", json!({"value": 100})).ok);
    assert_eq!(m.knob(), 100);
    let open = d.store().journal_open(None).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].original, json!(7));
    assert_eq!(open[0].applied, Some(json!(100)));
    let entry_id = open[0].id;

    // Crash: no shutdown, so the module never undoes its change.
    drop(d);
    assert_eq!(m.knob(), 100);

    let d = boot(&env, &m);
    assert_eq!(m.knob(), 7, "recovery must restore the original");
    assert_eq!(m.recover_calls.load(Ordering::SeqCst), 1);
    assert_eq!(d.recovery().restored, 1);
    assert!(d.store().journal_open(None).unwrap().is_empty());
    assert_eq!(d.store().journal_status(entry_id).unwrap(), Some(("restored".into(), Some("startup".into()))));

    let status = agent_call(&d, "status", json!({})).into_result().unwrap();
    assert_eq!(status["journal"]["open"], json!(0));
    assert_eq!(status["journal"]["last_recovery"]["restored"], json!(1));

    // Nothing left to do on the next start.
    drop(d);
    let d = boot(&env, &m);
    assert_eq!(d.recovery().restored, 0);
    assert_eq!(m.recover_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn normal_undo_leaves_nothing_to_recover() {
    let env = Env::new();
    let m = Machine::at(1);
    let d = boot(&env, &m);
    assert!(agent_call(&d, "knob.set", json!({"value": 5})).ok);
    assert!(agent_call(&d, "knob.reset", json!({})).ok);
    assert_eq!(m.knob(), 1);
    drop(d);

    let d = boot(&env, &m);
    assert_eq!(m.recover_calls.load(Ordering::SeqCst), 0);
    assert_eq!(d.recovery().restored, 0);
}

#[test]
fn crash_between_record_and_change_is_harmless() {
    let env = Env::new();
    let m = Machine::at(3);
    let d = boot(&env, &m);
    let resp = agent_call(&d, "knob.set_then_panic", json!({"value": 50}));
    assert_eq!(err_code(&resp).0, ErrorCode::ModuleFaulted);
    assert_eq!(m.knob(), 3, "the change was never applied");
    assert_eq!(d.store().journal_open(None).unwrap().len(), 1);
    drop(d);

    let d = boot(&env, &m);
    assert_eq!(m.knob(), 3);
    assert_eq!(d.recovery().restored, 1);
    assert!(d.store().journal_open(None).unwrap().is_empty());
    assert!(agent_call(&d, "knob.peek", json!({})).ok, "a restart clears the fault");
}

#[test]
fn failed_recovery_stays_open_and_is_retried() {
    let env = Env::new();
    let m = Machine::at(1);
    let d = boot(&env, &m);
    assert!(agent_call(&d, "knob.set", json!({"value": 9})).ok);
    drop(d);

    m.fail_recover.store(true, Ordering::SeqCst);
    let d = boot(&env, &m);
    assert_eq!(d.recovery().failed, 1);
    assert_eq!(m.knob(), 9);
    let open = d.store().journal_open(None).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].attempts, 1);
    assert_eq!(open[0].last_error.as_deref(), Some("setting is locked"));
    drop(d);

    m.fail_recover.store(false, Ordering::SeqCst);
    let d = boot(&env, &m);
    assert_eq!(d.recovery().restored, 1);
    assert_eq!(m.knob(), 1);
}

#[test]
fn adopted_entry_stays_open() {
    let env = Env::new();
    let m = Machine::at(1);
    let d = boot(&env, &m);
    assert!(agent_call(&d, "knob.set", json!({"value": 2})).ok);
    drop(d);

    m.adopt_on_recover.store(true, Ordering::SeqCst);
    let d = boot(&env, &m);
    assert_eq!(d.recovery().adopted, 1);
    assert_eq!(m.knob(), 2);
    assert_eq!(d.store().journal_open(None).unwrap().len(), 1);
}

#[test]
fn entry_of_missing_module_is_reported_then_recovered_later() {
    let env = Env::new();
    let m = Machine::at(4);
    let d = boot(&env, &m);
    assert!(agent_call(&d, "knob.set", json!({"value": 8})).ok);
    drop(d);

    // Started without the knob module (renamed or removed).
    let d = env.start(daemon::modules::builtin());
    assert_eq!(d.recovery().orphaned, 1);
    assert_eq!(d.store().journal_open(None).unwrap().len(), 1);
    drop(d);

    let d = boot(&env, &m);
    assert_eq!(d.recovery().restored, 1);
    assert_eq!(m.knob(), 4);
}

#[test]
fn recovery_runs_for_switched_off_modules() {
    let env = Env::new();
    let m = Machine::at(10);
    let d = boot(&env, &m);
    assert!(agent_call(&d, "knob.set", json!({"value": 11})).ok);
    drop(d);

    env.write_config(json!({"modules": {"knob": {"enabled": false}}}));
    let d = boot(&env, &m);
    assert_eq!(m.knob(), 10);
    assert_eq!(d.recovery().restored, 1);
    let resp = agent_call(&d, "knob.peek", json!({}));
    assert_eq!(err_code(&resp).0, ErrorCode::ModuleDisabled);
}

#[test]
fn broken_config_file_still_recovers_and_starts_no_module() {
    let env = Env::new();
    let m = Machine::at(7);
    let d = boot(&env, &m);
    assert!(agent_call(&d, "knob.set", json!({"value": 99})).ok);
    drop(d); // crash with the setting changed

    std::fs::write(&env.config.config_file, "{ \"modules\": { broken").unwrap();
    let d = boot(&env, &m);
    assert_eq!(m.knob(), 7, "recovery must run even though the config file is broken");
    assert_eq!(d.recovery().restored, 1);
    assert!(d.store().journal_open(None).unwrap().is_empty());

    // The daemon answers; status says what is wrong; no module was started.
    let st = agent_call(&d, "status", json!({})).into_result().unwrap();
    let why = st["daemon"]["config_error"].as_str().expect("config_error in status");
    assert!(why.contains(proto::names::CONFIG_FILE), "{why}");
    for module in st["modules"].as_array().unwrap() {
        assert_eq!(module["health"], "config_error", "{module}");
        assert_eq!(module["enabled"], false, "{module}");
    }
    for (cmd, args) in [("knob.peek", json!({})), ("away", json!({})), ("free", json!({})), ("example.echo", json!({"text": "x"}))] {
        let resp = agent_call(&d, cmd, args);
        assert_eq!(err_code(&resp), (ErrorCode::ModuleDisabled, Some(subcode::CONFIG_INVALID.into())), "{cmd}");
    }
    assert!(agent_call(&d, "log", json!({})).ok);
    d.shutdown();
    assert_eq!(m.stop_calls.load(Ordering::SeqCst), 0, "never started, never stopped");
    drop(d);

    // Fixed file: back to normal.
    env.write_config(json!({}));
    let d = boot(&env, &m);
    assert!(agent_call(&d, "knob.peek", json!({})).ok);
    assert!(agent_call(&d, "status", json!({})).into_result().unwrap()["daemon"]["config_error"].is_null());
}

#[test]
fn module_whose_migration_failed_still_recovers() {
    let env = Env::new();
    let m = Machine::at(7);
    let d = boot(&env, &m);
    assert!(agent_call(&d, "knob.set", json!({"value": 99})).ok);
    drop(d);

    // The next build of the module has a broken table step, so it faults at startup.
    let d = env.start_with(vec![knob_with_spec(&m, &KNOB_BROKEN_MIGRATION_SPEC)]);
    assert_eq!(m.knob(), 7, "a faulted module still puts its settings back");
    assert_eq!(d.recovery().restored, 1);
    assert!(d.store().journal_open(None).unwrap().is_empty());
    let st = agent_call(&d, "status", json!({})).into_result().unwrap();
    let knob = common::module_status(&st, "knob");
    assert_eq!(knob["health"], "faulted");
    assert!(knob["fault"].as_str().unwrap().contains("migration"), "{knob}");
    assert_eq!(err_code(&agent_call(&d, "knob.peek", json!({}))).0, ErrorCode::ModuleFaulted);
}

#[test]
fn faulted_module_whose_recovery_panics_is_contained_and_retried() {
    let env = Env::new();
    let m = Machine::at(7);
    let d = boot(&env, &m);
    assert!(agent_call(&d, "knob.set", json!({"value": 99})).ok);
    drop(d);

    m.panic_on_recover.store(true, Ordering::SeqCst);
    let d = env.start_with(vec![knob_with_spec(&m, &KNOB_BROKEN_MIGRATION_SPEC)]);
    assert_eq!(m.recover_calls.load(Ordering::SeqCst), 1, "recover was called");
    assert_eq!(d.recovery().failed, 1);
    let open = d.store().journal_open(None).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].attempts, 1);
    assert!(open[0].last_error.as_deref().unwrap().contains("recover exploded"), "{:?}", open[0].last_error);
    // Everything else is unaffected.
    let st = agent_call(&d, "status", json!({})).into_result().unwrap();
    assert_eq!(common::module_status(&st, "task")["health"], "running");
    assert!(agent_call(&d, "free", json!({})).ok);
    drop(d);

    m.panic_on_recover.store(false, Ordering::SeqCst);
    let d = boot(&env, &m);
    assert_eq!(d.recovery().restored, 1);
    assert_eq!(m.knob(), 7);
}

#[test]
fn original_is_on_disk_before_the_change() {
    let env = Env::new();
    let m = Machine::at(0);
    let d = boot(&env, &m);
    let db = env.db_path();
    let resp = agent_call(&d, "knob.check_journal_first", json!({"value": 1, "db": db.to_str().unwrap()}));
    assert_eq!(resp.into_result().unwrap()["seen_before_change"], json!(true));
    assert_eq!(m.knob(), 1);
}

#[test]
fn shutdown_stops_modules_once() {
    let env = Env::new();
    let m = Machine::at(0);
    let d = boot(&env, &m);
    d.shutdown();
    assert_eq!(m.stop_calls.load(Ordering::SeqCst), 1);
    d.shutdown();
    assert_eq!(m.stop_calls.load(Ordering::SeqCst), 1);
}
