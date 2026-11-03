//! All networking, notification and process actions here are in-memory fakes.
mod common;
use common::{Env, agent_call, call, err_code, module_status, trusted, wait_events};
use daemon::dispatch::Dispatcher;
use daemon::modules::exit_guard::{
    self,
    actions::{Actions, PendingActions},
    config::Action,
    probe::SERVICES,
};
use daemon::sys::{
    System,
    fake::FakeSystem,
    network::{Network, NetworkChange, ProxyRoute, fake::FakeNetwork},
};
use proto::{ApiError, ErrorCode};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct TestSystem {
    network: Arc<FakeNetwork>,
    base: Arc<FakeSystem>,
    notices: Mutex<Vec<String>>,
    refuse_actions: AtomicBool,
}
impl System for TestSystem {
    fn process_image_path(&self, _: u32) -> Option<String> {
        None
    }
    fn uptime_ms(&self) -> Result<u64, ApiError> {
        self.base.uptime_ms()
    }
    fn now_ms(&self) -> i64 {
        self.base.now_ms()
    }
    fn network(&self) -> &dyn Network {
        self.network.as_ref()
    }
    fn notify_user(&self, title: &str, message: &str) -> Result<(), ApiError> {
        self.notices
            .lock()
            .unwrap()
            .push(format!("{title}: {message}"));
        Ok(())
    }
    fn processes(&self) -> Result<Vec<daemon::sys::ProcessEntry>, ApiError> {
        self.base.processes()
    }
    fn process_info(&self, pid: u32) -> Result<daemon::sys::ProcessInfo, ApiError> {
        self.base.process_info(pid)
    }
    fn program_rules(&self) -> Result<Vec<daemon::arbitration::ProgramRule>, ApiError> {
        self.base.program_rules()
    }
    fn process_usage(&self, pid: u32) -> Result<daemon::sys::Usage, ApiError> {
        self.base.process_usage(pid)
    }
    fn track_tree(
        &self,
        root: u32,
        members: &[u32],
    ) -> Result<Box<dyn daemon::sys::TreeTracker>, ApiError> {
        self.base.track_tree(root, members)
    }
    fn suspension_state(
        &self,
        pid: u32,
        at: i64,
    ) -> Result<Vec<daemon::sys::ThreadState>, ApiError> {
        if self.refuse_actions.load(Ordering::SeqCst) {
            return Err(daemon::sys::access_denied("suspension_state"));
        }
        self.base.suspension_state(pid, at)
    }
    fn freeze_process(
        &self,
        pid: u32,
        at: i64,
        t: &[daemon::sys::ThreadState],
    ) -> Result<(), ApiError> {
        self.base.freeze_process(pid, at, t)
    }
    fn thaw_process(
        &self,
        pid: u32,
        at: i64,
        t: &[daemon::sys::ThreadState],
    ) -> Result<(), ApiError> {
        if self.refuse_actions.load(Ordering::SeqCst) {
            return Err(daemon::sys::access_denied("thaw_process"));
        }
        self.base.thaw_process(pid, at, t)
    }
    fn terminate_process(&self, pid: u32, at: i64) -> Result<(), ApiError> {
        self.base.terminate_process(pid, at)
    }
}
#[derive(Default)]
struct RecordingActions(Mutex<Vec<(String, Action)>>);
impl Actions for RecordingActions {
    fn apply(
        &self,
        ctx: &daemon::ModuleCtx,
        path: &str,
        action: Action,
    ) -> Result<Value, ApiError> {
        let intent=ctx.db().query("SELECT data FROM exit_guard_incidents WHERE kind = 'violation' ORDER BY id DESC LIMIT 1",&[])?;
        assert!(
            !intent.is_empty(),
            "violation must be durable before acting"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&intent[0].text(0).unwrap()).unwrap()["action_result"]["state"],
            "pending"
        );
        self.0.lock().unwrap().push((path.into(), action));
        PendingActions.apply(ctx, path, action)
    }
}
struct Test {
    env: Env,
    net: Arc<FakeNetwork>,
    system: Arc<TestSystem>,
    actions: Arc<RecordingActions>,
}
impl Test {
    fn new() -> Self {
        let mut env = Env::new();
        let net = Arc::new(FakeNetwork::default());
        *net.dns.lock().unwrap() = daemon::sys::network::DnsAssessment {
            state: "vpn_route".into(),
            details: vec![],
        };
        let system = Arc::new(TestSystem {
            network: net.clone(),
            base: env.system.clone(),
            notices: Mutex::default(),
            refuse_actions: AtomicBool::new(false),
        });
        env.config.sys.system = system.clone();
        Self {
            env,
            net,
            system,
            actions: Arc::default(),
        }
    }
    fn boot(&self) -> Arc<Dispatcher> {
        let d = self.env.start(vec![
            daemon::modules::mode::create(),
            exit_guard::create_with_actions(self.actions.clone()),
        ]);
        wait_events(&d);
        d
    }
    fn configure(&self, d: &Dispatcher, settings: Value) {
        let r = call(
            d,
            &mut trusted(d),
            "exit_guard.configure",
            json!({"settings": settings}),
        );
        assert!(r.ok, "{r:?}");
    }
    fn good(&self, country: &str) {
        self.net.reply(
            SERVICES[0],
            &format!("ip=203.0.113.8\nloc={country}\ncolo=SIN\n"),
        );
    }
    fn boot_sys_actions(&self) -> Arc<Dispatcher> {
        let d = self.env.start(vec![
            daemon::modules::mode::create(),
            daemon::modules::permission::create(),
            daemon::modules::task::create(),
            exit_guard::create(),
        ]);
        wait_events(&d);
        d
    }
}
fn state(d: &Dispatcher) -> Value {
    module_status(
        &agent_call(d, "status", json!({})).into_result().unwrap(),
        "exit_guard",
    )["state"]
        .clone()
}
fn settle(d: &Dispatcher, at_least: u64) -> Value {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let s = state(d);
        if s["checks"].as_u64().unwrap() >= at_least {
            return s;
        }
        assert!(Instant::now() < until, "check did not finish: {s}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn basic() -> Value {
    json!({"programs":[{"path":"D:\\apps\\one.exe"}],"region":{"country":"SG"},"proxy":{"kind":"direct"}})
}

#[test]
fn empty_default_no_queries_and_agent_cannot_change_or_approve() {
    let t = Test::new();
    let d = t.boot();
    let settings = agent_call(&d, "exit_guard.settings", json!({}))
        .into_result()
        .unwrap();
    assert_eq!(settings["programs"], json!([]));
    assert!(settings["interval_seconds"].is_null());
    assert!(t.net.requests.lock().unwrap().is_empty());
    assert!(["pending", "idle"].contains(&state(&d)["firewall"]["state"].as_str().unwrap()));
    for (cmd, args) in [
        ("exit_guard.configure", json!({"settings":basic()})),
        ("exit_guard.review", json!({"request_id":1,"approve":true})),
        ("exit_guard.requests", json!({})),
    ] {
        assert_eq!(err_code(&agent_call(&d, cmd, args)).0, ErrorCode::Denied);
    }
    d.shutdown();
    assert_eq!(t.net.subscriptions(), 0);
}

#[test]
fn proposals_are_durable_and_only_approved_by_page() {
    let t = Test::new();
    t.good("SG");
    let d = t.boot();
    let id = agent_call(&d, "exit_guard.request", json!({"settings":basic()}))
        .into_result()
        .unwrap()["request_id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        agent_call(&d, "exit_guard.settings", json!({}))
            .into_result()
            .unwrap()["programs"],
        json!([])
    );
    d.shutdown();
    drop(d);
    let d = t.boot();
    let pending = call(&d, &mut trusted(&d), "exit_guard.requests", json!({}))
        .into_result()
        .unwrap();
    assert_eq!(pending["requests"][0]["id"], id);
    assert!(
        call(
            &d,
            &mut trusted(&d),
            "exit_guard.review",
            json!({"request_id":id,"approve":true})
        )
        .ok
    );
    assert_eq!(settle(&d, 1)["last_check"]["programs"][0]["matches"], true);
    d.shutdown();
    drop(d);
    let d = t.boot();
    settle(&d, 1);
    assert_eq!(
        agent_call(&d, "exit_guard.settings", json!({}))
            .into_result()
            .unwrap()["programs"][0]["action"],
        "block"
    );
    d.shutdown();
}

#[test]
fn invalid_paths_duplicate_paths_unknown_settings_and_external_control_are_rejected() {
    let t = Test::new();
    let d = t.boot();
    for patch in [
        json!({"programs":[{"path":"one.exe"}]}),
        json!({"programs":[{"path":"D:\\x\\..\\one.exe"}]}),
        json!({"programs":[{"path":"D:\\X\\a.exe"},{"path":"d:/x/a.exe"}]}),
        json!({"bad_key":true}),
        json!({"control":{"url":"http://example.com/"}}),
        json!({"control":{"url":"http://127.0.0.1.evil/"}}),
        json!({"interval_seconds":0}),
        json!({"region":{"country":"Singapore"}}),
    ] {
        assert_eq!(
            err_code(&call(
                &d,
                &mut trusted(&d),
                "exit_guard.configure",
                json!({"settings":patch})
            ))
            .0,
            ErrorCode::InvalidArgs
        );
    }
    assert!(t.net.requests.lock().unwrap().is_empty());
    d.shutdown();
}

#[test]
fn fallback_order_validates_country_and_never_uses_colo() {
    let t = Test::new();
    t.net
        .reply(SERVICES[0], "ip=203.0.113.8\ncolo=SIN\nloc=invalid\n");
    t.net.reply(
        SERVICES[1],
        r#"{"error":true,"ip":"203.0.113.8","country_code":"SG"}"#,
    );
    t.net.reply(
        SERVICES[2],
        r#"{"success":true,"ip":"203.0.113.8","country_code":"SG"}"#,
    );
    let d = t.boot();
    t.configure(&d, basic());
    let s = settle(&d, 1);
    assert_eq!(s["last_check"]["programs"][0]["matches"], true);
    assert_eq!(
        t.net
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.0.as_str())
            .collect::<Vec<_>>(),
        SERVICES
    );
    assert!(t.actions.0.lock().unwrap().is_empty());
    d.shutdown();
}

#[test]
fn unavailable_blocks_each_full_path_without_freezing_or_killing() {
    let t = Test::new();
    let d = t.boot();
    t.configure(&d,json!({"region":{"country":"SG"},"programs":[{"path":"D:\\a\\same.exe"},{"path":"D:\\b\\same.exe","action":"freeze"},{"path":"D:\\c\\other.exe","action":"kill"}]}));
    let s = settle(&d, 1);
    let programs = &s["last_check"]["programs"];
    assert_eq!(programs[0]["action_result"]["state"], "needs_admin");
    assert_eq!(programs[1]["action_result"]["state"], "needs_admin");
    assert_eq!(programs[2]["action_result"]["state"], "needs_admin");
    assert_eq!(t.actions.0.lock().unwrap().len(), 3);
    assert!(
        t.actions
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|(_, action)| *action == Action::Block)
    );
    assert_eq!(t.net.requests.lock().unwrap().len(), 3); // same route queried only once
    assert!(!t.system.notices.lock().unwrap().is_empty());
    assert_eq!(
        agent_call(&d, "exit_guard.incidents", json!({}))
            .into_result()
            .unwrap()["incidents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v.get("path").is_some())
            .count(),
        3
    );
    d.shutdown();
}

#[test]
fn application_overrides_share_route_but_get_individual_targets() {
    let t = Test::new();
    t.good("SG");
    let d = t.boot();
    t.configure(&d,json!({"region":{"country":"SG"},"proxy":{"kind":"explicit","url":"http://127.0.0.1:7890"},"programs":[{"path":"D:\\a.exe"},{"path":"D:\\b.exe","region":{"country":"US"}},{"path":"D:\\c.exe","region":{"ip":"203.0.113.8"},"proxy":{"kind":"direct"}}]}));
    let s = settle(&d, 1);
    let p = &s["last_check"]["programs"];
    assert_eq!(p[0]["matches"], true);
    assert_eq!(p[1]["matches"], false);
    assert_eq!(p[2]["matches"], true);
    let requests = t.net.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .any(|r| r.1 == ProxyRoute::Explicit("http://127.0.0.1:7890".into()))
    );
    assert!(requests.iter().any(|r| r.1 == ProxyRoute::Direct));
    drop(requests);
    d.shutdown();
}

#[test]
fn every_network_change_triggers_once_and_unchanged_time_does_not_query_public_services() {
    let t = Test::new();
    t.good("SG");
    let d = t.boot();
    t.configure(&d, basic());
    settle(&d, 1);
    for (i, c) in [
        NetworkChange::Interface,
        NetworkChange::Address,
        NetworkChange::Route,
        NetworkChange::Proxy,
    ]
    .into_iter()
    .enumerate()
    {
        t.net.emit(c);
        settle(&d, i as u64 + 2);
    }
    let before = t.net.requests.lock().unwrap().len();
    std::thread::sleep(Duration::from_millis(650));
    assert_eq!(t.net.requests.lock().unwrap().len(), before);
    d.shutdown();
}

#[test]
fn local_node_changes_trigger_public_check_and_rule_dns_warnings_are_persistent() {
    let t = Test::new();
    t.good("SG");
    let url = "http://127.0.0.1:9090/status";
    t.net.reply(url, r#"{"now":"node-a","mode":"rule"}"#);
    t.net.dns.lock().unwrap().state = "bypass".into();
    let d = t.boot();
    let mut settings = basic();
    settings["region"]["node"] = json!("node-a");
    settings["control"] = json!({"url":url,"poll_seconds":1});
    t.configure(&d, settings);
    let s = settle(&d, 1);
    assert_eq!(s["last_check"]["programs"][0]["matches"], true);
    let notices = s["notices"].to_string();
    assert!(notices.contains("DNS"));
    assert!(notices.contains("rule mode"));
    std::thread::sleep(Duration::from_millis(1250));
    assert_eq!(state(&d)["checks"], 1);
    t.net.reply(url, r#"{"now":"node-b","mode":"global"}"#);
    let s = settle(&d, 2);
    assert_eq!(s["last_check"]["programs"][0]["matches"], false);
    assert!(
        t.net
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.0 == url)
            .all(|r| r.1 == ProxyRoute::Direct)
    );
    d.shutdown();
}

#[test]
fn incidents_link_to_away_reports_and_survive_restart() {
    let t = Test::new();
    let d = t.boot();
    assert!(agent_call(&d, "away", json!({})).ok);
    wait_events(&d);
    t.configure(&d, basic());
    settle(&d, 1);
    assert!(agent_call(&d, "back", json!({})).ok);
    wait_events(&d);
    let reports = agent_call(&d, "exit_guard.reports", json!({}))
        .into_result()
        .unwrap();
    assert!(
        reports["reports"][0]["incidents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["path"] == "D:\\apps\\one.exe")
    );
    d.shutdown();
    drop(d);
    let d = t.boot();
    assert!(
        !agent_call(&d, "exit_guard.reports", json!({}))
            .into_result()
            .unwrap()["reports"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    d.shutdown();
}

#[test]
fn periodic_checks_require_opt_in_and_can_be_disabled_again() {
    let t = Test::new();
    t.good("SG");
    let d = t.boot();
    t.configure(&d, basic());
    settle(&d, 1);
    t.env.system.advance_ms(100_000);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(state(&d)["checks"], 1);
    t.configure(&d, json!({"interval_seconds":10}));
    settle(&d, 2);
    t.env.system.advance_ms(11_000);
    assert_eq!(settle(&d, 3)["last_check"]["reason"], "periodic");
    t.configure(&d, json!({"interval_seconds":null}));
    settle(&d, 4);
    t.env.system.advance_ms(100_000);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(state(&d)["checks"], 4);
    assert_eq!(state(&d)["periodic_enabled"], false);
    d.shutdown();
}

#[test]
fn network_module_switch_event_and_recovery_are_observed() {
    let t = Test::new();
    let d = t.boot();
    t.configure(&d, basic());
    settle(&d, 1);
    t.good("SG");
    d.registry().bus().publish(daemon::Event {
        topic: "network.switched".into(),
        source: "network".into(),
        ts: t.system.now_ms(),
        data: json!({"interface":"fake-vpn"}),
    });
    let s = settle(&d, 2);
    assert_eq!(s["last_check"]["programs"][0]["matches"], true);
    assert_eq!(
        s["last_check"]["programs"][0]["action_result"]["state"],
        "not_needed"
    );
    assert_eq!(t.actions.0.lock().unwrap().len(), 1);
    d.shutdown();
}

#[test]
fn local_control_failure_invalidates_node_identity_without_public_polling_loop() {
    let t = Test::new();
    t.good("SG");
    let url = "http://[::1]:9090/status";
    t.net.reply(url, r#"{"now":"node-a","mode":"global"}"#);
    let d = t.boot();
    let mut settings = basic();
    settings["region"] = json!({"node":"node-a"});
    settings["control"] = json!({"url":url,"poll_seconds":1});
    t.configure(&d, settings);
    settle(&d, 1);
    t.net.replies.lock().unwrap().remove(url);
    t.env.system.advance_ms(1100);
    assert_eq!(settle(&d, 2)["last_check"]["programs"][0]["matches"], false);
    t.env.system.advance_ms(1100);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(state(&d)["checks"], 2);
    d.shutdown();
}

#[test]
fn shared_sys_freeze_is_journaled_resumed_on_recovery_and_never_suspends_twice() {
    let t = Test::new();
    t.good("US");
    t.env.system.spawn(201, 0, "one.exe");
    t.env.system.set_exe(201, Some("D:\\apps\\one.exe"));
    let d = t.boot_sys_actions();
    let mut s = basic();
    s["programs"][0]["action"] = json!("freeze");
    t.configure(&d, s);
    settle(&d, 1);
    assert!(t.env.system.suspended.lock().unwrap().contains(&201));
    assert_eq!(
        agent_call(&d, "status", json!({})).into_result().unwrap()["journal"]["open"],
        2
    );
    t.net.emit(NetworkChange::Address);
    settle(&d, 2);
    assert_eq!(
        t.env
            .system
            .actions
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.as_str() == "freeze(201)")
            .count(),
        1
    );
    t.good("SG");
    t.net.emit(NetworkChange::Route);
    settle(&d, 3);
    assert!(!t.env.system.suspended.lock().unwrap().contains(&201));
    assert_eq!(
        agent_call(&d, "status", json!({})).into_result().unwrap()["journal"]["open"],
        1
    );
    d.shutdown();
}

#[test]
fn shared_sys_kill_and_full_path_matching_leave_other_same_basename_untouched() {
    let t = Test::new();
    t.good("US");
    for pid in [201, 202] {
        t.env.system.spawn(pid, 0, "one.exe");
    }
    t.env.system.set_exe(201, Some("D:\\apps\\one.exe"));
    t.env.system.set_exe(202, Some("D:\\other\\one.exe"));
    let d = t.boot_sys_actions();
    let mut s = basic();
    s["programs"][0]["action"] = json!("kill");
    t.configure(&d, s);
    settle(&d, 1);
    assert!(t.env.system.process_info(201).is_err());
    assert!(t.env.system.process_info(202).is_ok());
    d.shutdown();
}

#[test]
fn permission_and_task_protection_block_exit_process_actions_in_every_mode() {
    let t = Test::new();
    t.good("US");
    t.env.system.spawn(201, 0, "one.exe");
    t.env.system.set_exe(201, Some("D:\\apps\\one.exe"));
    t.env
        .system
        .rules
        .lock()
        .unwrap()
        .push(daemon::arbitration::ProgramRule {
            path: "D:\\apps\\one.exe".into(),
            signer: None,
            source: "builtin".into(),
        });
    let d = t.boot_sys_actions();
    let mut s = basic();
    s["programs"][0]["action"] = json!("kill");
    t.configure(&d, s);
    settle(&d, 1);
    assert!(t.env.system.process_info(201).is_ok());
    assert_eq!(
        state(&d)["last_check"]["programs"][0]["action_result"]["state"],
        "denied"
    );
    d.shutdown();
    // A freshly protected task is checked synchronously, even before snapshot events settle.
    let t = Test::new();
    t.good("US");
    t.env.system.spawn(203, 0, "one.exe");
    t.env.system.set_exe(203, Some("D:\\apps\\one.exe"));
    let d = t.boot_sys_actions();
    assert!(agent_call(&d, "protect", json!({"pid":203})).ok);
    let mut s = basic();
    s["programs"][0]["action"] = json!("freeze");
    t.configure(&d, s);
    settle(&d, 1);
    assert!(!t.env.system.suspended.lock().unwrap().contains(&203));
    d.shutdown();
}

#[test]
fn removing_a_rule_and_clean_shutdown_restore_only_exit_owned_freezes() {
    let t = Test::new();
    t.good("US");
    t.env.system.spawn(201, 0, "one.exe");
    t.env.system.set_exe(201, Some("D:\\apps\\one.exe"));
    let d = t.boot_sys_actions();
    let mut s = basic();
    s["programs"][0]["action"] = json!("freeze");
    t.configure(&d, s.clone());
    settle(&d, 1);
    t.configure(&d, json!({"programs":[]}));
    let end = Instant::now() + Duration::from_secs(3);
    while t.env.system.suspended.lock().unwrap().contains(&201) {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
    t.configure(&d, s);
    settle(&d, 2);
    assert!(t.env.system.suspended.lock().unwrap().contains(&201));
    d.shutdown();
    assert!(!t.env.system.suspended.lock().unwrap().contains(&201));
}

#[test]
fn startup_recovery_resumes_same_process_but_never_touches_reused_pid() {
    for reused in [false, true] {
        let t = Test::new();
        t.env.system.spawn(201, 0, "one.exe");
        let p = t.env.system.process_info(201).unwrap();
        let threads = t.env.system.suspension_state(201, p.started_at).unwrap();
        let store =
            daemon::store::Store::open(&t.env.db_path(), t.env.config.sqlite.as_ref()).unwrap();
        store
            .journal_record(
                "exit_guard",
                &format!("frozen:201:{}", p.started_at),
                &json!({"process":p,"threads":threads}),
                Some(&json!(true)),
                None,
            )
            .unwrap();
        t.env
            .system
            .freeze_process(201, p.started_at, &threads)
            .unwrap();
        drop(store);
        if reused {
            t.env.system.exit(201);
            t.env.system.advance_ms(10);
            t.env.system.spawn(201, 0, "new.exe");
        }
        t.env.system.actions.lock().unwrap().clear();
        let d = t.boot_sys_actions();
        assert_eq!(
            agent_call(&d, "status", json!({})).into_result().unwrap()["journal"]["open"],
            0
        );
        assert_eq!(
            t.env
                .system
                .actions
                .lock()
                .unwrap()
                .iter()
                .any(|s| s == "thaw(201)"),
            !reused
        );
        d.shutdown();
    }
}

#[test]
fn new_task_protection_releases_exit_freeze_without_querying_egress_again() {
    let t = Test::new();
    t.good("US");
    t.env.system.spawn(201, 0, "one.exe");
    t.env.system.set_exe(201, Some("D:\\apps\\one.exe"));
    let d = t.boot_sys_actions();
    let mut s = basic();
    s["programs"][0]["action"] = json!("freeze");
    t.configure(&d, s);
    settle(&d, 1);
    assert!(t.env.system.suspended.lock().unwrap().contains(&201));
    assert!(agent_call(&d, "protect", json!({"pid":201})).ok);
    wait_events(&d);
    let end = Instant::now() + Duration::from_secs(3);
    while t.env.system.suspended.lock().unwrap().contains(&201) {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(state(&d)["checks"], 1);
    d.shutdown();
}

#[test]
fn proxy_kind_switch_and_explicit_disable_preserve_defaults_and_reject_typos() {
    let t = Test::new();
    let d = t.boot();
    t.configure(
        &d,
        json!({"proxy":{"kind":"explicit","url":"http://127.0.0.1:7890"}}),
    );
    t.configure(&d, json!({"proxy":{"kind":"direct"}}));
    assert_eq!(
        agent_call(&d, "exit_guard.settings", json!({}))
            .into_result()
            .unwrap()["proxy"],
        json!({"kind":"direct"})
    );
    assert_eq!(
        err_code(&call(
            &d,
            &mut trusted(&d),
            "exit_guard.configure",
            json!({"settings":{"proxy":{"kind":"system","typo":true}}})
        ))
        .0,
        ErrorCode::InvalidArgs
    );
    d.shutdown();
}

#[test]
fn returning_does_not_release_exit_freeze_until_egress_recovers() {
    let t = Test::new();
    t.good("US");
    t.env.system.spawn(201, 0, "one.exe");
    t.env.system.set_exe(201, Some("D:\\apps\\one.exe"));
    let d = t.boot_sys_actions();
    assert!(agent_call(&d, "away", json!({})).ok);
    wait_events(&d);
    let mut s = basic();
    s["programs"][0]["action"] = json!("freeze");
    t.configure(&d, s);
    settle(&d, 1);
    assert!(agent_call(&d, "back", json!({})).ok);
    wait_events(&d);
    assert!(t.env.system.suspended.lock().unwrap().contains(&201));
    t.good("SG");
    t.net.emit(NetworkChange::Route);
    settle(&d, 2);
    assert!(!t.env.system.suspended.lock().unwrap().contains(&201));
    d.shutdown();
}

struct FakeProcessAdmin(Arc<FakeSystem>, Mutex<Vec<(String, u32, i64)>>);
impl daemon::sys::Elevated for FakeProcessAdmin {
    fn backend(&self) -> &'static str {
        "fake"
    }
    fn set_interface_metric(&self, _: &str, _: u32) -> Result<(), ApiError> {
        Ok(())
    }
    fn terminate_process(&self, _: u32) -> Result<(), ApiError> {
        panic!("PID-only termination must never be used")
    }
    fn suspension_state(
        &self,
        pid: u32,
        at: i64,
    ) -> Result<Vec<daemon::sys::ThreadState>, ApiError> {
        self.1.lock().unwrap().push(("read".into(), pid, at));
        self.0.suspension_state(pid, at)
    }
    fn freeze_process(
        &self,
        pid: u32,
        at: i64,
        _: &[daemon::sys::ThreadState],
    ) -> Result<(), ApiError> {
        assert_eq!(self.0.process_info(pid)?.started_at, at);
        self.1.lock().unwrap().push(("freeze".into(), pid, at));
        self.0.suspended.lock().unwrap().insert(pid);
        Ok(())
    }
    fn thaw_process(
        &self,
        pid: u32,
        at: i64,
        t: &[daemon::sys::ThreadState],
    ) -> Result<(), ApiError> {
        self.1.lock().unwrap().push(("thaw".into(), pid, at));
        self.0.thaw_process(pid, at, t)
    }
    fn terminate_process_checked(&self, pid: u32, at: i64) -> Result<(), ApiError> {
        assert_eq!(self.0.process_info(pid)?.started_at, at);
        self.1.lock().unwrap().push(("kill".into(), pid, at));
        self.0.exit(pid);
        Ok(())
    }
}
#[test]
fn elevated_freeze_and_thaw_use_the_same_creation_checked_receipt() {
    for refuse_read in [false, true] {
        let mut t = Test::new();
        t.good("US");
        t.env.system.spawn(201, 0, "one.exe");
        t.env.system.set_exe(201, Some("D:\\apps\\one.exe"));
        t.env.system.set_elevated(201);
        t.system.refuse_actions.store(refuse_read, Ordering::SeqCst);
        let admin = Arc::new(FakeProcessAdmin(t.env.system.clone(), Mutex::default()));
        t.env.config.sys.elevated = admin.clone();
        let d = t.boot_sys_actions();
        let mut s = basic();
        s["programs"][0]["action"] = json!("freeze");
        t.configure(&d, s);
        settle(&d, 1);
        assert!(t.env.system.suspended.lock().unwrap().contains(&201));
        // Rights may change after suspension. Still let the service clear its own
        // durable receipt instead of reporting a local thaw and leaving stale ownership.
        t.system.refuse_actions.store(false, Ordering::SeqCst);
        t.good("SG");
        t.net.emit(NetworkChange::Route);
        settle(&d, 2);
        assert!(!t.env.system.suspended.lock().unwrap().contains(&201));
        let calls = admin.1.lock().unwrap();
        assert_eq!(calls.iter().any(|(op, _, _)| op == "read"), refuse_read);
        assert!(calls.iter().any(|(op, _, _)| op == "freeze"));
        assert!(calls.iter().any(|(op, _, _)| op == "thaw"));
        assert!(
            calls.iter().all(|(_, pid, at)| *pid == 201
                && *at == t.env.system.process_info(201).unwrap().started_at)
        );
        drop(calls);
        d.shutdown();
    }
}
#[test]
fn elevated_kill_never_uses_the_old_pid_only_interface() {
    let mut t = Test::new();
    t.good("US");
    t.env.system.spawn(201, 0, "one.exe");
    t.env.system.set_exe(201, Some("D:\\apps\\one.exe"));
    t.env.system.set_elevated(201);
    let admin = Arc::new(FakeProcessAdmin(t.env.system.clone(), Mutex::default()));
    t.env.config.sys.elevated = admin.clone();
    let d = t.boot_sys_actions();
    let mut s = basic();
    s["programs"][0]["action"] = json!("kill");
    t.configure(&d, s);
    settle(&d, 1);
    assert!(t.env.system.process_info(201).is_err());
    assert!(
        admin
            .1
            .lock()
            .unwrap()
            .iter()
            .any(|(op, _, _)| op == "kill")
    );
    d.shutdown();
}

#[test]
fn network_failure_does_not_execute_real_kill_or_freeze_actions() {
    for action in ["kill", "freeze"] {
        let t = Test::new();
        t.env.system.spawn(201, 0, "one.exe");
        t.env.system.set_exe(201, Some(r"D:\apps\one.exe"));
        let d = t.boot_sys_actions();
        let mut s = basic();
        s["programs"][0]["action"] = json!(action);
        t.configure(&d, s);
        let result = settle(&d, 1);
        assert_eq!(result["last_check"]["programs"][0]["action"], "block");
        assert!(t.env.system.alive(201));
        assert!(t.env.system.suspended.lock().unwrap().is_empty());
        assert!(t.env.elevated.exit_policies.lock().unwrap()[r"d:\apps\one.exe"].blocked);
        d.shutdown();
    }
}
