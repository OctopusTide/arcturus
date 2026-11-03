//! End-to-end policy lifecycle, using only fake network, firewall and process APIs.
mod common;
use common::{Env, agent_call, call, module_status, trusted, wait_events};
use daemon::{
    dispatch::Dispatcher,
    sys::{
        System,
        exit_firewall::{ExitFirewallPolicy, ExitRoute},
        fake::RefusingElevated,
        network::{Network, fake::FakeNetwork},
    },
};
use proto::ApiError;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
const PATH: &str = r"D:\apps\worker.exe";
const KEY: &str = r"d:\apps\worker.exe";
const VPN: &str = "11111111-2222-3333-4444-555555555555";
struct SystemFake {
    net: Arc<FakeNetwork>,
    notices: Mutex<Vec<String>>,
}
impl System for SystemFake {
    fn process_image_path(&self, _: u32) -> Option<String> {
        None
    }
    fn uptime_ms(&self) -> Result<u64, ApiError> {
        Ok(3600000)
    }
    fn network(&self) -> &dyn Network {
        self.net.as_ref()
    }
    fn notify_user(&self, _: &str, message: &str) -> Result<(), ApiError> {
        self.notices.lock().unwrap().push(message.into());
        Ok(())
    }
}
fn env() -> (Env, Arc<FakeNetwork>) {
    let mut e = Env::new();
    let n = Arc::new(FakeNetwork::default());
    e.config.sys.system = Arc::new(SystemFake {
        net: n.clone(),
        notices: Mutex::default(),
    });
    (e, n)
}
fn boot(e: &Env) -> Arc<Dispatcher> {
    let d = e.start(vec![
        daemon::modules::mode::create(),
        daemon::modules::exit_guard::create(),
    ]);
    wait_events(&d);
    d
}
fn state(d: &Dispatcher) -> Value {
    module_status(
        &agent_call(d, "status", json!({})).into_result().unwrap(),
        "exit_guard",
    )["state"]
        .clone()
}
fn settle(d: &Dispatcher, count: u64) -> Value {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let s = state(d);
        if s["checks"].as_u64().unwrap() >= count {
            wait_events(d);
            return s;
        }
        assert!(Instant::now() < end, "{s}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn configure(d: &Dispatcher, patch: Value) {
    let r = call(
        d,
        &mut trusted(d),
        "exit_guard.configure",
        json!({"settings":patch}),
    );
    assert!(r.ok, "{r:?}");
}
fn settings() -> Value {
    json!({"region":{"country":"SG"},"programs":[{"path":PATH}],"firewall_route":{"kind":"vpn","interface":VPN}})
}
fn good(n: &FakeNetwork) {
    n.reply(
        daemon::modules::exit_guard::probe::SERVICES[0],
        "ip=203.0.113.8\nloc=SG\n",
    );
}

#[test]
fn guards_exist_before_probe_and_recovery_unblocks_only_the_temporary_lock() {
    let (e, n) = env();
    let backend = e.elevated.clone();
    *n.before_get.lock().unwrap() = Some(Arc::new(move || {
        assert!(
            backend
                .exit_policies
                .lock()
                .unwrap()
                .get(KEY)
                .is_some_and(|p| p.blocked)
        );
    }));
    let d = boot(&e);
    configure(&d, settings());
    let s = settle(&d, 1);
    assert_eq!(
        s["last_check"]["programs"][0]["action_result"]["state"],
        "applied"
    );
    assert!(e.elevated.exit_policies.lock().unwrap()[KEY].blocked);
    good(&n);
    n.emit(daemon::sys::network::NetworkChange::Route);
    settle(&d, 2);
    let p = e.elevated.exit_policies.lock().unwrap()[KEY].clone();
    assert!(!p.blocked);
    assert_eq!(
        p.route,
        ExitRoute::Vpn {
            interface: VPN.into()
        }
    );
    assert!(n.guarded_requests.lock().unwrap().iter().all(|r| *r
        == ExitRoute::Vpn {
            interface: VPN.into()
        }));
    d.shutdown();
    assert!(e.elevated.exit_policies.lock().unwrap()[KEY].blocked);
}
#[test]
fn removing_program_removes_stale_owned_policy_and_closes_its_journal() {
    let (e, n) = env();
    good(&n);
    let original = ExitFirewallPolicy {
        program: KEY.into(),
        route: ExitRoute::LocalProxy {
            endpoint: "127.0.0.1:5555".parse().unwrap(),
        },
        blocked: true,
    };
    e.elevated
        .exit_policies
        .lock()
        .unwrap()
        .insert(KEY.into(), original.clone());
    let d = boot(&e);
    configure(&d, settings());
    settle(&d, 1);
    configure(&d, json!({"programs":[]}));
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        if !e.elevated.exit_policies.lock().unwrap().contains_key(KEY) {
            break;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        agent_call(&d, "status", json!({})).into_result().unwrap()["journal"]["open"],
        0
    );
    d.shutdown();
}
#[test]
fn service_unavailable_never_reports_firewall_applied() {
    let (mut e, n) = env();
    good(&n);
    e.config.sys.elevated = Arc::new(RefusingElevated::default());
    let d = boot(&e);
    configure(&d, settings());
    let s = settle(&d, 1);
    assert_eq!(s["firewall"]["state"], "needs_admin");
    assert_eq!(s["firewall"]["applied"], false);
    assert_eq!(s["last_check"]["programs"][0]["firewall"]["applied"], false);
    d.shutdown();
}
#[test]
fn restart_adopts_persistent_guard_and_rechecks_before_allowing() {
    let (e, n) = env();
    good(&n);
    let d = boot(&e);
    configure(&d, settings());
    settle(&d, 1);
    d.shutdown();
    drop(d);
    assert!(e.elevated.exit_policies.lock().unwrap()[KEY].blocked);
    let backend = e.elevated.clone();
    *n.before_get.lock().unwrap() = Some(Arc::new(move || {
        assert!(backend.exit_policies.lock().unwrap()[KEY].blocked)
    }));
    let d = boot(&e);
    settle(&d, 1);
    assert!(!e.elevated.exit_policies.lock().unwrap()[KEY].blocked);
    d.shutdown();
}
#[test]
fn no_selected_route_remains_blocked_even_when_country_matches() {
    let (e, n) = env();
    good(&n);
    let d = boot(&e);
    let mut s = settings();
    s["firewall_route"] = Value::Null;
    configure(&d, s);
    let s = settle(&d, 1);
    assert_eq!(s["last_check"]["programs"][0]["matches"], true);
    let p = e.elevated.exit_policies.lock().unwrap()[KEY].clone();
    assert!(p.blocked);
    assert_eq!(p.route, ExitRoute::DenyAll);
    d.shutdown();
}
#[test]
fn native_away_report_contains_the_final_egress_action_result() {
    let (e, _) = env();
    let d = boot(&e);
    assert!(agent_call(&d, "away", json!({})).ok);
    wait_events(&d);
    configure(&d, settings());
    settle(&d, 1);
    let back = agent_call(&d, "back", json!({})).into_result().unwrap();
    assert_eq!(back["report"]["exit_guard"][0]["path"], PATH);
    assert_eq!(
        back["report"]["exit_guard"][0]["action_result"]["state"],
        "applied"
    );
    let report = agent_call(&d, "mode.reports", json!({})).into_result().unwrap();
    assert_eq!(report["reports"][0]["exit_guard"].as_array().unwrap().len(), 1);
    d.shutdown();
}
#[test]
fn interface_inventory_uses_the_generic_read_only_boundary() {
    let (e, n) = env();
    n.interfaces
        .lock()
        .unwrap()
        .push(daemon::sys::network::NetworkInterface {
            id: VPN.into(),
            name: "test VPN".into(),
            up: true,
            tunnel: true,
        });
    let d = boot(&e);
    assert_eq!(
        agent_call(&d, "exit_guard.routes", json!({}))
            .into_result()
            .unwrap()["interfaces"][0]["id"],
        VPN
    );
    assert!(n.requests.lock().unwrap().is_empty());
    d.shutdown();
}

#[test]
fn late_action_event_updates_the_closed_native_report_without_duplicate_appendices() {
    let (e, _) = env();
    let d = boot(&e);
    agent_call(&d, "away", json!({}));
    wait_events(&d);
    let back = agent_call(&d, "back", json!({})).into_result().unwrap();
    wait_events(&d);
    let at = back["report"]["started_at"].as_i64().unwrap();
    for state in ["pending", "applied"] {
        d.registry().bus().publish(daemon::Event {
            topic: if state == "pending" {
                "exit_guard.violation"
            } else {
                "exit_guard.acted"
            }
            .into(),
            source: "exit_guard".into(),
            ts: at,
            data: json!({"incident_id":99,"path":PATH,"at":at,"action_result":{"state":state}}),
        });
        wait_events(&d);
    }
    let r = agent_call(&d, "mode.reports", json!({})).into_result().unwrap();
    assert_eq!(r["reports"][0]["exit_guard"].as_array().unwrap().len(), 1);
    assert_eq!(
        r["reports"][0]["exit_guard"][0]["action_result"]["state"],
        "applied"
    );
    assert_eq!(
        agent_call(&d, "exit_guard.reports", json!({}))
            .into_result()
            .unwrap()["reports"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    d.shutdown();
}

#[test]
fn malformed_stored_settings_retain_the_existing_firewall_lock() {
    for damaged in [json!("damaged"), Value::Null] {
        let (e, n) = env();
        good(&n);
        let d = boot(&e);
        configure(&d, settings());
        settle(&d, 1);
        d.shutdown();
        drop(d);
        let store = daemon::store::Store::open(&e.db_path(), e.config.sqlite.as_ref()).unwrap();
        store
            .setting_set("exit_guard", "settings", &json!({"programs":damaged}))
            .unwrap();
        drop(store);
        let d = boot(&e);
        let status = agent_call(&d, "status", json!({})).into_result().unwrap();
        assert_eq!(module_status(&status, "exit_guard")["health"], "faulted");
        assert!(e.elevated.exit_policies.lock().unwrap()[KEY].blocked);
        assert!(
            !e.elevated
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("remove_exit_policy"))
        );
        d.shutdown();
    }
}

#[test]
fn rechecking_a_valid_route_never_adds_a_temporary_lock() {
    let (e, n) = env();
    good(&n);
    let d = boot(&e);
    configure(&d, settings());
    settle(&d, 1);
    let backend = e.elevated.clone();
    *n.before_get.lock().unwrap() = Some(Arc::new(move || {
        assert!(!backend.exit_policies.lock().unwrap()[KEY].blocked);
    }));
    let before = e.elevated.calls.lock().unwrap().len();
    n.emit(daemon::sys::network::NetworkChange::Address);
    settle(&d, 2);
    assert!(
        e.elevated.calls.lock().unwrap()[before..]
            .iter()
            .all(|s| !s.contains(", true)"))
    );
    d.shutdown();
}
#[test]
fn disabling_module_removes_its_persistent_rules() {
    let (e, n) = env();
    good(&n);
    let d = boot(&e);
    configure(&d, settings());
    settle(&d, 1);
    d.shutdown();
    drop(d);
    e.write_config(json!({"modules":{"exit_guard":{"enabled":false}}}));
    let d = boot(&e);
    assert!(!e.elevated.exit_policies.lock().unwrap().contains_key(KEY));
    assert!(d.store().journal_open(Some("exit_guard")).unwrap().is_empty());
    d.shutdown();
}
#[test]
fn remote_program_paths_are_rejected_before_privileged_resolution() {
    for path in [
        r"\\server\share\agent.exe",
        r"\\?\UNC\server\share\agent.exe",
        r"\\.\pipe\agent.exe",
    ] {
        assert!(daemon::modules::exit_guard::config::path_key(path).is_err());
    }
}

#[test]
fn a_failed_block_is_reported_then_retried_without_another_network_event() {
    use daemon::sys::Elevated;
    use std::sync::atomic::{AtomicU32, Ordering};
    struct RecoveringFirewall {
        attempts: AtomicU32,
        policy: Mutex<Option<ExitFirewallPolicy>>,
    }
    impl Elevated for RecoveringFirewall {
        fn set_interface_metric(&self, _: &str, _: u32) -> Result<(), ApiError> {
            unreachable!()
        }
        fn terminate_process(&self, _: u32) -> Result<(), ApiError> {
            unreachable!()
        }
        fn backend(&self) -> &'static str {
            "test"
        }
        fn exit_policy(&self, _: &str) -> Result<Option<ExitFirewallPolicy>, ApiError> {
            Ok(self.policy.lock().unwrap().clone())
        }
        fn set_exit_policy(&self, p: &ExitFirewallPolicy) -> Result<(), ApiError> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                return Err(ApiError::internal("temporary firewall failure"));
            }
            *self.policy.lock().unwrap() = Some(p.clone());
            Ok(())
        }
        fn set_exit_blocked(&self, _: &str, b: bool) -> Result<(), ApiError> {
            self.policy
                .lock()
                .unwrap()
                .as_mut()
                .ok_or_else(|| ApiError::internal("policy missing"))?
                .blocked = b;
            Ok(())
        }
        fn remove_exit_policy(&self, _: &str) -> Result<(), ApiError> {
            Ok(())
        }
    }
    let (mut e, n) = env();
    let system = Arc::new(SystemFake {
        net: n.clone(),
        notices: Mutex::default(),
    });
    e.config.sys.system = system.clone();
    let firewall = Arc::new(RecoveringFirewall {
        attempts: AtomicU32::new(0),
        policy: Mutex::default(),
    });
    e.config.sys.elevated = firewall.clone();
    let d = boot(&e);
    configure(&d, settings());
    let first = settle(&d, 1);
    assert_eq!(first["last_check"]["programs"][0]["firewall"]["applied"], false);
    assert!(
        system
            .notices
            .lock()
            .unwrap()
            .iter()
            .any(|n| n.contains("保护操作失败"))
    );
    let requests = n.requests.lock().unwrap().len();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !firewall
        .policy
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|p| p.blocked)
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        n.requests.lock().unwrap().len(),
        requests,
        "retry applies firewall without refetching public services"
    );
    assert!(firewall.attempts.load(Ordering::SeqCst) >= 2);
    d.registry().stop_all();
}
