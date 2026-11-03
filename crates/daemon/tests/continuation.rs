//! A continuation is part of the work: away mode, power and protection wait for it, and it runs
//! the way the original ran. Real modules, simulated system.
mod common;
use common::{Env, agent_call, module_status, wait_events};
use daemon::dispatch::Dispatcher;
use serde_json::{Value, json};
use std::sync::Arc;

const URL: &str = "http://www.msftconnecttest.com/connecttest.txt";

fn ok(d: &Dispatcher, cmd: &str, args: Value) -> Value {
    agent_call(d, cmd, args).into_result().unwrap()
}
fn start(env: &Env) -> Arc<Dispatcher> {
    env.system.spawn(100, 0, "claude.exe").spawn(4242, 100, "away-guard-hook.exe");
    {
        let mut images = env.system.processes.lock().unwrap();
        images.insert(100, r"C:\fake\claude.exe".into());
        images.insert(4242, r"C:\fake\away-guard-hook.exe".into());
    }
    env.system.net.reply(URL, "Microsoft Connect Test");
    let d = env.start(vec![
        daemon::modules::mode::create(),
        daemon::modules::task::create(),
        daemon::modules::agents::create(),
        daemon::modules::network::create(),
        daemon::modules::resume::create(),
        daemon::modules::power::create(),
    ]);
    wait_events(&d);
    ok(&d, "network.check", json!({}));
    wait_events(&d);
    d
}
fn hook(d: &Dispatcher, event: Value) {
    ok(d, "agents.hook", json!({"agent":"claude-code","event":event}));
    wait_events(d);
}
fn prompt(d: &Dispatcher, permission_mode: &str) {
    hook(d, json!({"kind":"prompt_submit","session_id":"session-1","permission_mode":permission_mode}));
}
fn failure_event() -> Value {
    json!({"kind":"turn_end","session_id":"session-1","native_event":"StopFailure","cwd":r"D:\work",
        "outcome":{"kind":"api_error","failure":{"network":true,"class":"network","code":"unknown"}},
        "resume_context_complete":true,"resume_environment":{}})
}
fn failure(d: &Dispatcher) {
    hook(d, failure_event());
}
fn offline(env: &Env, d: &Dispatcher) {
    env.system.net.replies.lock().unwrap().remove(URL);
    for _ in 0..3 {
        ok(d, "network.check", json!({}));
    }
    wait_events(d);
}
fn online(env: &Env, d: &Dispatcher) {
    env.system.net.reply(URL, "Microsoft Connect Test");
    ok(d, "network.check", json!({}));
    wait_events(d);
}
fn tick(d: &Dispatcher) -> Value {
    let v = ok(d, "resume.check", json!({}));
    wait_events(d);
    v
}
/// The agent has exited and the task module has noticed.
fn agent_exits(env: &Env, d: &Dispatcher, pid: u32) {
    env.system.exit(4242);
    env.system.exit(pid);
    ok(d, "task.check", json!({}));
    wait_events(d);
}
fn mode(d: &Dispatcher) -> Value {
    ok(d, "mode.show", json!({}))
}
fn protected_failed_and_gone(env: &Env, d: &Dispatcher) {
    ok(d, "protect", json!({"pid":100,"label":"job","mem_limit_mb":500,"deps":["wsl"]}));
    ok(d, "away", json!({}));
    wait_events(d);
    offline(env, d);
    prompt(d, "bypassPermissions");
    failure(d);
    agent_exits(env, d, 100);
    assert_eq!(tick(d)["entries"][0]["state"], "waiting");
    let m = mode(d);
    assert_eq!(m["mode"], "away", "{m}");
    assert_eq!(m["tasks_active"], 0);
    assert_eq!(m["continuations"], 1);
}

#[test]
fn away_mode_and_protection_last_through_the_continuation() {
    let env = Env::new();
    let d = start(&env);
    protected_failed_and_gone(&env, &d);
    online(&env, &d);
    tick(&d);
    {
        let calls = env.system.resume_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].session_id, "session-1");
        assert_eq!(calls[0].permission_mode.as_deref(), Some("bypassPermissions"));
    }
    assert_eq!(mode(&d)["mode"], "away");
    let tasks = ok(&d, "task.list", json!({}))["tasks"].clone();
    assert_eq!(tasks.as_array().unwrap().len(), 1, "{tasks}");
    assert_eq!(tasks[0]["label"], "job");
    assert_eq!(tasks[0]["mem_limit_mb"], 500);
    assert_eq!(tasks[0]["root_pid"], 900_000);
    assert!(tasks[0]["deps"].as_array().unwrap().iter().any(|x| x["name"] == "wsl"));
    assert!(!mode(&d)["state"]["tasks_done_waiting"].as_bool().unwrap());

    // The continuation finishes its turn and exits: now the work really is done.
    env.system.spawn(4242, 900_000, "away-guard-hook.exe");
    hook(&d, json!({"kind":"turn_end","session_id":"session-1","outcome":{"kind":"completed"}}));
    assert_eq!(tick(&d)["entries"][0]["state"], "completed");
    assert_eq!(mode(&d)["mode"], "away", "its task is still running");
    agent_exits(&env, &d, 900_000);
    let m = mode(&d);
    assert_eq!(m["mode"], "normal", "{m}");
    assert_eq!(m["state"]["trigger"], "tasks_done");
    let reports = ok(&d, "mode.reports", json!({}))["reports"].clone();
    assert_eq!(reports[0]["agent_continues"], 1, "{reports}");
    d.registry().stop_all();
}

#[test]
fn giving_up_ends_the_wait_for_away_mode() {
    let env = Env::new();
    let d = start(&env);
    protected_failed_and_gone(&env, &d);
    let id = tick(&d)["entries"][0]["id"].clone();
    ok(&d, "resume.cancel", json!({"id":id}));
    wait_events(&d);
    let m = mode(&d);
    assert_eq!((m["mode"].as_str(), m["state"]["trigger"].as_str()), (Some("normal"), Some("tasks_done")), "{m}");
    d.registry().stop_all();

    let env = Env::new();
    let d = start(&env);
    protected_failed_and_gone(&env, &d);
    env.system.advance_ms(120 * 60_000 + 1);
    assert_eq!(tick(&d)["entries"][0]["state"], "expired");
    let m = mode(&d);
    assert_eq!((m["mode"].as_str(), m["state"]["trigger"].as_str()), (Some("normal"), Some("tasks_done")), "{m}");
    d.registry().stop_all();
}

#[test]
fn the_user_coming_back_still_leaves_at_once_and_the_queue_stays() {
    let env = Env::new();
    let d = start(&env);
    protected_failed_and_gone(&env, &d);
    ok(&d, "back", json!({}));
    wait_events(&d);
    assert_eq!(mode(&d)["mode"], "normal");
    assert_eq!(tick(&d)["entries"][0]["state"], "waiting");
    online(&env, &d);
    tick(&d);
    assert_eq!(env.system.resume_calls.lock().unwrap().len(), 1);
    assert_eq!(mode(&d)["mode"], "normal", "the continuation does not bring away mode back");
    d.registry().stop_all();
}

#[test]
fn power_keeps_the_machine_up_for_a_protected_conversation_waiting_in_normal_mode() {
    let env = Env::new();
    let d = start(&env);
    ok(&d, "protect", json!({"pid":100}));
    offline(&env, &d);
    failure(&d);
    agent_exits(&env, &d, 100);
    let awake = |d: &Dispatcher| module_status(&ok(d, "status", json!({})), "power")["state"]["keep_awake"].clone();
    assert!(awake(&d).is_object(), "{}", awake(&d));
    let id = tick(&d)["entries"][0]["id"].clone();
    ok(&d, "resume.cancel", json!({"id":id}));
    wait_events(&d);
    assert!(awake(&d).is_null(), "{}", awake(&d));
    d.registry().stop_all();
}

#[test]
fn an_unprotected_conversation_stays_unprotected_and_does_not_end_a_button_absence() {
    let env = Env::new();
    let d = start(&env);
    ok(&d, "away", json!({}));
    wait_events(&d);
    failure(&d);
    env.system.exit(100);
    tick(&d);
    assert_eq!(env.system.resume_calls.lock().unwrap().len(), 1);
    assert!(ok(&d, "task.list", json!({}))["tasks"].as_array().unwrap().is_empty());
    env.system.spawn(4242, 900_000, "away-guard-hook.exe");
    hook(&d, json!({"kind":"turn_end","session_id":"session-1","outcome":{"kind":"completed"}}));
    assert_eq!(tick(&d)["entries"][0]["state"], "completed");
    assert_eq!(mode(&d)["mode"], "away");
    d.registry().stop_all();
}

/// The connection context stands in for the broker a real hook leaves behind.
fn session_ends(d: &Dispatcher, session: &str, reason: &str) {
    hook(d, json!({"kind":"session_end","session_id":session,"native_event":"SessionEnd","detail":reason,
        "resume_context_complete":true,"resume_environment":{}}));
}
fn failure_in(d: &Dispatcher, in_a_window: Value) {
    let mut event = failure_event();
    event["in_a_window"] = in_a_window;
    hook(d, event);
}

#[test]
fn a_conversation_a_person_ended_is_not_continued() {
    for (in_a_window, reason, ended) in [
        (json!(true), "prompt_input_exit", true),
        (json!(true), "clear", true),
        (json!(true), "other", true),
        (json!(false), "prompt_input_exit", true),
        // The end of a `-p` run says the same as a closed window, and is the usual unattended case.
        (json!(false), "other", false),
        (Value::Null, "other", false),
        (json!(true), "something_new", false),
    ] {
        let env = Env::new();
        let d = start(&env);
        offline(&env, &d);
        failure_in(&d, in_a_window.clone());
        session_ends(&d, "session-1", reason);
        env.system.exit(4242);
        env.system.exit(100);
        let expected = if ended { "user_ended" } else { "waiting" };
        assert_eq!(tick(&d)["entries"][0]["state"], expected, "{in_a_window} {reason}");
        online(&env, &d);
        tick(&d);
        let calls = env.system.resume_calls.lock().unwrap().len();
        assert_eq!(calls, usize::from(!ended), "{in_a_window} {reason}");
        d.registry().stop_all();
    }
}

#[test]
fn a_crash_says_nothing_and_is_continued() {
    let env = Env::new();
    let d = start(&env);
    offline(&env, &d);
    failure_in(&d, json!(true));
    env.system.exit(4242);
    env.system.exit(100);
    online(&env, &d);
    tick(&d);
    assert_eq!(env.system.resume_calls.lock().unwrap().len(), 1);
    d.registry().stop_all();
}

#[test]
fn ending_it_lets_away_mode_finish() {
    let env = Env::new();
    let d = start(&env);
    ok(&d, "protect", json!({"pid":100,"label":"job"}));
    ok(&d, "away", json!({}));
    wait_events(&d);
    offline(&env, &d);
    failure_in(&d, json!(true));
    session_ends(&d, "session-1", "prompt_input_exit");
    agent_exits(&env, &d, 100);
    let m = mode(&d);
    assert_eq!((m["mode"].as_str(), m["state"]["trigger"].as_str()), (Some("normal"), Some("tasks_done")), "{m}");
    assert_eq!(m["continuations"], 0);
    d.registry().stop_all();
}

#[test]
fn another_conversation_ending_changes_nothing() {
    let env = Env::new();
    let d = start(&env);
    offline(&env, &d);
    failure_in(&d, json!(true));
    session_ends(&d, "session-2", "prompt_input_exit");
    assert_eq!(tick(&d)["entries"][0]["state"], "waiting");
    d.registry().stop_all();
}

/// The user is told not to go on in the old window, so closing it must not stop the fork.
#[test]
fn closing_the_old_window_leaves_a_started_fork_and_its_retry_alone() {
    let env = Env::new();
    let d = start(&env);
    failure_in(&d, json!(true));
    env.system.advance_ms(2001);
    tick(&d);
    let fork = env.system.resume_calls.lock().unwrap()[0].fork_session_id.clone().expect("the window is open, so it forks");
    session_ends(&d, "session-1", "other");
    assert_eq!(tick(&d)["entries"][0]["state"], "running");

    // The fork fails too: it is a `-p` run, its hook says so, and the old window closing again means nothing.
    offline(&env, &d);
    env.system.spawn(4242, 900_000, "away-guard-hook.exe");
    let mut retry = failure_event();
    retry["session_id"] = json!(fork);
    retry["in_a_window"] = json!(false);
    hook(&d, retry);
    session_ends(&d, "session-1", "other");
    assert_eq!(tick(&d)["entries"][0]["state"], "waiting");
    session_ends(&d, &fork, "other");
    assert_eq!(tick(&d)["entries"][0]["state"], "waiting");
    d.registry().stop_all();
}

#[test]
fn the_last_reported_permission_mode_is_used_and_what_it_means_is_said() {
    for (reported, expected, says) in [
        (vec!["default", "acceptEdits"], Some("acceptEdits"), Some("没人批准")),
        (vec!["bypassPermissions", "default"], Some("default"), Some("没人批准")),
        (vec!["acceptEdits", "bypassPermissions"], Some("bypassPermissions"), None),
        (vec![], None, Some("查不到")),
    ] {
        let env = Env::new();
        let d = start(&env);
        for mode in &reported {
            prompt(&d, mode);
        }
        failure(&d);
        env.system.exit(100);
        tick(&d);
        let calls = env.system.resume_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].permission_mode.as_deref(), expected, "{reported:?}");
        drop(calls);
        let notes = env.system.notifications.lock().unwrap().clone();
        let said = |text: &str| notes.iter().any(|(_, m)| m.contains(text));
        match says {
            Some(text) => assert!(said(text), "{notes:?}"),
            None => assert!(!said("没人批准") && !said("查不到"), "{notes:?}"),
        }
        d.registry().stop_all();
    }
}

#[test]
fn a_broker_that_cannot_be_trusted_is_not_used_but_the_failure_still_counts() {
    let env = Env::new();
    let d = start(&env);
    // Not the hook's child.
    env.system.spawn(5000, 1, "away-guard-hook.exe");
    let info = daemon::sys::System::process_info(&*env.system, 5000).unwrap();
    let mut event = failure_event();
    event["resume_context_complete"] = json!(false);
    event["resume_broker"] = json!(info);
    hook(&d, event);
    env.system.exit(100);
    assert_eq!(tick(&d)["entries"][0]["state"], "needs_user");
    assert!(env.system.resume_calls.lock().unwrap().is_empty());
    assert!(env.system.notifications.lock().unwrap().iter().any(|(_, m)| m.contains("手动继续")));
    d.registry().stop_all();
}

#[test]
fn a_real_child_of_the_hook_is_accepted_as_the_broker() {
    let env = Env::new();
    let d = start(&env);
    env.system.spawn(5000, 4242, "away-guard-hook.exe");
    let info = daemon::sys::System::process_info(&*env.system, 5000).unwrap();
    let mut event = failure_event();
    event["resume_context_complete"] = json!(false);
    event["resume_broker"] = json!(info);
    hook(&d, event);
    env.system.exit(100);
    tick(&d);
    let calls = env.system.resume_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].broker.as_ref().map(|b| b.pid), Some(5000));
    drop(calls);
    d.registry().stop_all();
}
