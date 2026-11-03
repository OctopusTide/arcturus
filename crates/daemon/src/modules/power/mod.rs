//! 电源 (power): the machine level — may it sleep, what closing the lid does, how overheating
//! or a flat battery end, and the readings `status` shows (battery, temperature, memory,
//! network). Freezing or ending programs belongs to the resource module; this module only
//! publishes `power.overheat` and friends.
//!
//! It learns everything from events: `mode.changed` (mode and effective preset) and the
//! `active` count on `task.*`. From those, its readings and the overheat / battery levels it
//! works out a set of wanted changes ([`Targets`]) and reconciles the system with it: every
//! change is journaled first (original value), applied, and put back when no longer wanted,
//! when the module stops, or by `recover` after a crash. Changes the administrator layer
//! refuses are shown in `status` as [`NEEDS_ADMIN_NOTICE`]; they never fail a command.

mod levels;

use std::collections::BTreeMap;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use proto::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::dlog;
use crate::module::{
    ArgKind, ArgSpec, CommandSpec, Event, EventSpec, Module, ModuleCtx, ModuleRequest, ModuleSpec,
    RecoverOutcome, Section,
};
use crate::modules::settings;
use crate::store::{JournalEntry, now_ms};
use crate::sys::{
    ConnectivityHint, Guard, MemoryStatus, PowerCapabilities, PowerMode, PowerSetting, PowerValue, Suspend,
    SystemEvent, Temperature, is_needs_elevation,
};
use levels::{Discharge, LEVEL_NAMES, Level, Thresholds, falling_level, rising_level};

static SPEC: ModuleSpec = ModuleSpec::new("power", Section::Usage, "Sleep, lid, overheat and battery")
    .commands(&[
        CommandSpec::new("settings", "Power settings and thresholds")
            .agent()
            .read_only(),
        CommandSpec::new("configure", "Change power settings (thresholds, last step …)").args(&[
            ArgSpec::new(
                "settings",
                ArgKind::Json,
                "Partial settings object, e.g. {\"last_step\":\"sleep\"}",
            )
            .required(),
        ]),
        CommandSpec::new("refresh", "Take fresh readings now").agent(),
    ])
    .publishes(&[
        EventSpec::new("lid", "The lid was closed or opened"),
        EventSpec::new("session", "The session was locked or unlocked"),
        EventSpec::new("source", "The machine was plugged in or unplugged"),
        EventSpec::new("reading", "Fresh readings (battery, temperature)"),
        EventSpec::new("notice", "A bounded recovery needs attention"),
        EventSpec::new("overheat", "The temperature level changed"),
        EventSpec::new("low_battery", "The battery level changed"),
        EventSpec::new("critical", "A protective sleep is pending, or was called off"),
        EventSpec::new(
            "suspending",
            "The daemon is about to sleep, hibernate or shut the machine down",
        ),
    ])
    .subscribes(&["mode.changed", "task.*", "resource.thawed", "resume.active"]);

pub use crate::modules::NEEDS_ADMIN_NOTICE;

// ---- settings ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LastStep {
    Hibernate,
    Sleep,
    ShutDown,
    /// Only warn.
    None,
}

impl LastStep {
    fn suspend(self) -> Option<Suspend> {
        match self {
            LastStep::Hibernate => Some(Suspend::Hibernate),
            LastStep::Sleep => Some(Suspend::Sleep),
            LastStep::ShutDown => Some(Suspend::ShutDown),
            LastStep::None => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhenTasksDone {
    /// Put everything back and let the machine sleep on its own timetable.
    Normal,
    /// Hibernate right away (only while away).
    Hibernate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerSettings {
    /// How often readings are taken.
    pub poll_seconds: u32,
    /// °C: warn, cool down, last step. Placeholders until the lid test (task 2).
    pub temperature: Thresholds,
    /// With the lid closed every temperature threshold is this much lower.
    pub lid_closed_margin_c: f64,
    pub temperature_hysteresis_c: f64,
    /// Battery %, on battery only: warn, cool down (save power), last step.
    pub battery: Thresholds,
    pub battery_hysteresis: f64,
    /// CPU maximum while cooling down or saving battery.
    pub cool_cpu_max_percent: u32,
    /// What the last step does.
    pub last_step: LastStep,
    /// In normal mode the last step waits this long, so the person can save their work.
    pub last_step_countdown_s: u32,
    pub when_tasks_done: WhenTasksDone,
    /// Brightness for a preset whose screen is "dim".
    pub dim_brightness_percent: u32,
    /// How long OS updates are paused while away (needs administrator rights).
    pub update_pause_days: u32,
    /// False while the temperature thresholds are still the placeholders.
    pub thresholds_confirmed: bool,
}

impl Default for PowerSettings {
    fn default() -> Self {
        PowerSettings {
            poll_seconds: 10,
            temperature: Thresholds {
                warn: 80.0,
                cool: 85.0,
                critical: 92.0,
            },
            lid_closed_margin_c: 5.0,
            temperature_hysteresis_c: 3.0,
            battery: Thresholds {
                warn: 20.0,
                cool: 12.0,
                critical: 6.0,
            },
            battery_hysteresis: 2.0,
            cool_cpu_max_percent: 70,
            last_step: LastStep::Hibernate,
            last_step_countdown_s: 60,
            when_tasks_done: WhenTasksDone::Normal,
            dim_brightness_percent: 20,
            update_pause_days: 7,
            thresholds_confirmed: false,
        }
    }
}

impl PowerSettings {
    fn check(&self) -> Result<(), String> {
        let t = &self.temperature;
        if !(t.warn < t.cool && t.cool < t.critical) {
            return Err("temperature thresholds must rise: warn < cool < critical".into());
        }
        let b = &self.battery;
        if !(b.warn > b.cool && b.cool > b.critical && b.critical >= 0.0 && b.warn <= 100.0) {
            return Err("battery thresholds must fall: 100 >= warn > cool > critical >= 0".into());
        }
        if !(1..=600).contains(&self.poll_seconds) {
            return Err("poll_seconds must be 1..=600".into());
        }
        if !(10..=100).contains(&self.cool_cpu_max_percent) || self.dim_brightness_percent > 100 {
            return Err("cool_cpu_max_percent must be 10..=100 and dim_brightness_percent 0..=100".into());
        }
        if !(1..=35).contains(&self.update_pause_days) {
            return Err("update_pause_days must be 1..=35".into());
        }
        if self.lid_closed_margin_c < 0.0
            || self.temperature_hysteresis_c < 0.0
            || self.battery_hysteresis < 0.0
        {
            return Err("margins and hysteresis cannot be negative".into());
        }
        Ok(())
    }

    fn temperature_now(&self, lid_closed: bool) -> Thresholds {
        if lid_closed {
            self.temperature.shifted(-self.lid_closed_margin_c)
        } else {
            self.temperature
        }
    }
}

// ---- what other modules tell us ------------------------------------------------------------

/// The parts of a preset (from `mode.changed`) this module acts on.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
struct PresetView {
    lock_screen: bool,
    display: String,
    defer_updates: bool,
    lid_on_ac: bool,
    lid_on_battery: LidView,
    no_power_saving: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
struct LidView {
    enabled: bool,
    min_percent: u8,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
struct ModeView {
    mode: String,
    preset: Option<String>,
    /// Effective preset; `None` in normal mode.
    settings: Option<PresetView>,
    /// The preset closing the lid would enter.
    default_settings: Option<PresetView>,
}

impl Default for ModeView {
    fn default() -> Self {
        ModeView {
            mode: "normal".into(),
            preset: None,
            settings: None,
            default_settings: None,
        }
    }
}

// ---- wanted state --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Change {
    Set(u32),
    /// Lower it to this, never raise it.
    AtMost(u32),
}

impl Change {
    fn apply(self, original: u32) -> u32 {
        match self {
            Change::Set(v) => v,
            Change::AtMost(v) => original.min(v),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Override {
    ac: Option<Change>,
    dc: Option<Change>,
}

impl Override {
    fn both(c: Change) -> Self {
        Override {
            ac: Some(c),
            dc: Some(c),
        }
    }
}

/// Everything the module wants changed right now. Anything absent is put back.
#[derive(Debug, Default)]
struct Targets {
    awake: Option<(String, bool)>,
    plan: BTreeMap<PowerSetting, Override>,
    power_mode: Option<&'static str>,
    update_pause: bool,
    adapters_off: bool,
}

const POWER_MODE_KEY: &str = "power_mode";
const UPDATE_PAUSE_KEY: &str = "update_pause";
const ADAPTERS_KEY: &str = "adapter_power_saving";

/// How one managed change is doing, for `status`.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct Item {
    state: &'static str,
    detail: String,
}

impl Item {
    fn applied(detail: impl Into<String>) -> Self {
        Item {
            state: "applied",
            detail: detail.into(),
        }
    }

    fn from_error(e: &ApiError) -> Self {
        if is_needs_elevation(e) {
            Item {
                state: "needs_admin",
                detail: NEEDS_ADMIN_NOTICE.into(),
            }
        } else if e.code == ErrorCode::NotImplemented {
            Item {
                state: "unsupported",
                detail: format!("not available on this machine: {}", e.message),
            }
        } else {
            Item {
                state: "failed",
                detail: e.message.clone(),
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
struct Readings {
    at: i64,
    on_ac: Option<bool>,
    battery_present: bool,
    battery_percent: Option<u8>,
    charging: bool,
    saver_on: bool,
    /// From the measured discharge rate.
    remaining_min: Option<u64>,
    /// The OS's own estimate.
    os_remaining_min: Option<u64>,
    lid_closed: Option<bool>,
    locked: Option<bool>,
    display_on: Option<bool>,
    temp_c: Option<f64>,
    zones: Vec<Temperature>,
    memory: Option<MemoryStatus>,
    network: Option<ConnectivityHint>,
    capabilities: Option<PowerCapabilities>,
    /// Readings that could not be taken, by name.
    errors: BTreeMap<&'static str, String>,
}

#[derive(Debug, Clone, Serialize)]
struct Critical {
    reason: &'static str,
    action: LastStep,
    /// When the last step runs, Unix ms.
    at: i64,
    done: bool,
}

struct Awake {
    _guard: Box<dyn Guard>,
    reason: String,
    display: bool,
}

struct Inner {
    settings: PowerSettings,
    mode: ModeView,
    tasks_active: u64,
    /// Continuations waiting or running, and those of them for protected conversations.
    continuations: u64,
    protected_continuations: u64,
    readings: Readings,
    discharge: Discharge,
    thermal: Level,
    battery: Level,
    critical: Option<Critical>,
    awake: Option<Awake>,
    items: BTreeMap<String, Item>,
    /// Last wanted state of each administrator-layer change, so a refusal is not retried
    /// until what is wanted changes.
    elevated_tried: BTreeMap<&'static str, bool>,
    restart_pending: Option<bool>,
    system_requests: Option<Result<Vec<String>, String>>,
    last_actions: Vec<Value>,
    watch: Option<Box<dyn Guard>>,
    polls: u64,
    restore_wait: Option<i64>,
    restoring_from_away: bool,
    restore_wait_notified: bool,
}

impl Inner {
    fn new(settings: PowerSettings) -> Self {
        Inner {
            settings,
            mode: ModeView::default(),
            tasks_active: 0,
            continuations: 0,
            protected_continuations: 0,
            readings: Readings::default(),
            discharge: Discharge::default(),
            thermal: 0,
            battery: 0,
            critical: None,
            awake: None,
            items: BTreeMap::new(),
            elevated_tried: BTreeMap::new(),
            restart_pending: None,
            system_requests: None,
            last_actions: Vec::new(),
            watch: None,
            polls: 0,
            restore_wait: None,
            restoring_from_away: false,
            restore_wait_notified: false,
        }
    }

    fn away(&self) -> Option<&PresetView> {
        self.mode.settings.as_ref().filter(|_| self.mode.mode != "normal")
    }

    /// Protected work: a protected agent waiting to be continued counts like a running one.
    fn protected_work(&self) -> u64 {
        self.tasks_active + self.protected_continuations
    }

    /// The preset that decides the lid: the current one while away, else the one closing
    /// the lid would enter, as long as a task is protected.
    fn lid_preset(&self) -> Option<&PresetView> {
        self.away().or(if self.protected_work() > 0 {
            self.mode.default_settings.as_ref()
        } else {
            None
        })
    }

    fn below_battery_floor(&self) -> bool {
        let on_battery = self.readings.on_ac == Some(false);
        match (self.lid_preset(), self.readings.battery_percent) {
            (Some(p), Some(pct)) if on_battery && p.lid_on_battery.enabled => {
                pct < p.lid_on_battery.min_percent
            }
            _ => false,
        }
    }

    fn targets(&self) -> Targets {
        let mut t = Targets::default();
        let away = self.away();
        if away.is_some() || self.protected_work() > 0 {
            let reason = match (away.is_some(), self.protected_work()) {
                (true, 0) => "away-guard: away mode".to_owned(),
                (true, n) => format!("away-guard: away mode, {n} protected task(s)"),
                (false, n) => format!("away-guard: {n} protected task(s)"),
            };
            t.awake = Some((reason, away.is_some_and(|p| p.display == "on")));
        }
        if let Some(p) = self.lid_preset() {
            let floor_ok = self
                .readings
                .battery_percent
                .is_none_or(|pct| pct >= p.lid_on_battery.min_percent);
            let o = Override {
                ac: p.lid_on_ac.then_some(Change::Set(0)),
                dc: (p.lid_on_battery.enabled && floor_ok).then_some(Change::Set(0)),
            };
            if o.ac.is_some() || o.dc.is_some() {
                t.plan.insert(PowerSetting::LidAction, o);
            }
        }
        if self.thermal >= 2 || self.battery >= 2 {
            t.plan.insert(
                PowerSetting::CpuMaxPercent,
                Override::both(Change::Set(self.settings.cool_cpu_max_percent)),
            );
            t.power_mode = Some(PowerMode::EFFICIENCY);
        }
        if let Some(p) = away {
            if p.display == "dim" {
                let dim = Change::AtMost(self.settings.dim_brightness_percent);
                t.plan
                    .insert(PowerSetting::DisplayBrightness, Override::both(dim));
            }
            if p.defer_updates {
                t.plan.insert(
                    PowerSetting::SaverThreshold,
                    Override {
                        ac: None,
                        dc: Some(Change::Set(0)),
                    },
                );
                t.update_pause = true;
            }
            if p.no_power_saving {
                t.plan
                    .insert(PowerSetting::UsbSuspend, Override::both(Change::Set(0)));
                t.plan
                    .insert(PowerSetting::WifiPowerSaving, Override::both(Change::Set(0)));
                t.adapters_off = true;
            }
        }
        t
    }

    /// Brings the system in line with `targets`. Never fails: problems end up in `items`.
    fn reconcile(&mut self, ctx: &ModuleCtx, t: Targets) {
        if self.restoring_from_away && !ctx.arbitration().resource_frozen.is_empty() {
            let now = ctx.system().now_ms();
            let since = *self.restore_wait.get_or_insert(now);
            if now.saturating_sub(since) < 10_000 {
                self.note(
                    "restore_order",
                    Some(Item::applied("Waiting for resource thaw (at most 10 seconds)")),
                );
                return;
            }
            if !self.restore_wait_notified {
                self.restore_wait_notified = true;
                let message = "部分程序解冻失败，已等待 10 秒；现在恢复电源设置，程序解冻仍会重试。";
                let _ = ctx.system().notify_user("Arcturus 恢复", message);
                let _ = ctx.publish("notice", json!({"reason":"thaw_timeout","message":message}));
            }
        } else {
            self.restore_wait = None;
            self.restore_wait_notified = false;
        }
        self.restoring_from_away = false;
        self.note("restore_order", None);
        self.apply_awake(ctx, t.awake);
        for s in PowerSetting::ALL {
            self.apply_plan(ctx, s, t.plan.get(&s).copied());
        }
        self.apply_power_mode(ctx, t.power_mode);
        self.apply_update_pause(ctx, t.update_pause);
        self.apply_adapters(ctx, t.adapters_off);
    }

    fn sync(&mut self, ctx: &ModuleCtx) {
        let t = self.targets();
        self.reconcile(ctx, t);
    }

    fn note(&mut self, name: &str, item: Option<Item>) {
        match item {
            Some(i) => {
                self.items.insert(name.to_owned(), i);
            }
            None => {
                self.items.remove(name);
            }
        }
    }

    fn apply_awake(&mut self, ctx: &ModuleCtx, want: Option<(String, bool)>) {
        let Some((reason, display)) = want else {
            self.awake = None;
            self.note("keep_awake", None);
            return;
        };
        if self
            .awake
            .as_ref()
            .is_some_and(|a| a.reason == reason && a.display == display)
        {
            return;
        }
        match ctx.system().keep_awake(&reason, display) {
            // The new request exists before the old one is dropped: no gap.
            Ok(guard) => {
                self.awake = Some(Awake {
                    _guard: guard,
                    reason: reason.clone(),
                    display,
                });
                let what = if display { "system and display" } else { "system" };
                self.note(
                    "keep_awake",
                    Some(Item::applied(format!("{what} stay awake: {reason}"))),
                );
            }
            Err(e) => self.note("keep_awake", Some(Item::from_error(&e))),
        }
    }

    fn apply_plan(&mut self, ctx: &ModuleCtx, setting: PowerSetting, want: Option<Override>) {
        let name = setting.name();
        let sys = ctx.system();
        let open = match ctx.journal().open_entry(name) {
            Ok(e) => e,
            Err(e) => return self.note(name, Some(Item::from_error(&e))),
        };
        let Some(want) = want else {
            if let Some(entry) = open {
                let restored = serde_json::from_value::<PowerValue>(entry.original.clone())
                    .map_err(|e| ApiError::internal(format!("journal entry {}: {e}", entry.id)))
                    .and_then(|v| sys.set_power_setting(setting, &v))
                    .and_then(|()| ctx.journal().mark_restored(entry.id));
                return self.note(name, restored.err().map(|e| Item::from_error(&e)));
            }
            return self.note(name, None);
        };
        let original = match &open {
            Some(e) => match serde_json::from_value::<PowerValue>(e.original.clone()) {
                Ok(v) => v,
                Err(err) => {
                    return self.note(
                        name,
                        Some(Item {
                            state: "failed",
                            detail: err.to_string(),
                        }),
                    );
                }
            },
            None => match sys.power_setting(setting) {
                Ok(v) => v,
                Err(e) => return self.note(name, Some(Item::from_error(&e))),
            },
        };
        let wanted = PowerValue {
            plan: original.plan.clone(),
            ac: want.ac.map_or(original.ac, |c| c.apply(original.ac)),
            dc: want.dc.map_or(original.dc, |c| c.apply(original.dc)),
        };
        if open.is_none() && wanted == original {
            return self.note(name, None);
        }
        let applied = serde_json::to_value(&wanted).unwrap_or_default();
        if open.as_ref().is_none_or(|e| e.applied.as_ref() != Some(&applied)) {
            let original_json = serde_json::to_value(&original).unwrap_or_default();
            if let Err(e) = ctx.journal().record(name, &original_json, Some(&applied)) {
                return self.note(name, Some(Item::from_error(&e)));
            }
        }
        let detail = format!(
            "plugged in {} (was {}), on battery {} (was {})",
            wanted.ac, original.ac, wanted.dc, original.dc
        );
        if sys.power_setting(setting).ok().as_ref() == Some(&wanted) {
            return self.note(name, Some(Item::applied(detail)));
        }
        match sys.set_power_setting(setting, &wanted) {
            Ok(()) => self.note(name, Some(Item::applied(detail))),
            // The entry stays open: putting the original back is always safe.
            Err(e) => self.note(name, Some(Item::from_error(&e))),
        }
    }

    fn apply_power_mode(&mut self, ctx: &ModuleCtx, want: Option<&str>) {
        let sys = ctx.system();
        let open = match ctx.journal().open_entry(POWER_MODE_KEY) {
            Ok(e) => e,
            Err(e) => return self.note(POWER_MODE_KEY, Some(Item::from_error(&e))),
        };
        let Some(want) = want else {
            if let Some(entry) = open {
                let original = entry.original.as_str().unwrap_or(PowerMode::BALANCED).to_owned();
                let r = sys
                    .set_power_mode(&original)
                    .and_then(|()| ctx.journal().mark_restored(entry.id));
                return self.note(POWER_MODE_KEY, r.err().map(|e| Item::from_error(&e)));
            }
            return self.note(POWER_MODE_KEY, None);
        };
        let current = match sys.power_mode() {
            Ok(m) => m,
            Err(e) => return self.note(POWER_MODE_KEY, Some(Item::from_error(&e))),
        };
        if open.is_none() {
            if current == want {
                return self.note(POWER_MODE_KEY, None);
            }
            if let Err(e) = ctx
                .journal()
                .record(POWER_MODE_KEY, &json!(current), Some(&json!(want)))
            {
                return self.note(POWER_MODE_KEY, Some(Item::from_error(&e)));
            }
        }
        let was = open.map_or(current.clone(), |e| {
            e.original.as_str().unwrap_or_default().to_owned()
        });
        let result = if current == want {
            Ok(())
        } else {
            sys.set_power_mode(want)
        };
        self.note(
            POWER_MODE_KEY,
            Some(match result {
                Ok(()) => Item::applied(format!("{want} (was {was})")),
                Err(e) => Item::from_error(&e),
            }),
        );
    }

    /// Shared flow of the administrator-layer changes: read the original (refused under a
    /// user-mode backend, so nothing is journaled), journal it, change, and on the way back
    /// put it back. A refusal is remembered until what is wanted changes.
    fn apply_elevated(
        &mut self,
        ctx: &ModuleCtx,
        key: &'static str,
        want: bool,
        read: impl FnOnce() -> Result<Value, ApiError>,
        write: impl Fn(Option<&Value>, &Value) -> Result<(), ApiError>,
        describe: &str,
    ) {
        if self.elevated_tried.get(key) == Some(&want)
            && self.items.get(key).is_some_and(|i| i.state != "applied")
        {
            return;
        }
        self.elevated_tried.insert(key, want);
        let open = match ctx.journal().open_entry(key) {
            Ok(e) => e,
            Err(e) => return self.note(key, Some(Item::from_error(&e))),
        };
        if !want {
            if let Some(entry) = open {
                let r = write(None, &entry.original).and_then(|()| ctx.journal().mark_restored(entry.id));
                return self.note(key, r.err().map(|e| Item::from_error(&e)));
            }
            return self.note(key, None);
        }
        if open.is_some() {
            return self.note(key, Some(Item::applied(describe)));
        }
        let original = match read() {
            Ok(v) => v,
            Err(e) => return self.note(key, Some(Item::from_error(&e))),
        };
        let entry = match ctx.journal().record(key, &original, None) {
            Ok(e) => e,
            Err(e) => return self.note(key, Some(Item::from_error(&e))),
        };
        match write(Some(&original), &original) {
            Ok(()) => self.note(key, Some(Item::applied(describe))),
            Err(e) => {
                if is_needs_elevation(&e) {
                    // Refused before doing anything: nothing to put back.
                    let _ = ctx.journal().mark_restored(entry.id);
                }
                self.note(key, Some(Item::from_error(&e)));
            }
        }
    }

    fn apply_update_pause(&mut self, ctx: &ModuleCtx, want: bool) {
        let el = ctx.elevated();
        let until = now_ms() + i64::from(self.settings.update_pause_days) * 86_400_000;
        self.apply_elevated(
            ctx,
            UPDATE_PAUSE_KEY,
            want,
            || el.update_pause().map(|v| json!(v)),
            |apply, original| match apply {
                Some(_) => el.set_update_pause(Some(until)),
                None => el.set_update_pause(original.as_i64()),
            },
            "OS updates and their restarts paused while away",
        );
    }

    fn apply_adapters(&mut self, ctx: &ModuleCtx, want: bool) {
        let el = ctx.elevated();
        self.apply_elevated(
            ctx,
            ADAPTERS_KEY,
            want,
            || el.adapter_power_saving().map(|m| json!(m)),
            |apply, original| {
                let map: BTreeMap<String, bool> =
                    serde_json::from_value(original.clone()).unwrap_or_default();
                match apply {
                    Some(_) => el.set_adapter_power_saving(&map.keys().map(|k| (k.clone(), false)).collect()),
                    None => el.set_adapter_power_saving(&map),
                }
            },
            "network adapters may not be switched off to save power",
        );
    }

    fn record_action(&mut self, action: &str, result: &Result<(), ApiError>) {
        self.last_actions.push(json!({
            "action": action,
            "at": now_ms(),
            "ok": result.is_ok(),
            "error": result.as_ref().err().map(|e| e.message.clone()),
        }));
        let excess = self.last_actions.len().saturating_sub(8);
        self.last_actions.drain(..excess);
    }

    fn notices(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (name, item) in &self.items {
            if item.state != "applied" {
                out.push(format!("{name}: {}", item.detail));
            }
        }
        if let Some(c) = &self.critical {
            let left = ((c.at - now_ms()) / 1000).max(0);
            out.push(format!("{} critical: {:?} in {left} s", c.reason, c.action));
        }
        if self.restart_pending == Some(true) {
            out.push(
                "the OS is waiting for a restart to finish updates; away-guard does not restart it".into(),
            );
        }
        if !self.settings.thresholds_confirmed {
            out.push("temperature thresholds are placeholders until the lid test (task 2)".into());
        }
        for (name, e) in &self.readings.errors {
            out.push(format!("reading {name}: {e}"));
        }
        out
    }

    fn summary(&self) -> String {
        let r = &self.readings;
        let mut parts = Vec::new();
        match (r.battery_present, r.battery_percent) {
            (true, Some(p)) => {
                let src = match r.on_ac {
                    Some(true) if r.charging => "charging",
                    Some(true) => "plugged in",
                    Some(false) => "on battery",
                    None => "source unknown",
                };
                let left = r
                    .remaining_min
                    .or(r.os_remaining_min)
                    .map(|m| format!(", ~{m} min left"))
                    .unwrap_or_default();
                parts.push(format!("battery {p}% {src}{left}"));
            }
            _ => parts.push(if r.on_ac == Some(true) {
                "plugged in, no battery".into()
            } else {
                "no battery".into()
            }),
        }
        parts.push(match r.lid_closed {
            Some(true) => "lid closed".into(),
            Some(false) => "lid open".into(),
            None => "lid unknown".into(),
        });
        if let Some(t) = r.temp_c {
            parts.push(format!("{t:.1} °C ({})", LEVEL_NAMES[self.thermal as usize]));
        }
        if let Some(m) = &r.memory {
            parts.push(format!(
                "memory {:.1} of {:.1} GB free",
                m.available_mb as f64 / 1024.0,
                m.total_mb as f64 / 1024.0
            ));
        }
        if let Some(n) = &r.network {
            let level = serde_json::to_value(n.level)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            parts.push(format!(
                "network {level}{}",
                if n.metered { " (metered)" } else { "" }
            ));
        }
        if let Some(a) = &self.awake {
            parts.push(format!("keeping awake ({})", a.reason));
        }
        parts.join(", ")
    }
}

// ---- the module ----------------------------------------------------------------------------

enum Msg {
    System(SystemEvent),
    Stop,
}

pub struct Power {
    shared: Arc<Mutex<Inner>>,
    tx: Option<Sender<Msg>>,
    worker: Option<JoinHandle<()>>,
}

pub fn create() -> Box<dyn Module> {
    Box::new(Power {
        shared: Arc::new(Mutex::new(Inner::new(PowerSettings::default()))),
        tx: None,
        worker: None,
    })
}

fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Takes readings, updates the levels and acts on them. Slow OS queries run before the
/// lock is taken; the last step (sleep, hibernate) runs after it is released.
fn poll(shared: &Mutex<Inner>, ctx: &ModuleCtx) {
    let sys = ctx.system();
    let now = now_ms();
    let power = sys.power_status();
    let temps = sys.temperatures();
    let memory = sys.memory();
    let network = sys.connectivity();
    let (first, every_minute, period) = {
        let g = lock(shared);
        let per_minute = (60 / g.settings.poll_seconds.max(1)).max(1) as u64;
        (
            g.polls == 0,
            g.polls % per_minute == 0,
            g.away().is_some_and(|p| p.defer_updates),
        )
    };
    let caps = first.then(|| sys.power_capabilities());
    let restart = (every_minute && period).then(|| sys.restart_pending());
    let requests = every_minute.then(|| ctx.elevated().power_requests());

    let due = {
        let mut g = lock(shared);
        g.polls += 1;
        let r = &mut g.readings;
        r.at = now;
        let mut errors = BTreeMap::new();
        match power {
            Ok(p) => {
                r.on_ac = p.on_ac;
                r.battery_present = p.battery_present;
                r.battery_percent = p.battery_percent;
                r.charging = p.charging;
                r.saver_on = p.saver_on;
                r.os_remaining_min = p.os_remaining_s.map(|s| s / 60);
            }
            Err(e) => {
                errors.insert("power_status", e.message);
            }
        }
        match temps {
            Ok(z) => {
                r.temp_c = z.iter().map(|t| t.celsius).reduce(f64::max);
                r.zones = z;
            }
            Err(e) => {
                errors.insert("temperatures", e.message);
            }
        }
        match memory {
            Ok(m) => r.memory = Some(m),
            Err(e) => {
                errors.insert("memory", e.message);
            }
        }
        match network {
            Ok(n) => r.network = Some(n),
            Err(e) => {
                errors.insert("connectivity", e.message);
            }
        }
        if let Some(c) = caps {
            match c {
                Ok(c) => r.capabilities = Some(c),
                Err(e) => {
                    errors.insert("capabilities", e.message);
                }
            }
        }
        if let Some(watch_err) = r.errors.remove("watch") {
            errors.insert("watch", watch_err);
        }
        r.errors = errors;
        let (on_ac, pct) = (r.on_ac, r.battery_percent);
        g.discharge.add(now, on_ac, pct);
        g.readings.remaining_min = g.discharge.remaining_minutes();
        if let Some(Ok(pending)) = restart {
            g.restart_pending = Some(pending);
        } else if !period {
            g.restart_pending = None;
        }
        if let Some(req) = requests {
            g.system_requests = Some(req.map_err(|e| e.message));
        }
        evaluate(&mut g, ctx, now);
        let _ = ctx.publish(
            "reading",
            json!({
                "temp_c": g.readings.temp_c,
                "battery_percent": g.readings.battery_percent,
                "on_ac": g.readings.on_ac,
                "lid_closed": g.readings.lid_closed,
            }),
        );
        due_action(&mut g, now)
    };
    if let Some((how, reason)) = due {
        suspend(shared, ctx, how, reason);
    }
}

/// Works out the overheat and battery levels and what they call for.
fn evaluate(g: &mut Inner, ctx: &ModuleCtx, now: i64) {
    let lid_closed = g.readings.lid_closed == Some(true);
    if let Some(t) = g.readings.temp_c {
        let th = g.settings.temperature_now(lid_closed);
        let level = rising_level(g.thermal, t, th.as_array(), g.settings.temperature_hysteresis_c);
        if level != g.thermal {
            g.thermal = level;
            let _ = ctx.publish(
                "overheat",
                json!({ "level": level, "name": LEVEL_NAMES[level as usize], "temp_c": t, "lid_closed": lid_closed, "thresholds": th }),
            );
        }
    }
    let battery = match (g.readings.on_ac, g.readings.battery_percent) {
        (Some(false), Some(p)) => falling_level(
            g.battery,
            f64::from(p),
            g.settings.battery.as_array(),
            g.settings.battery_hysteresis,
        ),
        _ => 0,
    };
    if battery != g.battery {
        g.battery = battery;
        let _ = ctx.publish(
            "low_battery",
            json!({ "level": battery, "name": LEVEL_NAMES[battery as usize], "percent": g.readings.battery_percent }),
        );
    }
    let reason = if g.thermal >= 3 {
        Some("temperature")
    } else if g.battery >= 3 {
        Some("battery")
    } else if lid_closed && g.below_battery_floor() {
        Some("battery_floor")
    } else {
        None
    };
    match (reason, &g.critical) {
        (Some(reason), None) => {
            let action = g.settings.last_step;
            // Nobody is there to save their work while away: no countdown.
            let countdown = if g.away().is_some() {
                0
            } else {
                g.settings.last_step_countdown_s
            };
            g.critical = Some(Critical {
                reason,
                action,
                at: now + i64::from(countdown) * 1000,
                done: false,
            });
            dlog!("WARN", "power: {reason} critical, {action:?} in {countdown} s");
            let _ = ctx.publish(
                "critical",
                json!({ "state": "pending", "reason": reason, "action": action, "countdown_s": countdown }),
            );
        }
        (None, Some(c)) => {
            let _ = ctx.publish("critical", json!({ "state": "cancelled", "reason": c.reason }));
            g.critical = None;
        }
        (Some(_), Some(_)) if g.away().is_some() => {
            // Left during a countdown: nobody is there to wait for any more.
            if let Some(c) = g.critical.as_mut() {
                c.at = c.at.min(now);
            }
        }
        _ => {}
    }
    g.sync(ctx);
}

fn due_action(g: &mut Inner, now: i64) -> Option<(Suspend, &'static str)> {
    let c = g.critical.as_mut()?;
    if c.done || now < c.at {
        return None;
    }
    c.done = true;
    c.action.suspend().map(|s| (s, c.reason))
}

fn suspend(shared: &Mutex<Inner>, ctx: &ModuleCtx, how: Suspend, reason: &str) {
    dlog!("WARN", "power: {} because of {reason}", how.name());
    let _ = ctx.publish("suspending", json!({ "action": how, "reason": reason }));
    let result = ctx.system().suspend(how);
    if let Err(e) = &result {
        dlog!("ERROR", "power: {} failed: {}", how.name(), e.message);
    }
    lock(shared).record_action(how.name(), &result);
}

fn on_system_event(shared: &Mutex<Inner>, ctx: &ModuleCtx, e: SystemEvent) {
    let mut repoll = false;
    let mut due = None;
    {
        let mut g = lock(shared);
        match e {
            SystemEvent::Lid { closed, initial } => {
                g.readings.lid_closed = Some(closed);
                let _ = ctx.publish(
                    "lid",
                    json!({ "closed": closed, "initial": initial, "on_ac": g.readings.on_ac }),
                );
                let now = now_ms();
                evaluate(&mut g, ctx, now);
                due = due_action(&mut g, now);
            }
            SystemEvent::PowerSource { on_ac } => {
                g.readings.on_ac = Some(on_ac);
                let _ = ctx.publish("source", json!({ "on_ac": on_ac }));
                repoll = true;
            }
            SystemEvent::Session { locked } => {
                g.readings.locked = Some(locked);
                let _ = ctx.publish("session", json!({ "locked": locked }));
            }
            SystemEvent::Display { on } => g.readings.display_on = Some(on),
            SystemEvent::Suspending => dlog!("INFO", "power: the machine is going to sleep"),
            SystemEvent::Resumed => {
                dlog!("INFO", "power: the machine woke up");
                g.critical = None;
                repoll = true;
            }
        }
    }
    if let Some((how, reason)) = due {
        suspend(shared, ctx, how, reason);
    }
    if repoll {
        poll(shared, ctx);
    }
}

impl Power {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        lock(&self.shared)
    }

    fn stop_worker(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Stop);
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }

    fn on_mode(&mut self, ctx: &ModuleCtx, data: &Value) {
        let Ok(view) = serde_json::from_value::<ModeView>(data.clone()) else {
            return dlog!("WARN", "power: unreadable mode.changed event");
        };
        let mut g = lock(&self.shared);
        let entering = view.mode != "normal" && (g.mode.mode == "normal" || g.mode.preset != view.preset);
        g.restoring_from_away |= view.mode == "normal" && g.mode.mode != "normal";
        if view.mode != "normal" {
            g.restoring_from_away = false;
        }
        g.mode = view;
        // The screen is handled once, on the way in; everything else is reconciled.
        if entering && let Some(p) = g.mode.settings.clone() {
            if p.lock_screen {
                let r = ctx.system().lock_screen();
                g.record_action("lock_screen", &r);
            }
            if p.display == "off" {
                let r = ctx.system().display_off();
                g.record_action("display_off", &r);
            }
        }
        // Battery-floor and countdown rules depend on the mode too.
        let now = now_ms();
        evaluate(&mut g, ctx, now);
        let due = due_action(&mut g, now);
        drop(g);
        if let Some((how, reason)) = due {
            suspend(&self.shared, ctx, how, reason);
        }
    }

    fn on_tasks(&mut self, ctx: &ModuleCtx, active: u64) {
        self.on_work(ctx, |g| g.tasks_active = active);
    }

    fn on_continuations(&mut self, ctx: &ModuleCtx, data: &Value) {
        let active = data["active"].as_u64().unwrap_or(0);
        let protected = data["protected"].as_u64().unwrap_or(0);
        self.on_work(ctx, |g| {
            g.continuations = active;
            g.protected_continuations = protected;
        });
    }

    /// A continuation still to come means the work is not done, protected or not.
    fn on_work(&mut self, ctx: &ModuleCtx, change: impl FnOnce(&mut Inner)) {
        let hibernate = {
            let mut g = self.inner();
            let before = g.tasks_active + g.continuations;
            change(&mut g);
            g.sync(ctx);
            before > 0
                && g.tasks_active + g.continuations == 0
                && g.away().is_some()
                && g.settings.when_tasks_done == WhenTasksDone::Hibernate
        };
        if hibernate {
            suspend(&self.shared, ctx, Suspend::Hibernate, "all tasks done");
        }
    }
}

impl Drop for Power {
    fn drop(&mut self) {
        // Without a stop (crash, test teardown) the worker still ends; settings stay as they
        // are and the journal puts them back on the next start.
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Stop);
        }
    }
}

impl Module for Power {
    fn spec(&self) -> &'static ModuleSpec {
        &SPEC
    }

    fn start(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        let settings: PowerSettings = settings::load(ctx)?;
        self.shared = Arc::new(Mutex::new(Inner::new(settings)));
        let (tx, rx) = mpsc::channel::<Msg>();
        let sink_tx = tx.clone();
        match ctx.system().watch(Arc::new(move |e| {
            let _ = sink_tx.send(Msg::System(e));
        })) {
            Ok(guard) => self.inner().watch = Some(guard),
            Err(e) => {
                self.inner()
                    .readings
                    .errors
                    .insert("watch", format!("no lid or lock events: {}", e.message));
            }
        }
        let shared = self.shared.clone();
        let wctx = ctx.clone();
        let worker = std::thread::Builder::new()
            .name("power".into())
            .spawn(move || {
                if wctx.is_active() {
                    poll(&shared, &wctx);
                }
                loop {
                    if !wctx.is_active() {
                        break;
                    }
                    let wait = Duration::from_secs(u64::from(lock(&shared).settings.poll_seconds.max(1)));
                    match rx.recv_timeout(wait) {
                        Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                        Ok(Msg::System(e)) if wctx.is_active() => on_system_event(&shared, &wctx, e),
                        Ok(Msg::System(_)) => break,
                        Err(RecvTimeoutError::Timeout) if wctx.is_active() => poll(&shared, &wctx),
                        Err(RecvTimeoutError::Timeout) => break,
                    }
                }
            })
            .map_err(|e| ApiError::internal(format!("cannot start the power thread: {e}")))?;
        self.tx = Some(tx);
        self.worker = Some(worker);
        Ok(())
    }

    fn stop(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        self.stop_worker();
        let mut g = self.inner();
        g.watch = None;
        g.reconcile(ctx, Targets::default());
        let left: Vec<String> = g
            .items
            .iter()
            .map(|(k, i)| format!("{k}: {}", i.detail))
            .collect();
        if left.is_empty() {
            Ok(())
        } else {
            Err(ApiError::internal(format!(
                "could not put back: {}",
                left.join("; ")
            )))
        }
    }

    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        match req.cmd {
            "settings" => Ok(json!({ "settings": self.inner().settings })),
            "configure" => {
                let next: PowerSettings =
                    settings::patch(ctx, &req.args["settings"], &[], PowerSettings::check)?;
                let mut g = self.inner();
                g.settings = next;
                let now = now_ms();
                evaluate(&mut g, ctx, now);
                Ok(json!({ "settings": g.settings }))
            }
            "refresh" => {
                poll(&self.shared, ctx);
                Ok(json!({ "readings": self.inner().readings }))
            }
            other => Err(ApiError::internal(format!("power has no handler for '{other}'"))),
        }
    }

    fn status(&self, _ctx: &ModuleCtx) -> Value {
        let g = self.inner();
        let th = g.settings.temperature_now(g.readings.lid_closed == Some(true));
        json!({
            "implemented": true,
            "summary": g.summary(),
            "notices": g.notices(),
            "readings": g.readings,
            "levels": { "temperature": LEVEL_NAMES[g.thermal as usize], "battery": LEVEL_NAMES[g.battery as usize] },
            "thresholds": { "temperature_c": th, "battery_percent": g.settings.battery, "placeholder": !g.settings.thresholds_confirmed },
            "critical": g.critical,
            "mode": g.mode.mode,
            "tasks_active": g.tasks_active,
            "keep_awake": g.awake.as_ref().map(|a| json!({ "reason": a.reason, "display": a.display })),
            "changes": g.items,
            "last_actions": g.last_actions,
            "system_requests": match &g.system_requests {
                Some(Ok(list)) => json!(list),
                Some(Err(e)) => json!({ "error": e }),
                None => Value::Null,
            },
            "restart_pending": g.restart_pending,
        })
    }

    fn recover(&mut self, entry: &JournalEntry, ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        let bad = |e: serde_json::Error| ApiError::internal(format!("journal entry {}: {e}", entry.id));
        if let Some(setting) = PowerSetting::from_name(&entry.key) {
            let original: PowerValue = serde_json::from_value(entry.original.clone()).map_err(bad)?;
            ctx.system().set_power_setting(setting, &original)?;
        } else {
            match entry.key.as_str() {
                POWER_MODE_KEY => ctx
                    .system()
                    .set_power_mode(entry.original.as_str().unwrap_or(PowerMode::BALANCED))?,
                UPDATE_PAUSE_KEY => ctx.elevated().set_update_pause(entry.original.as_i64())?,
                ADAPTERS_KEY => {
                    let map: BTreeMap<String, bool> =
                        serde_json::from_value(entry.original.clone()).map_err(bad)?;
                    ctx.elevated().set_adapter_power_saving(&map)?;
                }
                other => {
                    return Err(ApiError::internal(format!(
                        "power does not know journal key '{other}'"
                    )));
                }
            }
        }
        Ok(RecoverOutcome::Restored)
    }

    fn on_event(&mut self, event: &Event, ctx: &ModuleCtx) -> Result<(), ApiError> {
        if event.topic == "resource.thawed" {
            self.inner().sync(ctx);
            return Ok(());
        }
        if event.topic == "mode.changed" {
            self.on_mode(ctx, &event.data);
        } else if event.topic == "resume.active" {
            self.on_continuations(ctx, &event.data);
        } else if let Some(active) = event.data.get("active").and_then(Value::as_u64) {
            self.on_tasks(ctx, active);
        }
        Ok(())
    }
}
