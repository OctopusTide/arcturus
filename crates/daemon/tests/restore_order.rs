//! Both real modules are loaded, all system calls are simulated.
mod common;
use common::{Env, agent_call, wait_events};
use daemon::sys::PowerSetting;
use serde_json::json;
fn run(fail_thaw: bool) {
    let env = Env::new();
    env.system.spawn(100, 0, "browser.exe").set_memory_mb(100, 4000);
    env.system.memory.lock().unwrap().available_mb = 500;
    let d = env.start(vec![
        daemon::modules::mode::create(),
        daemon::modules::permission::create(),
        daemon::modules::task::create(),
        daemon::modules::power::create(),
        daemon::modules::resource::create(),
    ]);
    wait_events(&d);
    assert!(agent_call(&d, "away", json!({})).ok);
    wait_events(&d);
    assert!(agent_call(&d, "resource.check", json!({})).ok);
    wait_events(&d);
    assert!(env.system.suspended.lock().unwrap().contains(&100));
    assert_eq!(env.system.setting(PowerSetting::LidAction).ac, 0);
    env.system.power_writes.lock().unwrap().clear();
    if fail_thaw {
        env.system.set_exe(100, None);
    }
    assert!(agent_call(&d, "back", json!({})).ok);
    wait_events(&d);
    if fail_thaw {
        assert_eq!(
            env.system.setting(PowerSetting::LidAction).ac,
            0,
            "power must wait first"
        );
        assert!(env.system.power_writes.lock().unwrap().is_empty());
        env.system.advance_ms(10001);
        assert!(agent_call(&d, "power.refresh", json!({})).ok);
        wait_events(&d);
        assert_eq!(
            env.system.setting(PowerSetting::LidAction).ac,
            1,
            "timeout must release power recovery"
        );
        assert!(
            env.system
                .notifications
                .lock()
                .unwrap()
                .iter()
                .any(|(_, m)| m.contains("10 秒"))
        );
        env.system.set_exe(100, Some(r"C:\fake\browser.exe"));
        assert!(agent_call(&d, "resource.check", json!({})).ok);
        wait_events(&d);
        assert!(!env.system.suspended.lock().unwrap().contains(&100));
    } else {
        assert!(agent_call(&d, "power.refresh", json!({})).ok);
        wait_events(&d);
        assert_eq!(env.system.setting(PowerSetting::LidAction).ac, 1);
        let writes = env.system.power_writes.lock().unwrap();
        assert!(!writes.is_empty());
        assert!(
            writes.iter().all(|(_, s)| s.is_empty()),
            "every setting write follows thaw"
        );
    }
    d.registry().stop_all();
}
#[test]
fn successful_thaw_precedes_every_power_restore() {
    run(false);
}
#[test]
fn failed_thaw_waits_then_restores_power_and_keeps_retrying() {
    run(true);
}
