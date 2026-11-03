//! Saved-line failover and recovery on a scripted machine. No real network is changed.
mod common;
use common::{Env, agent_call, call, trusted, wait_events};
use daemon::dispatch::Dispatcher;
use daemon::sys::network::{LinkReading, NetworkChange, WifiProfile};
use serde_json::{Value, json};
use std::sync::Arc;
const WIFI: &str = "a8776d53-0e5b-4023-b425-71dbd176cf2d";
const USB: &str = "b8776d53-0e5b-4023-b425-71dbd176cf2d";
const INTERNET: &str = "http://www.msftconnecttest.com/connecttest.txt";
fn ok(d: &Dispatcher, cmd: &str, args: Value) -> Value {
    agent_call(d, cmd, args).into_result().unwrap()
}
fn ui(d: &Dispatcher, cmd: &str, args: Value) -> Value {
    call(d, &mut trusted(d), cmd, args).into_result().unwrap()
}
fn online(env: &Env) {
    env.system.net.reply(INTERNET, "Microsoft Connect Test");
}
fn offline(env: &Env) {
    env.system.net.replies.lock().unwrap().remove(INTERNET);
}
fn check(d: &Dispatcher) -> Value {
    ok(d, "network.check", json!({}))
}
fn start(env: &Env) -> Arc<Dispatcher> {
    online(env);
    env.system.net.profiles.lock().unwrap().extend([
        WifiProfile {
            interface: WIFI.into(),
            name: "Home".into(),
            connected: true,
            available: true,
            hotspot_hint: false,
        },
        WifiProfile {
            interface: WIFI.into(),
            name: "Phone".into(),
            connected: false,
            available: true,
            hotspot_hint: true,
        },
    ]);
    env.system.net.links.lock().unwrap().push(LinkReading {
        interface: WIFI.into(),
        up: true,
        received_bytes: 100,
        sent_bytes: 100,
        metric: 25,
        automatic_metric: true,
    });
    let d = env.start(vec![
        daemon::modules::mode::create(),
        daemon::modules::network::create(),
    ]);
    wait_events(&d);
    ui(&d, "network.configure", json!({"settings":{"poll_millis":60000}}));
    std::thread::sleep(std::time::Duration::from_millis(20));
    check(&d);
    d
}
fn away(d: &Dispatcher) {
    ok(d, "away", json!({}));
    wait_events(d);
    std::thread::sleep(std::time::Duration::from_millis(10));
}
fn down(d: &Dispatcher) {
    for _ in 0..3 {
        check(d);
    }
}
fn state(d: &Dispatcher) -> Value {
    ok(d, "network.show", json!({}))
}

#[test]
fn three_consecutive_failures_notify_in_normal_mode_and_recovery_is_recorded() {
    let env = Env::new();
    let d = start(&env);
    offline(&env);
    check(&d);
    check(&d);
    assert_eq!(state(&d)["online"], true);
    check(&d);
    assert_eq!(state(&d)["online"], false);
    assert!(env.system.net.connections.lock().unwrap().is_empty());
    env.system.advance_ms(5000);
    online(&env);
    check(&d);
    let history = ok(&d, "network.history", json!({}));
    assert_eq!(history["entries"].as_array().unwrap().len(), 1);
    assert!(history["entries"][0]["ended_at"].is_i64());
    d.registry().stop_all();
}
#[test]
fn away_uses_saved_hotspot_marks_metered_and_back_restores() {
    let env = Env::new();
    let d = start(&env);
    away(&d);
    offline(&env);
    down(&d);
    assert_eq!(
        env.system.net.connections.lock().unwrap().last().unwrap().1,
        "Phone"
    );
    online(&env);
    check(&d);
    assert_eq!(state(&d)["online"], true);
    assert!(
        env.system
            .net
            .metered
            .lock()
            .unwrap()
            .iter()
            .any(|(_, p, v)| p == "Phone" && *v)
    );
    ok(&d, "back", json!({}));
    wait_events(&d);
    check(&d);
    assert_eq!(
        env.system.net.connections.lock().unwrap().last().unwrap().1,
        "Home"
    );
    assert!(
        env.system
            .net
            .metered
            .lock()
            .unwrap()
            .iter()
            .any(|(_, p, v)| p == "Phone" && !*v)
    );
    assert!(d.store().journal_open(Some("network")).unwrap().is_empty());
    d.registry().stop_all();
}
#[test]
fn captive_portals_and_api_only_outages_do_not_switch() {
    let env = Env::new();
    let d = start(&env);
    away(&d);
    env.system.net.reply(INTERNET, "<html>Sign in</html>");
    down(&d);
    assert!(env.system.net.connections.lock().unwrap().is_empty());
    assert_eq!(state(&d)["reading"]["reason"], "captive_portal");
    online(&env);
    ui(
        &d,
        "network.configure",
        json!({"settings":{"api_urls":["https://api.example.com/"]}}),
    );
    down(&d);
    assert!(env.system.net.connections.lock().unwrap().is_empty());
    assert_eq!(state(&d)["reading"]["reason"], "api_unreachable");
    d.registry().stop_all();
}
#[test]
fn custom_hotspot_switch_is_off_until_enabled_and_agent_cannot_change_it() {
    let env = Env::new();
    let d = start(&env);
    ui(
        &d,
        "mode.configure",
        json!({"settings":{"presets":{"custom":{"hotspot_failover":false}}}}),
    );
    ok(&d, "away", json!({"preset":"custom"}));
    wait_events(&d);
    offline(&env);
    down(&d);
    assert!(env.system.net.connections.lock().unwrap().is_empty());
    ui(
        &d,
        "mode.configure",
        json!({"settings":{"presets":{"custom":{"hotspot_failover":true}}}}),
    );
    wait_events(&d);
    check(&d);
    assert!(!env.system.net.connections.lock().unwrap().is_empty());
    assert_eq!(
        agent_call(&d, "network.configure", json!({"settings":{"failures":1}}))
            .error
            .unwrap()
            .code,
        proto::ErrorCode::Denied
    );
    d.registry().stop_all();
}
#[test]
fn main_signal_return_is_immediate_but_always_visible_main_waits_for_retry() {
    let env = Env::new();
    let d = start(&env);
    away(&d);
    offline(&env);
    down(&d);
    online(&env);
    check(&d);
    let n = env.system.net.connections.lock().unwrap().len();
    env.system.advance_ms(599_000);
    check(&d);
    assert_eq!(env.system.net.connections.lock().unwrap().len(), n);
    env.system.net.profiles.lock().unwrap()[0].available = false;
    check(&d);
    env.system.net.profiles.lock().unwrap()[0].available = true;
    check(&d);
    assert_eq!(
        env.system.net.connections.lock().unwrap().last().unwrap().1,
        "Home"
    );
    d.registry().stop_all();
}
#[test]
fn failed_main_trial_returns_to_phone_after_timeout() {
    let env = Env::new();
    let d = start(&env);
    away(&d);
    offline(&env);
    down(&d);
    online(&env);
    check(&d);
    env.system.advance_ms(601_000);
    check(&d);
    assert_eq!(
        env.system.net.connections.lock().unwrap().last().unwrap().1,
        "Home"
    );
    offline(&env);
    env.system.advance_ms(11_000);
    check(&d);
    assert_eq!(
        env.system.net.connections.lock().unwrap().last().unwrap().1,
        "Phone"
    );
    d.registry().stop_all();
}
#[test]
fn mobile_bytes_are_measured_from_interface_counters_and_persist() {
    let env = Env::new();
    let d = start(&env);
    away(&d);
    offline(&env);
    down(&d);
    online(&env);
    check(&d);
    env.system.net.links.lock().unwrap()[0].received_bytes += 2 * 1024 * 1024;
    check(&d);
    assert_eq!(state(&d)["mobile_bytes"], 2 * 1024 * 1024);
    d.registry().stop_all();
    drop(d);
    let d = env.start(vec![
        daemon::modules::mode::create(),
        daemon::modules::network::create(),
    ]);
    assert_eq!(state(&d)["mobile_bytes"], 2 * 1024 * 1024);
    d.registry().stop_all();
}
#[test]
fn backup_metrics_are_journaled_and_restore_automatic_state() {
    let env = Env::new();
    let d = start(&env);
    env.system.net.links.lock().unwrap().push(LinkReading {
        interface: USB.into(),
        up: true,
        received_bytes: 0,
        sent_bytes: 0,
        metric: 100,
        automatic_metric: true,
    });
    ui(
        &d,
        "network.configure",
        json!({"settings":{"backup_interface":USB}}),
    );
    std::thread::sleep(std::time::Duration::from_millis(10));
    away(&d);
    offline(&env);
    down(&d);
    assert!(
        env.elevated
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.contains("set_interface_metric") && s.contains(USB))
    );
    assert!(env.system.net.connections.lock().unwrap().is_empty());
    ok(&d, "back", json!({}));
    wait_events(&d);
    check(&d);
    assert!(
        env.elevated
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.contains("restore_interface_metric") && s.contains("true"))
    );
    d.registry().stop_all();
}
#[test]
fn crash_recovery_restores_owned_wifi_even_when_module_is_disabled() {
    let env = Env::new();
    let d = start(&env);
    away(&d);
    offline(&env);
    down(&d);
    drop(d);
    env.write_config(json!({"modules":{"network":{"enabled":false}}}));
    let d = env.start(vec![
        daemon::modules::mode::create(),
        daemon::modules::network::create(),
    ]);
    assert_eq!(
        env.system.net.connections.lock().unwrap().last().unwrap().1,
        "Home"
    );
    assert!(d.store().journal_open(Some("network")).unwrap().is_empty());
    d.registry().stop_all();
}
#[test]
fn notifications_trigger_checks_and_invalid_settings_are_rejected() {
    let env = Env::new();
    let d = start(&env);
    let n = env.system.net.requests.lock().unwrap().len();
    env.system.net.emit(NetworkChange::Address);
    for _ in 0..100 {
        if env.system.net.requests.lock().unwrap().len() > n {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(env.system.net.requests.lock().unwrap().len() > n);
    for patch in [
        json!({"failures":0}),
        json!({"internet_url":"http://bad"}),
        json!({"typo":true}),
        json!({"backup_interface":"shell;calc"}),
    ] {
        assert_eq!(
            call(
                &d,
                &mut trusted(&d),
                "network.configure",
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
fn restoring_a_previously_disconnected_adapter_preserves_a_users_new_connection() {
    let env = Env::new();
    let d = start(&env);
    env.system.net.profiles.lock().unwrap()[0].connected = false;
    ui(
        &d,
        "network.configure",
        json!({"settings":{"lines":[{"interface":WIFI,"profile":"Phone","hotspot":true}]}}),
    );
    std::thread::sleep(std::time::Duration::from_millis(10));
    away(&d);
    offline(&env);
    down(&d);
    assert_eq!(
        env.system.net.connections.lock().unwrap().last().unwrap().1,
        "Phone"
    );
    {
        let mut profiles = env.system.net.profiles.lock().unwrap();
        profiles[0].connected = true;
        profiles[1].connected = false;
    }
    ok(&d, "back", json!({}));
    wait_events(&d);
    check(&d);
    assert!(env.system.net.profiles.lock().unwrap()[0].connected);
    assert!(d.store().journal_open(Some("network")).unwrap().is_empty());
    d.registry().stop_all();
}

#[test]
fn moving_to_office_updates_main_line_and_normalizes_interface_ids() {
    let env = Env::new();
    let d = start(&env);
    {
        let mut profiles = env.system.net.profiles.lock().unwrap();
        profiles[0].connected = false;
        profiles[0].available = false;
        profiles.push(WifiProfile {
            interface: WIFI.into(),
            name: "Office".into(),
            connected: true,
            available: true,
            hotspot_hint: false,
        });
    }
    ui(
        &d,
        "network.configure",
        json!({"settings":{"lines":[
            {"interface":format!("{{{}}}", WIFI.to_uppercase()),"profile":"Office"},
            {"interface":WIFI,"profile":"Phone","hotspot":true}
        ]}}),
    );
    check(&d);
    assert_eq!(state(&d)["main"]["profile"], "Office");
    away(&d);
    offline(&env);
    down(&d);
    online(&env);
    check(&d);
    env.system.advance_ms(601_000);
    check(&d);
    assert_eq!(
        env.system.net.connections.lock().unwrap().last().unwrap().1,
        "Office"
    );
    d.registry().stop_all();
}
#[test]
fn mobile_limit_resets_each_utc_calendar_month() {
    let env = Env::new();
    let d = start(&env);
    away(&d);
    offline(&env);
    down(&d);
    online(&env);
    check(&d);
    env.system.net.links.lock().unwrap()[0].received_bytes += 2 * 1024 * 1024;
    check(&d);
    assert_eq!(state(&d)["mobile_bytes"], 2 * 1024 * 1024);
    env.system.advance_ms(32 * 86_400_000);
    check(&d);
    assert_eq!(state(&d)["mobile_bytes"], 0);
    d.registry().stop_all();
}

#[test]
fn proxy_variants_can_change_and_reject_unknown_fields() {
    let env = Env::new();
    let d = start(&env);
    ui(
        &d,
        "network.configure",
        json!({"settings":{"proxy":{"kind":"explicit","url":"http://127.0.0.1:7890"}}}),
    );
    ui(
        &d,
        "network.configure",
        json!({"settings":{"proxy":{"kind":"system"}}}),
    );
    let result = call(
        &d,
        &mut trusted(&d),
        "network.configure",
        json!({"settings":{"proxy":{"kind":"explicit","url":"http://127.0.0.1:7890","typo":1}}}),
    );
    assert!(!result.ok);
    d.registry().stop_all();
}
