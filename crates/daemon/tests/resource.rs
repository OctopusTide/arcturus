//! Resource arbitration against a fake machine; never changes user applications.
mod common;
use common::{Env, agent_call, call, module_status, trusted, wait_events};
use daemon::arbitration::ProgramRule;
use proto::ErrorCode;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

fn rule(path: &str, source: &str) -> ProgramRule {
    ProgramRule {
        path: path.into(),
        signer: None,
        source: source.into(),
    }
}
fn start(env: &Env) -> std::sync::Arc<daemon::dispatch::Dispatcher> {
    let d = env.start(vec![
        daemon::modules::mode::create(),
        daemon::modules::permission::create(),
        daemon::modules::task::create(),
        daemon::modules::resource::create(),
    ]);
    wait_events(&d);
    d
}
fn ok(d: &daemon::dispatch::Dispatcher, cmd: &str, args: Value) -> Value {
    agent_call(d, cmd, args).into_result().unwrap()
}
fn ui(d: &daemon::dispatch::Dispatcher, cmd: &str, args: Value) -> Value {
    call(d, &mut trusted(d), cmd, args).into_result().unwrap()
}
fn away(d: &daemon::dispatch::Dispatcher) {
    ok(d, "away", json!({}));
    wait_events(d);
}
fn pressure(env: &Env) {
    env.system.memory.lock().unwrap().available_mb = 500;
}
fn tick(d: &daemon::dispatch::Dispatcher) {
    ok(d, "resource.check", json!({}));
    wait_events(d);
}
fn state(d: &daemon::dispatch::Dispatcher) -> Value {
    module_status(&ok(d, "status", json!({})), "resource")["state"].clone()
}
fn configure(d: &daemon::dispatch::Dispatcher, v: Value) {
    ui(d, "resource.configure", json!({"settings":v}));
}

#[test]
fn normal_mode_only_notifies_and_reports_actual_zero() {
    let env = Env::new();
    env.system.spawn(100, 0, "browser.exe").set_memory_mb(100, 5000);
    pressure(&env);
    let d = start(&env);
    tick(&d);
    let r = ok(&d, "free", json!({"mb":8000}));
    assert_eq!(r["state"], "needs_user");
    assert_eq!(r["freed_mb"], 0);
    assert!(env.system.actions().is_empty());
    assert_eq!(state(&d)["notice"], "memory_needs_user");
    d.registry().stop_all();
}

#[test]
fn freezes_whole_tree_waits_then_kills_and_cleans_journal() {
    let env = Env::new();
    env.system
        .spawn(100, 0, "browser.exe")
        .spawn(101, 100, "renderer.exe");
    env.system.set_memory_mb(100, 2000);
    env.system.set_memory_mb(101, 2000);
    pressure(&env);
    let d = start(&env);
    away(&d);
    tick(&d);
    assert_eq!(
        *env.system.suspended.lock().unwrap(),
        [100, 101].into_iter().collect()
    );
    assert!(d.store().journal_open(Some("resource")).unwrap().len() >= 2);
    env.system.advance_ms(29_000);
    tick(&d);
    assert!(env.system.alive(100));
    env.system.advance_ms(1_000);
    tick(&d);
    assert!(!env.system.alive(100) && !env.system.alive(101));
    assert!(d.store().journal_open(Some("resource")).unwrap().is_empty());
    d.registry().stop_all();
}

#[test]
fn a_successful_free_reports_measured_mb_and_does_not_kill_later() {
    let env = Env::new();
    env.system.spawn(100, 0, "browser.exe").set_memory_mb(100, 4000);
    env.system.freeze_reclaims_mb.store(2500, Ordering::SeqCst);
    let d = start(&env);
    away(&d);
    let r = ok(&d, "free", json!({"mb":2000}));
    assert_eq!(r["state"], "completed");
    assert_eq!(r["freed_mb"], 2500);
    assert_eq!(
        ok(&d, "resource.result", json!({"request_id":r["request_id"]}))["freed_mb"],
        2500
    );
    env.system.advance_ms(31_000);
    tick(&d);
    assert!(env.system.alive(100));
    ok(&d, "back", json!({}));
    wait_events(&d);
    assert!(env.system.suspended.lock().unwrap().is_empty());
    d.registry().stop_all();
}

#[test]
fn pending_free_completes_after_wait_and_cannot_run_after_back() {
    let env = Env::new();
    env.system.spawn(100, 0, "browser.exe").set_memory_mb(100, 4000);
    let d = start(&env);
    away(&d);
    let r = ok(&d, "free", json!({"mb":2000}));
    assert_eq!(r["state"], "pending");
    env.system.advance_ms(31_000);
    tick(&d);
    let r = ok(&d, "resource.result", json!({"request_id":r["request_id"]}));
    assert_eq!(r["state"], "completed");
    assert_eq!(r["freed_mb"], 4000);
    env.system.spawn(102, 0, "other.exe").set_memory_mb(102, 2000);
    let r = ok(&d, "free", json!({"mb":1000}));
    assert_eq!(r["state"], "pending");
    ok(&d, "back", json!({}));
    wait_events(&d);
    env.system.advance_ms(31_000);
    tick(&d);
    assert!(env.system.alive(102));
    assert_eq!(
        ok(&d, "resource.result", json!({"request_id":r["request_id"]}))["state"],
        "needs_user"
    );
    d.registry().stop_all();
}

#[test]
fn whitelist_tasks_dependencies_and_unknown_users_are_never_touched() {
    let env = Env::new();
    env.system
        .rules
        .lock()
        .unwrap()
        .push(rule(r"C:\fake\system.exe", "builtin"));
    for (pid, name) in [
        (100, "system.exe"),
        (200, "editor.exe"),
        (300, "agent.exe"),
        (400, "postgres.exe"),
        (500, "foreign.exe"),
        (600, "unknown.exe"),
    ] {
        env.system.spawn(pid, 0, name).set_memory_mb(pid, 2000);
    }
    env.system.set_other_user(500);
    env.system.set_exe(600, None);
    let d = start(&env);
    ui(
        &d,
        "permission.set",
        json!({"program":r"C:\fake\editor.exe","class":"never"}),
    );
    ok(&d, "protect", json!({"pid":300,"deps":["postgres.exe"]}));
    // New child/dependency appears before task's next periodic snapshot.
    env.system.spawn(301, 300, "worker.exe").set_memory_mb(301, 1000);
    env.system.spawn(401, 0, "postgres.exe").set_memory_mb(401, 2000);
    pressure(&env);
    away(&d);
    tick(&d);
    env.system.advance_ms(31_000);
    tick(&d);
    assert!(env.system.suspended.lock().unwrap().is_empty());
    assert!(
        !env.system
            .actions()
            .iter()
            .any(|a| a.starts_with("freeze(") || a.starts_with("terminate("))
    );
    d.registry().stop_all();
}

#[test]
fn partial_tree_with_exempt_descendant_is_skipped() {
    let env = Env::new();
    env.system
        .spawn(100, 0, "browser.exe")
        .spawn(101, 100, "important.exe");
    env.system.set_memory_mb(100, 5000);
    let d = start(&env);
    ui(
        &d,
        "permission.set",
        json!({"program":r"C:\fake\important.exe","class":"never"}),
    );
    pressure(&env);
    away(&d);
    tick(&d);
    assert!(env.system.suspended.lock().unwrap().is_empty());
    d.registry().stop_all();
}

#[test]
fn newly_protected_frozen_tree_is_thawed_before_termination() {
    let env = Env::new();
    env.system.spawn(100, 0, "browser.exe").set_memory_mb(100, 4000);
    pressure(&env);
    let d = start(&env);
    away(&d);
    assert!(env.system.suspended.lock().unwrap().contains(&100));
    ok(&d, "protect", json!({"pid":100}));
    wait_events(&d);
    assert!(env.system.suspended.lock().unwrap().is_empty());
    env.system.advance_ms(31_000);
    tick(&d);
    assert!(env.system.alive(100));
    d.registry().stop_all();
}

#[test]
fn own_task_limit_stops_reclaiming_including_later_children() {
    let env = Env::new();
    env.system.spawn(100, 0, "agent.exe").set_memory_mb(100, 1);
    env.system.spawn(200, 0, "browser.exe").set_memory_mb(200, 6000);
    let d = start(&env);
    ok(&d, "protect", json!({"pid":100,"mem_limit_mb":100}));
    env.system.spawn(101, 100, "leak.exe").set_memory_mb(101, 200);
    pressure(&env);
    away(&d);
    assert_eq!(ok(&d, "free", json!({"mb":2000}))["state"], "task_limit");
    assert!(env.system.suspended.lock().unwrap().is_empty());
    assert_eq!(state(&d)["notice"], "protected_task_memory_limit");
    d.registry().stop_all();
}

#[test]
fn custom_preset_may_disable_reclaim_and_changing_it_thaws() {
    let env = Env::new();
    env.system.spawn(100, 0, "browser.exe").set_memory_mb(100, 4000);
    pressure(&env);
    let d = start(&env);
    ui(
        &d,
        "mode.configure",
        json!({"settings":{"presets":{"custom":{"free_memory":false}}}}),
    );
    ok(&d, "away", json!({"preset":"custom"}));
    wait_events(&d);
    tick(&d);
    assert!(env.system.suspended.lock().unwrap().is_empty());
    ui(
        &d,
        "mode.configure",
        json!({"settings":{"presets":{"custom":{"free_memory":true}}}}),
    );
    wait_events(&d);
    assert!(env.system.suspended.lock().unwrap().contains(&100));
    ui(
        &d,
        "mode.configure",
        json!({"settings":{"presets":{"custom":{"free_memory":false}}}}),
    );
    wait_events(&d);
    assert!(env.system.suspended.lock().unwrap().is_empty());
    d.registry().stop_all();
}

#[test]
fn wechat_is_last_and_an_elevated_candidate_does_not_block_others() {
    let env = Env::new();
    for (pid, name, mb) in [
        (100, "wechat.exe", 7000),
        (200, "admin.exe", 6000),
        (300, "browser.exe", 4000),
    ] {
        env.system.spawn(pid, 0, name).set_memory_mb(pid, mb);
    }
    env.system.set_elevated(200);
    pressure(&env);
    let d = start(&env);
    away(&d);
    assert!(env.system.suspended.lock().unwrap().contains(&300));
    assert!(!env.system.suspended.lock().unwrap().contains(&100));
    d.registry().stop_all();
}

#[test]
fn vram_limit_is_optional_away_only_and_respects_protection() {
    let env = Env::new();
    env.system.spawn(100, 0, "gpu.exe").spawn(200, 0, "agent.exe");
    {
        let mut v = env.system.video.lock().unwrap();
        v.used_mb = 4000;
        v.per_pid_mb.insert(100, 1500);
        v.per_pid_mb.insert(200, 2500);
    }
    let d = start(&env);
    configure(&d, json!({"video_limit_mb":3000}));
    tick(&d);
    assert!(env.system.alive(100));
    ok(&d, "protect", json!({"pid":200}));
    away(&d);
    tick(&d);
    assert!(!env.system.alive(100));
    assert!(env.system.alive(200));
    d.registry().stop_all();
}

#[test]
fn daemon_crash_and_disabled_module_recover_frozen_processes_but_not_reused_pids() {
    let env = Env::new();
    env.system.spawn(100, 0, "browser.exe").set_memory_mb(100, 4000);
    pressure(&env);
    let d = start(&env);
    away(&d);
    assert!(env.system.suspended.lock().unwrap().contains(&100));
    drop(d);
    env.write_config(json!({"modules":{"resource":{"enabled":false}}}));
    let d = start(&env);
    assert!(env.system.suspended.lock().unwrap().is_empty());
    assert!(d.store().journal_open(Some("resource")).unwrap().is_empty());
    d.registry().stop_all();
    drop(d);
    env.write_config(json!({}));
    env.system.memory.lock().unwrap().available_mb = 500;
    let d = start(&env);
    away(&d);
    assert!(env.system.suspended.lock().unwrap().contains(&100));
    drop(d);
    env.system.exit(100);
    env.system.advance_ms(1000);
    env.system.spawn(100, 0, "replacement.exe");
    env.system.suspended.lock().unwrap().remove(&100);
    let n = env.system.actions().len();
    env.write_config(json!({"modules":{"resource":{"enabled":false}}}));
    let d = start(&env);
    assert!(!env.system.actions()[n..].iter().any(|a| a == "thaw(100)"));
    d.registry().stop_all();
}

#[test]
fn settings_and_arguments_are_validated_and_agents_cannot_change_policy() {
    let env = Env::new();
    let d = start(&env);
    assert_eq!(
        agent_call(
            &d,
            "resource.configure",
            json!({"settings":{"available_percent":50}})
        )
        .error
        .unwrap()
        .code,
        ErrorCode::Denied
    );
    for settings in [
        json!({"available_percent":0}),
        json!({"freeze_wait_seconds":0}),
        json!({"typo":4}),
        json!({"video_limit_mb":0}),
    ] {
        assert_eq!(
            call(
                &d,
                &mut trusted(&d),
                "resource.configure",
                json!({"settings":settings})
            )
            .error
            .unwrap()
            .code,
            ErrorCode::InvalidArgs
        );
    }
    assert_eq!(
        agent_call(&d, "free", json!({"mb":u64::MAX})).error.unwrap().code,
        ErrorCode::InvalidArgs
    );
    d.registry().stop_all();
}

#[test]
fn screensaver_discovery_does_not_disable_resource_actions() {
    let env = Env::new();
    env.system
        .rules
        .lock()
        .unwrap()
        .push(rule(r"C:\Windows\System32\Mystify.scr", "builtin"));
    env.system.spawn(100, 0, "browser.exe").set_memory_mb(100, 4000);
    pressure(&env);
    let d = start(&env);
    away(&d);
    assert!(env.system.suspended.lock().unwrap().contains(&100));
    d.registry().stop_all();
}
#[test]
fn unreadable_image_keeps_recovery_receipt_until_it_can_be_verified() {
    let env = Env::new();
    env.system.spawn(100, 0, "browser.exe").set_memory_mb(100, 4000);
    pressure(&env);
    let d = start(&env);
    away(&d);
    env.system.set_exe(100, None);
    ok(&d, "back", json!({}));
    wait_events(&d);
    assert!(env.system.suspended.lock().unwrap().contains(&100));
    assert!(!d.store().journal_open(Some("resource")).unwrap().is_empty());
    env.system.set_exe(100, Some(r"C:\fake\browser.exe"));
    tick(&d);
    assert!(!env.system.suspended.lock().unwrap().contains(&100));
    assert!(d.store().journal_open(Some("resource")).unwrap().is_empty());
    d.registry().stop_all();
}
#[test]
fn thermal_freeze_is_limited_to_low_priority_and_released_after_cooling() {
    let env = Env::new();
    env.system.spawn(100, 0, "normal.exe").set_memory_mb(100, 2000);
    env.system.spawn(200, 0, "low.exe").set_memory_mb(200, 2000);
    env.system
        .set_priority(200, Some(daemon::sys::ProcessPriority::BelowNormal));
    let d = start(&env);
    away(&d);
    for level in [2, 0] {
        d.registry().bus().publish(daemon::Event {
            topic: "power.overheat".into(),
            source: "power".into(),
            ts: daemon::store::now_ms(),
            data: json!({"level":level}),
        });
        wait_events(&d);
        assert!(!env.system.suspended.lock().unwrap().contains(&100));
        assert_eq!(env.system.suspended.lock().unwrap().contains(&200), level == 2);
    }
    d.registry().stop_all();
}
#[test]
fn unsatisfied_free_request_expires_instead_of_killing_indefinitely() {
    let env = Env::new();
    env.system.spawn(100, 0, "browser.exe").set_memory_mb(100, 4000);
    let d = start(&env);
    configure(&d, json!({"freeze_wait_seconds":3600}));
    away(&d);
    let r = ok(&d, "free", json!({"mb":1000}));
    assert_eq!(r["state"], "pending");
    env.system.advance_ms(5 * 60_000 + 1);
    tick(&d);
    assert_eq!(
        ok(&d, "resource.result", json!({"request_id":r["request_id"]}))["state"],
        "expired"
    );
    assert!(env.system.alive(100));
    d.registry().stop_all();
}

#[test]
fn os_wrappers_do_not_exempt_chrome_but_real_agent_hosts_still_do() {
    let env = Env::new();
    env.system
        .spawn(100, 0, "chrome.exe")
        .spawn(101, 100, "cmd.exe")
        .spawn(102, 101, "extension-host.exe")
        .spawn(200, 0, "chatgpt.exe")
        .spawn(201, 200, "codex.exe")
        .spawn(300, 0, "python.exe")
        .spawn(301, 300, "conhost.exe");
    env.system.set_memory_mb(100, 4500);
    env.system.set_memory_mb(200, 2500);
    env.system.set_memory_mb(300, 200);
    env.system.rules.lock().unwrap().extend([
        rule(r"C:\fake\cmd.exe", "default"),
        rule(r"C:\fake\conhost.exe", "builtin"),
        rule(r"C:\fake\codex.exe", "default"),
    ]);
    let d = start(&env);
    tick(&d);
    let candidates = state(&d)["reading"]["candidates"].as_array().unwrap().clone();
    assert!(
        candidates
            .iter()
            .any(|c| c["root_pid"] == 100 && c["pids"] == json!([100, 102])),
        "{candidates:?}"
    );
    assert!(candidates.iter().any(|c| c["root_pid"] == 300), "{candidates:?}");
    assert!(
        !candidates
            .iter()
            .any(|c| c["root_pid"] == 200 || c["root_pid"] == 201),
        "{candidates:?}"
    );
    d.registry().stop_all();
}
