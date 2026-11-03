//! Mode and power modules together, on fake system calls: away/back, presets, lid, unlock,
//! tasks done, overheat and battery levels, the administrator layer refusing, crash
//! recovery and a clean stop. Nothing here touches the real system.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::{Env, agent_call, call, err_code, module_status, trusted, wait_events};
use daemon::dispatch::Dispatcher;
use daemon::modules::power::NEEDS_ADMIN_NOTICE;
use daemon::sys::fake::{FAKE_PLAN, RefusingElevated};
use daemon::sys::{PowerMode, PowerSetting, PowerValue, SystemEvent};
use daemon::{
    ArgKind, ArgSpec, CommandSpec, EventSpec, JournalEntry, Module, ModuleCtx, ModuleRequest,
    ModuleSpec,
};
use daemon::{RecoverOutcome, Section};
use proto::{ApiError, ErrorCode, subcode};
use serde_json::{Value, json};

/// Stands in for the task module: publishes the `active` count like the real one.
static FAKE_TASK: ModuleSpec =
    ModuleSpec::new("task", Section::Usage, "Test stand-in for the task module")
        .commands(&[
            CommandSpec::new("set", "Pretend this many tasks are protected")
                .agent()
                .args(&[ArgSpec::new("active", ArgKind::Uint, "").required()]),
        ])
        .publishes(&[
            EventSpec::new("protected", ""),
            EventSpec::new("unprotected", ""),
            EventSpec::new("active", ""),
        ]);

struct FakeTask(u64);

impl Module for FakeTask {
    fn spec(&self) -> &'static ModuleSpec {
        &FAKE_TASK
    }

    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        let active = req.args["active"].as_u64().unwrap();
        let event = if active > self.0 {
            "protected"
        } else {
            "unprotected"
        };
        self.0 = active;
        ctx.publish(
            event,
            json!({ "active": active, "task_id": 1, "by": "test" }),
        )?;
        Ok(json!({}))
    }

    fn recover(
        &mut self,
        _entry: &JournalEntry,
        _ctx: &ModuleCtx,
    ) -> Result<RecoverOutcome, ApiError> {
        Ok(RecoverOutcome::Restored)
    }
}

fn boot(env: &Env) -> Arc<Dispatcher> {
    let d = env.start(vec![
        daemon::modules::mode::create(),
        daemon::modules::power::create(),
        Box::new(FakeTask(0)),
    ]);
    wait_events(&d);
    d
}

fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn status(d: &Dispatcher) -> Value {
    agent_call(d, "status", json!({})).into_result().unwrap()
}

fn mode_of(d: &Dispatcher) -> String {
    module_status(&status(d), "mode")["state"]["mode"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn power_state(d: &Dispatcher) -> Value {
    module_status(&status(d), "power")["state"].clone()
}

fn value(ac: u32, dc: u32) -> PowerValue {
    PowerValue {
        plan: FAKE_PLAN.into(),
        ac,
        dc,
    }
}

fn open_keys(d: &Dispatcher) -> Vec<String> {
    let mut keys: Vec<String> = d
        .store()
        .journal_open(Some("power"))
        .unwrap()
        .into_iter()
        .map(|e| e.key)
        .collect();
    keys.sort();
    keys
}

fn set_tasks(d: &Dispatcher, n: u64) {
    assert!(agent_call(d, "task.set", json!({ "active": n })).ok);
    wait_events(d);
}

#[test]
fn away_changes_settings_through_the_journal_and_back_puts_them_back() {
    let env = Env::new();
    env.elevated
        .adapters
        .lock()
        .unwrap()
        .insert("wifi0".into(), true);
    let d = boot(&env);
    let sys = &env.system;
    assert_eq!(mode_of(&d), "normal");
    assert!(
        sys.awake_reasons().is_empty(),
        "nothing protected, nothing kept awake"
    );

    let r = agent_call(&d, "away", json!({})).into_result().unwrap();
    assert_eq!(
        (r["mode"].as_str(), r["changed"].as_bool()),
        (Some("away"), Some(true))
    );
    wait_events(&d);
    // Away preset: lid on AC keeps running, on battery it does not.
    assert_eq!(sys.setting(PowerSetting::LidAction), value(0, 1));
    assert_eq!(sys.setting(PowerSetting::UsbSuspend), value(0, 0));
    assert_eq!(sys.setting(PowerSetting::WifiPowerSaving), value(0, 0));
    assert_eq!(sys.setting(PowerSetting::SaverThreshold), value(20, 0));
    assert_eq!(
        sys.setting(PowerSetting::DisplayBrightness),
        value(80, 60),
        "screen off, not dimmed"
    );
    assert_eq!(sys.actions(), ["lock_screen", "display_off"]);
    assert_eq!(
        sys.awake_reasons(),
        [("away-guard: away mode".to_owned(), false)]
    );
    assert!(env.elevated.update_pause.lock().unwrap().is_some());
    assert_eq!(
        *env.elevated.adapters.lock().unwrap(),
        [("wifi0".to_owned(), false)].into()
    );
    assert_eq!(
        open_keys(&d),
        [
            "adapter_power_saving",
            "lid_action",
            "saver_threshold",
            "update_pause",
            "usb_suspend",
            "wifi_power_saving"
        ]
    );
    let p = power_state(&d);
    assert_eq!(p["mode"], "away");
    assert_eq!(p["changes"]["lid_action"]["state"], "applied");
    // Entering again changes nothing.
    assert_eq!(
        agent_call(&d, "away", json!({})).into_result().unwrap()["changed"],
        false
    );

    let r = agent_call(&d, "back", json!({})).into_result().unwrap();
    assert_eq!(r["mode"], "normal");
    assert!(r["report"]["away_ms"].is_number());
    wait_events(&d);
    assert_eq!(sys.setting(PowerSetting::LidAction), value(1, 1));
    assert_eq!(sys.setting(PowerSetting::UsbSuspend), value(1, 1));
    assert_eq!(sys.setting(PowerSetting::WifiPowerSaving), value(0, 2));
    assert_eq!(sys.setting(PowerSetting::SaverThreshold), value(20, 20));
    assert_eq!(*env.elevated.update_pause.lock().unwrap(), None);
    assert_eq!(
        *env.elevated.adapters.lock().unwrap(),
        [("wifi0".to_owned(), true)].into()
    );
    assert!(sys.awake_reasons().is_empty());
    assert!(open_keys(&d).is_empty());
    let reports = agent_call(&d, "mode.reports", json!({}))
        .into_result()
        .unwrap();
    assert_eq!(reports["reports"][0]["left_by"], "cli");
    assert_eq!(
        agent_call(&d, "back", json!({})).into_result().unwrap()["changed"],
        false
    );
}

#[test]
fn refused_administrator_changes_show_in_status_and_never_fail_away() {
    let mut env = Env::new();
    let refusing = Arc::new(RefusingElevated::default());
    env.config.sys.elevated = refusing.clone();
    let d = boot(&env);

    assert!(agent_call(&d, "away", json!({})).ok);
    wait_events(&d);
    // What needs no administrator rights still happens.
    assert_eq!(env.system.setting(PowerSetting::LidAction), value(0, 1));
    let p = power_state(&d);
    for key in ["update_pause", "adapter_power_saving"] {
        assert_eq!(p["changes"][key]["state"], "needs_admin", "{key}: {p}");
    }
    let notices: Vec<&str> = p["notices"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for key in ["update_pause", "adapter_power_saving"] {
        assert!(
            notices.contains(&format!("{key}: {NEEDS_ADMIN_NOTICE}").as_str()),
            "{notices:?}"
        );
    }
    // Refused at the read, so nothing was journaled; and not retried on every reading.
    assert!(!open_keys(&d).contains(&"update_pause".to_owned()));
    let tries = refusing
        .attempts
        .lock()
        .unwrap()
        .iter()
        .filter(|a| **a == "update_pause")
        .count();
    assert!(agent_call(&d, "power.refresh", json!({})).ok);
    assert_eq!(
        refusing
            .attempts
            .lock()
            .unwrap()
            .iter()
            .filter(|a| **a == "update_pause")
            .count(),
        tries
    );
    assert!(
        power_state(&d)["system_requests"]["error"]
            .as_str()
            .unwrap()
            .contains("administrator")
    );

    assert!(agent_call(&d, "back", json!({})).ok);
    wait_events(&d);
    assert!(open_keys(&d).is_empty());
    assert!(power_state(&d)["changes"].as_object().unwrap().is_empty());
}

#[test]
fn a_protected_task_keeps_the_machine_awake_and_closing_the_lid_means_away() {
    let env = Env::new();
    let d = boot(&env);
    let sys = &env.system;

    // Closing the lid without a protected task changes nothing.
    sys.emit(SystemEvent::Lid {
        closed: true,
        initial: false,
    });
    std::thread::sleep(Duration::from_millis(100));
    wait_events(&d);
    assert_eq!(mode_of(&d), "normal");
    sys.emit(SystemEvent::Lid {
        closed: false,
        initial: false,
    });

    set_tasks(&d, 1);
    // All modes keep a protected task awake; the lid is armed for the default preset.
    assert_eq!(
        sys.awake_reasons(),
        [("away-guard: 1 protected task(s)".to_owned(), false)]
    );
    assert_eq!(sys.setting(PowerSetting::LidAction), value(0, 1));
    assert_eq!(
        sys.setting(PowerSetting::UsbSuspend),
        value(1, 1),
        "only the lid is armed in normal mode"
    );

    sys.emit(SystemEvent::Lid {
        closed: true,
        initial: false,
    });
    eventually("lid close to enter away", || mode_of(&d) == "away");
    wait_events(&d);
    let m = module_status(&status(&d), "mode")["state"].clone();
    assert_eq!(
        (m["trigger"].as_str(), m["keep_running"].as_bool()),
        (Some("lid"), Some(false))
    );
    assert_eq!(sys.setting(PowerSetting::UsbSuspend), value(0, 0));

    // The last task ends: back to normal, everything put back.
    set_tasks(&d, 0);
    eventually("tasks done to end away", || mode_of(&d) == "normal");
    wait_events(&d);
    assert_eq!(
        module_status(&status(&d), "mode")["state"]["trigger"],
        "tasks_done"
    );
    assert_eq!(sys.setting(PowerSetting::LidAction), value(1, 1));
    assert!(sys.awake_reasons().is_empty());
    assert!(open_keys(&d).is_empty());
}

#[test]
fn unlocking_means_back_when_the_preset_locks_the_screen() {
    let env = Env::new();
    let d = boot(&env);
    assert!(agent_call(&d, "away", json!({})).ok);
    wait_events(&d);
    env.system.emit(SystemEvent::Session { locked: true });
    env.system.emit(SystemEvent::Session { locked: false });
    eventually("unlock to end away", || mode_of(&d) == "normal");
    assert_eq!(
        module_status(&status(&d), "mode")["state"]["trigger"],
        "unlock"
    );
}

#[test]
fn custom_preset_starts_as_away_is_stored_and_page_only_to_change() {
    let env = Env::new();
    let d = boot(&env);
    let show = agent_call(&d, "mode.show", json!({}))
        .into_result()
        .unwrap();
    assert_eq!(show["presets"]["custom"], show["presets"]["away"]);
    assert_eq!(
        show["presets"]["away"]["lid_on_battery"],
        json!({"enabled": false, "min_percent": 30})
    );
    assert_eq!(show["presets"]["away"].as_object().unwrap().len(), 9);

    let patch =
        json!({"settings": {"presets": {"custom": {"display": "dim", "lock_screen": false}}}});
    let resp = agent_call(&d, "mode.configure", patch.clone());
    assert_eq!(
        err_code(&resp),
        (ErrorCode::Denied, Some(subcode::AGENT_NOT_ALLOWED.into()))
    );
    let mut page = trusted(&d);
    for bad in [
        json!({"settings": {"presets": {"custom": {"dispaly": "dim"}}}}),
        json!({"settings": {"presets": {"custom": {"display": "blink"}}}}),
        json!({"settings": {"presets": {"away": {"display": "on"}}}}),
        json!({"settings": {"default_preset": "nope"}}),
    ] {
        assert_eq!(
            err_code(&call(&d, &mut page, "mode.configure", bad.clone())).0,
            ErrorCode::InvalidArgs,
            "{bad}"
        );
    }
    assert!(call(&d, &mut page, "mode.configure", patch).ok);

    assert!(agent_call(&d, "away", json!({"preset": "custom"})).ok);
    wait_events(&d);
    assert_eq!(mode_of(&d), "custom");
    // Dimmed (never brightened), no lock, no screen off.
    assert_eq!(
        env.system.setting(PowerSetting::DisplayBrightness),
        value(20, 20)
    );
    assert!(
        env.system.actions().is_empty(),
        "{:?}",
        env.system.actions()
    );
    // The page and tray may say why; an agent may not pretend to be a button.
    let resp = agent_call(&d, "away", json!({"trigger": "button"}));
    assert_eq!(
        err_code(&resp),
        (ErrorCode::Denied, Some("mode.TRIGGER_NOT_ALLOWED".into()))
    );
    assert!(agent_call(&d, "back", json!({})).ok);
    wait_events(&d);
    assert_eq!(
        env.system.setting(PowerSetting::DisplayBrightness),
        value(80, 60)
    );

    // Stored: a restarted daemon still has it.
    d.shutdown();
    drop(d);
    let d = boot(&env);
    let show = agent_call(&d, "mode.show", json!({}))
        .into_result()
        .unwrap();
    assert_eq!(show["presets"]["custom"]["display"], "dim");
    assert_eq!(show["presets"]["away"]["display"], "off");
}

#[test]
fn idle_entry_needs_the_setting_and_a_task() {
    let env = Env::new();
    let d = boot(&env);
    let mut tray = trusted(&d);
    let resp = call(&d, &mut tray, "away", json!({"trigger": "idle"}));
    assert_eq!(
        err_code(&resp),
        (ErrorCode::Conflict, Some("mode.IDLE_ENTRY_OFF".into()))
    );
    assert!(
        call(
            &d,
            &mut tray,
            "mode.configure",
            json!({"settings": {"idle_enter_minutes": 15}})
        )
        .ok
    );
    let r = call(&d, &mut tray, "away", json!({"trigger": "idle"}))
        .into_result()
        .unwrap();
    assert_eq!(
        (r["changed"].as_bool(), r["reason"].as_str()),
        (Some(false), Some("no protected task"))
    );
    set_tasks(&d, 2);
    let r = call(&d, &mut tray, "away", json!({"trigger": "idle"}))
        .into_result()
        .unwrap();
    assert_eq!(r["changed"], true);
    assert_eq!(
        module_status(&status(&d), "mode")["state"]["keep_running"],
        false
    );
}

#[test]
fn overheat_cools_down_then_takes_the_last_step() {
    let env = Env::new();
    let d = boot(&env);
    let sys = &env.system;
    let mut page = trusted(&d);
    assert!(
        call(
            &d,
            &mut page,
            "power.configure",
            json!({"settings": {"last_step_countdown_s": 0}})
        )
        .ok
    );

    sys.set_temp(86.0);
    assert!(agent_call(&d, "power.refresh", json!({})).ok);
    let p = power_state(&d);
    assert_eq!(p["levels"]["temperature"], "cool");
    assert_eq!(sys.setting(PowerSetting::CpuMaxPercent), value(70, 70));
    assert_eq!(*sys.power_mode.lock().unwrap(), PowerMode::EFFICIENCY);
    assert!(sys.actions().is_empty());

    sys.set_temp(95.0);
    assert!(agent_call(&d, "power.refresh", json!({})).ok);
    assert_eq!(sys.actions(), ["suspend(hibernate)"]);
    assert_eq!(power_state(&d)["critical"]["reason"], "temperature");

    // Cooled down (past the hysteresis): everything goes back.
    sys.set_temp(50.0);
    assert!(agent_call(&d, "power.refresh", json!({})).ok);
    let p = power_state(&d);
    assert_eq!(
        (p["levels"]["temperature"].as_str(), p["critical"].is_null()),
        (Some("ok"), true)
    );
    assert_eq!(sys.setting(PowerSetting::CpuMaxPercent), value(100, 100));
    assert_eq!(*sys.power_mode.lock().unwrap(), PowerMode::BALANCED);
    assert!(open_keys(&d).is_empty());

    // The lid closed makes every threshold stricter.
    sys.emit(SystemEvent::Lid {
        closed: true,
        initial: false,
    });
    sys.set_temp(81.0);
    eventually("lid reading", || {
        power_state(&d)["readings"]["lid_closed"] == true
    });
    assert!(agent_call(&d, "power.refresh", json!({})).ok);
    assert_eq!(power_state(&d)["levels"]["temperature"], "cool");
    assert!(
        power_state(&d)["notices"]
            .to_string()
            .contains("placeholders")
    );
}

#[test]
fn battery_floor_and_low_battery() {
    let env = Env::new();
    let d = boot(&env);
    let sys = &env.system;
    let mut page = trusted(&d);
    let custom = json!({"settings": {"presets": {"custom": {"lid_on_battery": {"enabled": true, "min_percent": 30}}}}});
    assert!(call(&d, &mut page, "mode.configure", custom).ok);
    sys.set_power(|p| {
        p.on_ac = Some(false);
        p.battery_percent = Some(50);
    });
    assert!(agent_call(&d, "power.refresh", json!({})).ok);
    assert!(agent_call(&d, "away", json!({"preset": "custom"})).ok);
    wait_events(&d);
    assert_eq!(
        sys.setting(PowerSetting::LidAction),
        value(0, 0),
        "battery above the floor"
    );

    // Under the floor: closing the lid on battery no longer keeps running …
    sys.set_power(|p| p.battery_percent = Some(25));
    assert!(agent_call(&d, "power.refresh", json!({})).ok);
    assert_eq!(sys.setting(PowerSetting::LidAction), value(0, 1));
    assert!(sys.actions().iter().all(|a| !a.starts_with("suspend")));
    // … and with the lid already closed the machine goes to sleep at once (away: no countdown).
    sys.emit(SystemEvent::Lid {
        closed: true,
        initial: false,
    });
    eventually("battery floor to hibernate", || {
        sys.actions().contains(&"suspend(hibernate)".to_owned())
    });
    assert_eq!(power_state(&d)["critical"]["reason"], "battery_floor");

    // Low battery levels count on battery only.
    sys.set_power(|p| p.battery_percent = Some(11));
    assert!(agent_call(&d, "power.refresh", json!({})).ok);
    assert_eq!(power_state(&d)["levels"]["battery"], "cool");
    assert_eq!(sys.setting(PowerSetting::CpuMaxPercent), value(70, 70));
    sys.set_power(|p| p.on_ac = Some(true));
    assert!(agent_call(&d, "power.refresh", json!({})).ok);
    assert_eq!(power_state(&d)["levels"]["battery"], "ok");
}

#[test]
fn hibernate_when_all_tasks_are_done_is_optional() {
    let env = Env::new();
    let d = boot(&env);
    let mut page = trusted(&d);
    assert!(
        call(
            &d,
            &mut page,
            "power.configure",
            json!({"settings": {"when_tasks_done": "hibernate"}})
        )
        .ok
    );
    set_tasks(&d, 1);
    set_tasks(&d, 0);
    assert!(
        env.system.actions().is_empty(),
        "not while someone is at the machine"
    );
    set_tasks(&d, 1);
    assert!(agent_call(&d, "away", json!({})).ok);
    wait_events(&d);
    set_tasks(&d, 0);
    eventually("hibernate after the last task", || {
        env.system
            .actions()
            .contains(&"suspend(hibernate)".to_owned())
    });
    eventually("back to normal", || mode_of(&d) == "normal");
}

#[test]
fn a_crash_during_an_absence_is_recovered_and_the_absence_resumes() {
    let env = Env::new();
    let d = boot(&env);
    assert!(agent_call(&d, "away", json!({})).ok);
    wait_events(&d);
    assert_eq!(env.system.setting(PowerSetting::LidAction), value(0, 1));
    drop(d); // crash: no stop, settings stay changed

    // Same boot: the journal puts everything back, then the absence carries on.
    let d = boot(&env);
    assert!(d.recovery().restored >= 4, "{:?}", d.recovery());
    assert_eq!(mode_of(&d), "away");
    eventually("settings applied again", || {
        env.system.setting(PowerSetting::LidAction) == value(0, 1)
    });
    drop(d);

    // After a reboot someone is back at the machine.
    env.system.uptime_ms.store(1_000, Ordering::SeqCst);
    let d = boot(&env);
    assert_eq!(mode_of(&d), "normal");
    assert_eq!(
        module_status(&status(&d), "mode")["state"]["trigger"],
        "reboot"
    );
    assert_eq!(env.system.setting(PowerSetting::LidAction), value(1, 1));
    assert!(open_keys(&d).is_empty());
    let reports = agent_call(&d, "mode.reports", json!({}))
        .into_result()
        .unwrap();
    assert_eq!(reports["reports"][0]["left_by"], "reboot");
}

#[test]
fn a_clean_stop_puts_everything_back() {
    let env = Env::new();
    let d = boot(&env);
    set_tasks(&d, 1);
    assert!(agent_call(&d, "away", json!({})).ok);
    wait_events(&d);
    assert!(!open_keys(&d).is_empty());
    d.shutdown();
    for s in PowerSetting::ALL {
        assert_eq!(
            env.system.setting(s),
            daemon::sys::fake::FakeSystem::default().setting(s),
            "{s:?}"
        );
    }
    assert!(env.system.awake_reasons().is_empty());
    assert_eq!(env.system.watchers(), 0);
    assert!(open_keys(&d).is_empty());
    drop(d);
    // Stopped on purpose means someone is there: the next start is normal.
    let d = boot(&env);
    assert_eq!(mode_of(&d), "normal");
}

#[test]
fn status_shows_battery_temperature_memory_and_network() {
    let env = Env::new();
    let d = boot(&env);
    assert!(
        agent_call(&d, "power.refresh", json!({})).ok,
        "the first reading may still be on its way"
    );
    let st = status(&d);
    let p = &module_status(&st, "power")["state"];
    let summary = p["summary"].as_str().unwrap();
    for part in [
        "battery 80% plugged in",
        "lid unknown",
        "45.0 °C",
        "memory 8.0 of 16.0 GB free",
        "network internet",
    ] {
        assert!(summary.contains(part), "{part} missing from {summary}");
    }
    assert_eq!(p["readings"]["capabilities"]["hibernate"], true);
    assert!(
        module_status(&st, "mode")["state"]["summary"]
            .as_str()
            .unwrap()
            .starts_with("normal")
    );
}
