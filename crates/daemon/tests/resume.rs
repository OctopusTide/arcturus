mod common;
use common::{Env, agent_call, call, trusted, wait_events};
use daemon::dispatch::Dispatcher;
use serde_json::{Value, json};
use std::sync::Arc;
const URL: &str = "http://www.msftconnecttest.com/connecttest.txt";
fn ok(d: &Dispatcher, cmd: &str, args: Value) -> Value {
    agent_call(d, cmd, args).into_result().unwrap()
}
fn start(env: &Env) -> Arc<Dispatcher> {
    env.system
        .spawn(100, 0, "claude.exe")
        .spawn(4242, 100, "away-guard-hook.exe");
    env.system
        .processes
        .lock()
        .unwrap()
        .insert(100, r"C:\fake\claude.exe".into());
    env.system.net.reply(URL, "Microsoft Connect Test");
    let d = env.start(vec![
        daemon::modules::agents::create(),
        daemon::modules::network::create(),
        daemon::modules::resume::create(),
    ]);
    wait_events(&d);
    ok(&d, "network.check", json!({}));
    d
}
fn hook(d: &Dispatcher, agent: &str, kind: &str, outcome: Value) {
    ok(
        d,
        "agents.hook",
        json!({"agent":agent,"event":{"kind":kind,"session_id":"session-123","native_event":"StopFailure","cwd":r"D:\work","outcome":outcome,"resume_context_complete":true,"resume_environment":{"ANTHROPIC_BASE_URL":"https://api.example.test","HTTPS_PROXY":"http://127.0.0.1:7890"}}}),
    );
    wait_events(d);
}
fn failure(d: &Dispatcher) {
    hook(
        d,
        "claude-code",
        "turn_end",
        json!({"kind":"api_error","failure":{"network":true,"class":"network","code":"unknown"}}),
    );
}
fn offline(env: &Env, d: &Dispatcher) {
    env.system.net.replies.lock().unwrap().remove(URL);
    for _ in 0..3 {
        ok(d, "network.check", json!({}));
    }
}
fn tick(d: &Dispatcher) -> Value {
    ok(d, "resume.check", json!({}))
}

#[test]
fn print_mode_end_keeps_queue_and_resumes_after_exit() {
    let env = Env::new();
    let d = start(&env);
    offline(&env, &d);
    failure(&d);
    tick(&d);
    assert!(env.system.resume_calls.lock().unwrap().is_empty());
    env.system.net.reply(URL, "Microsoft Connect Test");
    ok(&d, "network.check", json!({}));
    tick(&d);
    assert!(
        env.system.resume_calls.lock().unwrap().is_empty(),
        "original window still owns the conversation"
    );
    hook(&d, "claude-code", "session_end", Value::Null);
    env.system.exit(100);
    tick(&d);
    let calls = env.system.resume_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].session_id, "session-123");
    assert_eq!(calls[0].cwd.as_deref(), Some(r"D:\work"));
    assert!(calls[0].prompt.contains("重试"));
    assert_eq!(
        calls[0].environment.as_ref().unwrap()["ANTHROPIC_BASE_URL"],
        "https://api.example.test"
    );
    assert!(d.store().oplog_recent(100).unwrap().iter().all(|r| {
        r.args
            .as_ref()
            .is_none_or(|a| !a.to_string().contains("api.example.test"))
    }));
    drop(calls);
    d.registry().stop_all();
}
#[test]
fn same_outage_never_launches_more_than_three_times() {
    let env = Env::new();
    let d = start(&env);
    for index in 0..4 {
        env.system.spawn(100, 0, "claude.exe");
        failure(&d);
        env.system.exit(100);
        env.system.advance_ms(2500);
        tick(&d);
        if index < 3 {
            env.system.exit(900_000 + index);
        }
    }
    assert_eq!(env.system.resume_calls.lock().unwrap().len(), 3);
    assert_eq!(
        ok(&d, "resume.show", json!({}))["entries"][0]["state"],
        "exhausted"
    );
    d.registry().stop_all();
}
#[test]
fn authentication_rate_limit_and_unknown_outcomes_are_not_retried() {
    let env = Env::new();
    let d = start(&env);
    for outcome in [
        json!({"kind":"completed"}),
        json!({"kind":"unknown"}),
        json!({"kind":"api_error","failure":{"network":false,"class":"auth"}}),
        json!({"kind":"api_error","failure":{"network":false,"class":"rate_limit"}}),
    ] {
        hook(&d, "claude-code", "turn_end", outcome);
    }
    assert!(tick(&d)["entries"].as_array().unwrap().is_empty());
    assert!(env.system.resume_calls.lock().unwrap().is_empty());
    d.registry().stop_all();
}
#[test]
fn an_agent_without_reliable_error_hooks_only_notifies() {
    let env = Env::new();
    let d = start(&env);
    hook(
        &d,
        "codex",
        "turn_end",
        json!({"kind":"api_error","failure":{"network":true}}),
    );
    assert!(tick(&d)["entries"].as_array().unwrap().is_empty());
    assert!(env.system.resume_calls.lock().unwrap().is_empty());
    d.registry().stop_all();
}
#[test]
fn wait_expires_after_two_hours_and_user_input_cancels_waiting() {
    let env = Env::new();
    let d = start(&env);
    offline(&env, &d);
    failure(&d);
    env.system.advance_ms(120 * 60_000 + 1);
    assert_eq!(tick(&d)["entries"][0]["state"], "expired");
    assert!(
        env.system
            .notifications
            .lock()
            .unwrap()
            .iter()
            .any(|(_, m)| m.contains("两小时"))
    );
    assert!(env.system.resume_calls.lock().unwrap().is_empty());
    // A new observed outage is a new incident; explicit user activity cancels it.
    env.system.net.reply(URL, "Microsoft Connect Test");
    ok(&d, "network.check", json!({}));
    offline(&env, &d);
    failure(&d);
    hook(&d, "claude-code", "prompt_submit", Value::Null);
    assert_eq!(tick(&d)["entries"][0]["state"], "user_active");
    d.registry().stop_all();
}
#[test]
fn restart_does_not_duplicate_a_running_or_uncertain_launch() {
    let env = Env::new();
    let d = start(&env);
    failure(&d);
    env.system.exit(100);
    tick(&d);
    assert_eq!(env.system.resume_calls.lock().unwrap().len(), 1);
    drop(d);
    let d = env.start(vec![
        daemon::modules::agents::create(),
        daemon::modules::network::create(),
        daemon::modules::resume::create(),
    ]);
    ok(&d, "network.check", json!({}));
    tick(&d);
    assert_eq!(env.system.resume_calls.lock().unwrap().len(), 1);
    d.store()
        .with_conn(|c| c.execute("UPDATE resume_queue SET state='launching'", &[]))
        .unwrap();
    drop(d);
    let d = env.start(vec![daemon::modules::resume::create()]);
    assert_eq!(tick(&d)["entries"][0]["state"], "needs_user");
    assert_eq!(env.system.resume_calls.lock().unwrap().len(), 1);
    d.registry().stop_all();
}
#[test]
fn unknown_identity_and_launch_errors_fail_without_retry_loops() {
    let env = Env::new();
    let d = start(&env);
    env.system.set_other_user(100);
    failure(&d);
    assert!(tick(&d)["entries"].as_array().unwrap().is_empty());
    env.system.spawn(100, 0, "claude.exe");
    *env.system.resume_error.lock().unwrap() = Some(proto::ApiError::internal("unrecognised version"));
    failure(&d);
    env.system.exit(100);
    assert_eq!(tick(&d)["entries"][0]["state"], "needs_user");
    assert!(env.system.resume_calls.lock().unwrap().is_empty());
    d.registry().stop_all();
}
#[test]
fn settings_are_user_only_and_cannot_raise_retry_or_wait_caps() {
    let env = Env::new();
    let d = start(&env);
    assert_eq!(
        agent_call(&d, "resume.configure", json!({"settings":{"enabled":false}}))
            .error
            .unwrap()
            .code,
        proto::ErrorCode::Denied
    );
    for patch in [
        json!({"max_attempts":4}),
        json!({"wait_minutes":121}),
        json!({"prompt":""}),
    ] {
        assert_eq!(
            call(
                &d,
                &mut trusted(&d),
                "resume.configure",
                json!({"settings":patch})
            )
            .error
            .unwrap()
            .code,
            proto::ErrorCode::InvalidArgs
        );
    }
    d.registry().stop_all();
}

#[test]
fn simultaneous_checks_launch_only_once() {
    let env = Env::new();
    let d = start(&env);
    failure(&d);
    env.system.exit(100);
    let barrier = Arc::new(std::sync::Barrier::new(9));
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let (d, barrier) = (d.clone(), barrier.clone());
            scope.spawn(move || {
                barrier.wait();
                tick(&d);
            });
        }
        barrier.wait();
    });
    assert_eq!(env.system.resume_calls.lock().unwrap().len(), 1);
    d.registry().stop_all();
}
#[test]
fn credential_dependent_or_missing_window_context_needs_user() {
    let env = Env::new();
    let d = start(&env);
    ok(
        &d,
        "agents.hook",
        json!({"agent":"claude-code","event":{
            "kind":"turn_end","session_id":"session-private","cwd":r"D:\work",
            "outcome":{"kind":"api_error","failure":{"network":true}},
            "resume_context_complete":false,"resume_environment":{}
        }}),
    );
    wait_events(&d);
    env.system.exit(100);
    assert_eq!(tick(&d)["entries"][0]["state"], "needs_user");
    assert!(env.system.resume_calls.lock().unwrap().is_empty());
    d.registry().stop_all();
}

#[test]
fn open_window_forks_and_all_branch_retries_share_the_original_budget() {
    let env = Env::new();
    let d = start(&env);
    failure(&d);
    env.system.advance_ms(2100);
    tick(&d);
    let first = env.system.resume_calls.lock().unwrap()[0].clone();
    let branch = first.fork_session_id.clone().expect("open window must fork");
    assert_eq!(first.session_id, "session-123");
    // Prompt in the original window after launch must not kill the fork.
    hook(&d, "claude-code", "prompt_submit", Value::Null);
    assert_eq!(tick(&d)["entries"][0]["state"], "running");
    for i in 0..3 {
        let pid = 900_000 + i.min(2);
        env.system.spawn(4242, pid, "away-guard-hook.exe");
        ok(
            &d,
            "agents.hook",
            json!({"agent":"claude-code","event":{"kind":"turn_end","session_id":branch,"cwd":r"D:\work",
            "outcome":{"kind":"api_error","failure":{"network":true}},
            "resume_context_complete":true,"resume_environment":{}}}),
        );
        wait_events(&d);
        env.system.exit(pid);
        env.system.advance_ms(2500);
        tick(&d);
    }
    let calls = env.system.resume_calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert!(
        env.system
            .notifications
            .lock()
            .unwrap()
            .iter()
            .any(|(_, m)| m.contains("3 次"))
    );
    assert!(
        env.system
            .notifications
            .lock()
            .unwrap()
            .iter()
            .any(|(_, m)| m.contains(&branch) && m.contains("接手"))
    );
    assert_eq!(calls[1].session_id, branch);
    assert_eq!(calls[2].session_id, branch);
    assert!(calls[1..].iter().all(|c| c.fork_session_id.is_none()));
    assert_eq!(tick(&d)["entries"][0]["state"], "exhausted");
    d.registry().stop_all();
}
