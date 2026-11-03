//! 模式 (mode): the master switch. Normal (someone is at the machine), away (fixed
//! recommended settings) or custom (a named preset, initially the same as away). Owns `away`
//! and `back`, the presets, and the away report.
//!
//! It never calls another module and is never called by one: every change is published as
//! `mode.changed` carrying the full effective settings, and power, network, resource … each
//! act on that event by themselves. It republishes the current mode once at startup
//! (on `core.started`), so every module learns it without asking.
//!
//! Inputs it listens to: `power.lid` (closing the lid with a protected task counts as
//! leaving), `power.session` (unlocking counts as coming back when the preset locks the
//! screen), and the `active` count every `task.*` event carries (all tasks done → back to
//! normal). Everything seen while away also feeds the away report ([`report`]).

mod report;

use std::collections::BTreeMap;

use proto::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::dlog;
use crate::module::{
    ArgKind, ArgSpec, CallerKind, CommandSpec, ErrorSpec, Event, EventSpec, Migration, Module, ModuleCtx,
    ModuleRequest, ModuleSpec, RecoverOutcome, Section,
};
use crate::modules::settings;
use crate::store::{JournalEntry, now_ms};
use report::Draft;

static SPEC: ModuleSpec = ModuleSpec::new("mode", Section::Usage, "Normal, away and custom mode")
    .commands(&[
        CommandSpec::new("away", "Enter away mode, or a custom preset").short("away").agent().args(&[
            ArgSpec::new("preset", ArgKind::Str, "away, custom, … (default: the configured one)"),
            ArgSpec::new("trigger", ArgKind::Str, "button or idle (page and tray only)"),
        ]),
        CommandSpec::new("back", "Leave away mode").short("back").agent(),
        CommandSpec::new("show", "Current mode, presets and settings").agent().read_only(),
        CommandSpec::new("configure", "Change mode settings or a custom preset").args(&[ArgSpec::new(
            "settings",
            ArgKind::Json,
            "Partial settings object, e.g. {\"presets\":{\"custom\":{\"display\":\"dim\"}}}",
        )
        .required()]),
        CommandSpec::new("reports", "Away reports, newest first").agent().read_only().args(&[ArgSpec::new(
            "limit",
            ArgKind::Uint,
            "How many (default 5)",
        )]),
    ])
    .publishes(&[
        EventSpec::new("changed", "The mode or its effective settings changed"),
        EventSpec::new("report", "An away report is ready"),
    ])
    .subscribes(&["*"])
    .migrations(&[Migration::new(
        1,
        "CREATE TABLE mode_reports (
            id         INTEGER PRIMARY KEY,
            started_at INTEGER NOT NULL,
            ended_at   INTEGER NOT NULL,
            preset     TEXT    NOT NULL,
            report     TEXT    NOT NULL,
            seen       INTEGER NOT NULL DEFAULT 0
        );",
    )])
    .error_codes(&[
        ErrorSpec::new("UNKNOWN_PRESET", ErrorCode::InvalidArgs, "No preset with that name"),
        ErrorSpec::new("TRIGGER_NOT_ALLOWED", ErrorCode::Denied, "Only the page and tray report button or idle"),
        ErrorSpec::new("BAD_TRIGGER", ErrorCode::InvalidArgs, "Trigger must be cli, button or idle"),
        ErrorSpec::new("IDLE_ENTRY_OFF", ErrorCode::Conflict, "Entering away mode on idle is switched off"),
    ]);

/// The built-in preset. Fixed; custom presets start as a copy.
pub const AWAY: &str = "away";
/// The first custom preset, always present.
pub const CUSTOM: &str = "custom";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Display {
    Off,
    Dim,
    On,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LidOnBattery {
    pub enabled: bool,
    /// Below this battery level the machine stops running with the lid closed.
    pub min_percent: u8,
}

impl Default for LidOnBattery {
    fn default() -> Self {
        LidOnBattery { enabled: false, min_percent: 30 }
    }
}

/// The nine things away mode does that affect programs in use, cost money or change system
/// settings. Away mode uses [`Preset::default`]; custom presets may change each one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preset {
    /// 锁屏
    pub lock_screen: bool,
    /// 屏幕: off, dim or on
    pub display: Display,
    /// 内存不够时冻结、强杀别的程序
    pub free_memory: bool,
    /// 断网后自动切到手机热点
    pub hotspot_failover: bool,
    /// 推迟系统更新这类会打断运行的东西
    pub defer_updates: bool,
    /// 插着电时合盖继续跑
    pub lid_on_ac: bool,
    /// 用电池时合盖继续跑（要设电量下限）
    pub lid_on_battery: LidOnBattery,
    /// 关掉网卡和 USB 口的省电
    pub no_power_saving: bool,
    /// 离开报告
    pub away_report: bool,
}

impl Default for Preset {
    fn default() -> Self {
        Preset {
            lock_screen: true,
            display: Display::Off,
            free_memory: true,
            hotspot_failover: true,
            defer_updates: true,
            lid_on_ac: true,
            lid_on_battery: LidOnBattery::default(),
            no_power_saving: true,
            away_report: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModeSettings {
    /// Preset used when nothing says which: lid close, idle, `away` without `--preset`.
    pub default_preset: String,
    /// Minutes without keyboard or mouse after which the tray may enter away mode; `None` =
    /// off. The tray measures idleness and calls `away --trigger idle`.
    pub idle_enter_minutes: Option<u32>,
    /// Custom presets by name. `away` is built in and not listed.
    pub presets: BTreeMap<String, Preset>,
}

impl Default for ModeSettings {
    fn default() -> Self {
        ModeSettings {
            default_preset: AWAY.into(),
            idle_enter_minutes: None,
            presets: BTreeMap::from([(CUSTOM.into(), Preset::default())]),
        }
    }
}

impl ModeSettings {
    fn preset(&self, name: &str) -> Option<Preset> {
        match name {
            AWAY => Some(Preset::default()),
            other => self.presets.get(other).cloned(),
        }
    }

    fn check(&self) -> Result<(), String> {
        if self.presets.contains_key(AWAY) {
            return Err("'away' is built in and cannot be changed".into());
        }
        if !self.presets.contains_key(CUSTOM) {
            return Err("the 'custom' preset cannot be removed".into());
        }
        if let Some(bad) = self.presets.keys().find(|n| !valid_name(n)) {
            return Err(format!("preset name '{bad}' must be [a-z][a-z0-9_]*"));
        }
        if self.preset(&self.default_preset).is_none() {
            return Err(format!("default_preset '{}' does not exist", self.default_preset));
        }
        if let Some(p) = self.presets.values().find(|p| p.lid_on_battery.min_percent > 100) {
            return Err(format!("lid_on_battery.min_percent {} is over 100", p.lid_on_battery.min_percent));
        }
        if self.idle_enter_minutes == Some(0) {
            return Err("idle_enter_minutes must be at least 1 (or null for off)".into());
        }
        Ok(())
    }
}

fn valid_name(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some('a'..='z')) && c.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_')) && s.len() <= 32
}

/// Which mode a preset puts the machine in.
fn kind(preset: Option<&str>) -> &'static str {
    match preset {
        None => "normal",
        Some(AWAY) => "away",
        Some(_) => "custom",
    }
}

/// The current mode, persisted so a daemon restart (crash) during an absence resumes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct ModeState {
    /// `None` = normal mode.
    preset: Option<String>,
    trigger: String,
    since: i64,
    /// Entered by button or CLI: keep running even without a protected task.
    keep_running: bool,
    /// When the machine booted, to tell a daemon restart from a reboot.
    boot_at: Option<i64>,
    /// Every protected task has ended, but continuations are still to come.
    tasks_done_waiting: bool,
}

impl Default for ModeState {
    fn default() -> Self {
        ModeState { preset: None, trigger: "startup".into(), since: now_ms(), keep_running: false, boot_at: None, tasks_done_waiting: false }
    }
}

const STATE_KEY: &str = "state";
const DRAFT_KEY: &str = "report_draft";
/// Two boot times further apart than this are two different boots.
const BOOT_TOLERANCE_MS: i64 = 120_000;

#[derive(Default)]
pub struct ModeModule {
    state: ModeState,
    settings: ModeSettings,
    tasks_active: u64,
    /// Continuations waiting or running, and those of them for protected conversations.
    continuations: u64,
    protected_continuations: u64,
    draft: Option<Draft>,
}

pub fn create() -> Box<dyn Module> {
    Box::new(ModeModule::default())
}

fn boot_at(ctx: &ModuleCtx) -> Option<i64> {
    ctx.system().uptime_ms().ok().map(|up| now_ms() - up as i64)
}

impl ModeModule {
    fn kind(&self) -> &'static str {
        kind(self.state.preset.as_deref())
    }

    fn current_preset(&self) -> Option<Preset> {
        self.state.preset.as_deref().and_then(|p| self.settings.preset(p))
    }

    fn save_state(&self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        ctx.settings().set(STATE_KEY, &serde_json::to_value(&self.state).unwrap_or_default())
    }

    fn save_draft(&self, ctx: &ModuleCtx) {
        let v = self.draft.as_ref().map_or(Value::Null, |d| serde_json::to_value(d).unwrap_or_default());
        if let Err(e) = ctx.settings().set(DRAFT_KEY, &v) {
            dlog!("WARN", "mode: cannot save the away report draft: {}", e.message);
        }
    }

    /// What `mode.changed` carries: everything a module needs to act without asking.
    fn changed_payload(&self, previous: &str) -> Value {
        let default_settings = self.settings.preset(&self.settings.default_preset).unwrap_or_default();
        json!({
            "mode": self.kind(),
            "preset": self.state.preset,
            "settings": self.current_preset(),
            "trigger": self.state.trigger,
            "since": self.state.since,
            "keep_running": self.state.keep_running,
            "previous": previous,
            "default_preset": self.settings.default_preset,
            "default_settings": default_settings,
        })
    }

    fn publish_changed(&self, ctx: &ModuleCtx, previous: &str) -> Result<(), ApiError> {
        {
            let mut a=ctx.arbitration();
            a.automatic = self.current_preset().is_some_and(|p| p.free_memory);
            a.hotspot = self.current_preset().is_some_and(|p| p.hotspot_failover);
        }
        ctx.publish("changed", self.changed_payload(previous))
    }

    fn enter(&mut self, ctx: &ModuleCtx, preset: &str, trigger: &str, keep_running: bool) -> Result<Value, ApiError> {
        let Some(p) = self.settings.preset(preset) else {
            return Err(ctx.error("UNKNOWN_PRESET", format!("no preset named '{preset}'")));
        };
        let previous = self.kind();
        let same = self.state.preset.as_deref() == Some(preset);
        if same && (self.state.keep_running || !keep_running) {
            return Ok(json!({ "changed": false, "mode": previous, "preset": preset }));
        }
        let now = now_ms();
        if self.state.preset.is_none() {
            self.draft = p.away_report.then(|| Draft::new(now, preset, trigger));
            self.save_draft(ctx);
        }
        self.state = ModeState {
            preset: Some(preset.into()),
            trigger: trigger.into(),
            since: if same { self.state.since } else { now },
            keep_running: keep_running || (same && self.state.keep_running),
            boot_at: boot_at(ctx),
            tasks_done_waiting: same && self.state.tasks_done_waiting,
        };
        self.save_state(ctx)?;
        self.publish_changed(ctx, previous)?;
        dlog!("INFO", "mode: {} (preset {preset}) via {trigger}", self.kind());
        Ok(json!({ "changed": true, "mode": self.kind(), "preset": preset, "previous": previous }))
    }

    fn leave(&mut self, ctx: &ModuleCtx, trigger: &str) -> Result<Value, ApiError> {
        if self.state.preset.is_none() {
            return Ok(json!({ "changed": false, "mode": "normal" }));
        }
        let previous = self.kind();
        let now = now_ms();
        let report = self.finish_report(ctx, now, trigger);
        self.state = ModeState { preset: None, trigger: trigger.into(), since: now, keep_running: false, boot_at: None, tasks_done_waiting: false };
        self.save_state(ctx)?;
        self.publish_changed(ctx, previous)?;
        dlog!("INFO", "mode: normal again via {trigger}");
        Ok(json!({ "changed": true, "mode": "normal", "previous": previous, "report": report }))
    }

    /// Stores and announces the report of the absence that just ended.
    fn finish_report(&mut self, ctx: &ModuleCtx, now: i64, left_by: &str) -> Option<Value> {
        let draft = self.draft.take()?;
        self.save_draft(ctx);
        let report = draft.finish(now, left_by);
        let stored = ctx.db().execute(
            "INSERT INTO mode_reports (started_at, ended_at, preset, report) VALUES (?, ?, ?, ?)",
            &[draft.started_at.into(), now.into(), draft.preset.clone().into(), report.to_string().into()],
        );
        if let Err(e) = stored {
            dlog!("WARN", "mode: cannot store the away report: {}", e.message);
        }
        let _ = ctx.publish("report", report.clone());
        Some(report)
    }

    fn away_cmd(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        let trusted = req.caller.kind == CallerKind::Trusted;
        let trigger = match req.args["trigger"].as_str() {
            None => if trusted { "button" } else { "cli" },
            Some(t @ ("button" | "idle")) if trusted => t,
            Some("cli") => "cli",
            Some("button" | "idle") => {
                return Err(ctx.error("TRIGGER_NOT_ALLOWED", "only the page and tray report button presses and idleness"));
            }
            Some(other) => return Err(ctx.error("BAD_TRIGGER", format!("unknown trigger '{other}'"))),
        };
        let preset = req.args["preset"].as_str().unwrap_or(&self.settings.default_preset).to_owned();
        if trigger == "idle" {
            if self.settings.idle_enter_minutes.is_none() {
                return Err(ctx.error("IDLE_ENTRY_OFF", "idle_enter_minutes is not set"));
            }
            if self.tasks_active == 0 && self.protected_continuations == 0 {
                return Ok(json!({ "changed": false, "mode": self.kind(), "reason": "no protected task" }));
            }
        }
        // Button and CLI mean "I want the machine to keep running", tasks or not.
        self.enter(ctx, &preset, trigger, trigger != "idle")
    }

    fn configure(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        let next: ModeSettings = settings::patch(ctx, &req.args["settings"], &["presets"], ModeSettings::check)?;
        let before = (self.changed_payload(""), self.settings.clone());
        self.settings = next;
        // A preset that no longer exists ends the absence that uses it.
        if self.state.preset.as_deref().is_some_and(|p| self.settings.preset(p).is_none()) {
            self.leave(ctx, "settings")?;
        } else if self.changed_payload("") != before.0 {
            let previous = self.kind();
            self.state.trigger = "settings".into();
            self.save_state(ctx)?;
            self.publish_changed(ctx, previous)?;
        }
        Ok(json!({ "settings": self.settings }))
    }

    fn reports(&self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        let limit = req.args["limit"].as_u64().unwrap_or(5).clamp(1, 100) as u32;
        let rows = ctx.db().query("SELECT id, report, seen FROM mode_reports ORDER BY id DESC LIMIT ?", &[limit.into()])?;
        let items: Vec<Value> = rows
            .iter()
            .map(|r| {
                let mut report: Value = r.text(1).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
                report["id"] = json!(r.int(0).ok());
                report["seen"] = json!(r.int(2).ok() == Some(1));
                report
            })
            .collect();
        Ok(json!({ "reports": items, "in_progress": self.draft.as_ref().map(|d| d.finish(now_ms(), "")) }))
    }

    fn summary(&self) -> String {
        let mut s = match &self.state.preset {
            None => "normal (someone is at the machine)".to_owned(),
            Some(p) => format!(
                "{} (preset {p}) for {}, entered by {}{}",
                self.kind(),
                fmt_minutes(now_ms() - self.state.since),
                self.state.trigger,
                if self.state.keep_running { ", keeps running without tasks" } else { "" }
            ),
        };
        s.push_str(&format!("; {} protected task(s)", self.tasks_active));
        s
    }
}

fn fmt_minutes(ms: i64) -> String {
    let m = ms.max(0) / 60_000;
    format!("{}h {:02}m", m / 60, m % 60)
}

impl Module for ModeModule {
    fn spec(&self) -> &'static ModuleSpec {
        &SPEC
    }

    fn start(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        self.settings = settings::load(ctx)?;
        let stored: Option<ModeState> =
            ctx.settings().get(STATE_KEY)?.and_then(|v| serde_json::from_value(v).ok());
        self.draft = ctx.settings().get(DRAFT_KEY)?.and_then(|v| serde_json::from_value(v).ok());
        let Some(stored) = stored else {
            return Ok(());
        };
        let same_boot = match (stored.boot_at, boot_at(ctx)) {
            (Some(a), Some(b)) => (a - b).abs() < BOOT_TOLERANCE_MS,
            _ => false,
        };
        let usable = stored.preset.as_deref().is_some_and(|p| self.settings.preset(p).is_some());
        if usable && same_boot {
            // The daemon restarted (crash) during an absence: carry on with it.
            dlog!("INFO", "mode: resuming {} after a daemon restart", kind(stored.preset.as_deref()));
            self.state = stored;
        } else if stored.preset.is_some() {
            // A reboot or logoff means someone is back at the machine.
            self.state.trigger = "reboot".into();
            let now = now_ms();
            self.finish_report(ctx, now, "reboot");
            self.save_state(ctx)?;
        }
        Ok(())
    }

    fn stop(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        // A deliberate stop (console, logoff, shutdown) means someone is at the machine;
        // only a crash keeps an absence going. Other modules are already stopped, so
        // nothing is published.
        if self.state.preset.is_some() {
            let now = now_ms();
            self.finish_report(ctx, now, "daemon_stopped");
            self.state = ModeState { preset: None, trigger: "daemon_stopped".into(), since: now, keep_running: false, boot_at: None, tasks_done_waiting: false };
            self.save_state(ctx)?;
        }
        Ok(())
    }

    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        match req.cmd {
            "away" => self.away_cmd(req, ctx),
            "back" => {
                let trigger = if req.caller.kind == CallerKind::Trusted { "button" } else { "cli" };
                self.leave(ctx, trigger)
            }
            "show" => {
                let mut presets = self.settings.presets.clone();
                presets.insert(AWAY.into(), Preset::default());
                Ok(json!({
                    "mode": self.kind(),
                    "state": self.state,
                    "tasks_active": self.tasks_active,
                    "continuations": self.continuations,
                    "settings": self.settings,
                    "presets": presets,
                    "effective": self.current_preset(),
                }))
            }
            "configure" => self.configure(req, ctx),
            "reports" => self.reports(req, ctx),
            other => Err(ApiError::internal(format!("mode has no handler for '{other}'"))),
        }
    }

    fn status(&self, _ctx: &ModuleCtx) -> Value {
        json!({
            "implemented": true,
            "mode": self.kind(),
            "preset": self.state.preset,
            "since": self.state.since,
            "trigger": self.state.trigger,
            "keep_running": self.state.keep_running,
            "tasks_active": self.tasks_active,
            "default_preset": self.settings.default_preset,
            "summary": self.summary(),
        })
    }

    fn recover(&mut self, _entry: &JournalEntry, _ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        // Changes no system setting itself, so it never writes journal entries.
        Ok(RecoverOutcome::Restored)
    }

    fn on_event(&mut self, event: &Event, ctx: &ModuleCtx) -> Result<(), ApiError> {
        if event.source == SPEC.name {
            return Ok(());
        }
        // A worker may finish just after `back` won the module lock. Keep the stored
        // report complete when its check timestamp belongs to the absence that ended.
        if self.state.preset.is_none() && matches!(event.topic.as_str(),"exit_guard.violation"|"exit_guard.acted") {
            let at=event.data["at"].as_i64().unwrap_or(event.ts);
            let rows=ctx.db().query("SELECT id, report FROM mode_reports WHERE started_at <= ? AND ended_at >= ? ORDER BY id DESC LIMIT 1",&[at.into(),at.into()])?;
            if let Some(row)=rows.first(){
                let mut value:Value=serde_json::from_str(&row.text(1).map_err(|e|ApiError::internal(e.to_string()))?).map_err(|e|ApiError::internal(e.to_string()))?;
                let mut items=value["exit_guard"].as_array().cloned().unwrap_or_default();report::upsert_exit(&mut items,&event.data);value["exit_guard"]=json!(items);
                ctx.db().execute("UPDATE mode_reports SET report = ?, seen = 0 WHERE id = ?",&[value.to_string().into(),row.int(0).map_err(|e|ApiError::internal(e.to_string()))?.into()])?;
                ctx.publish("report",value)?;
            }
        }
        if self.state.preset.is_some()
            && let Some(draft) = self.draft.as_mut()
            && draft.absorb(event)
        {
            self.save_draft(ctx);
        }
        if event.topic == "resume.active" {
            self.continuations = event.data["active"].as_u64().unwrap_or(0);
            self.protected_continuations = event.data["protected"].as_u64().unwrap_or(0);
            if self.continuations == 0 && self.state.tasks_done_waiting && self.tasks_active == 0 {
                self.leave(ctx, "tasks_done")?;
            }
            return Ok(());
        }
        if let Some(active) = event.data.get("active").and_then(Value::as_u64)
            && event.topic.starts_with("task.")
        {
            let before = self.tasks_active;
            self.tasks_active = active;
            if active > 0 && self.state.tasks_done_waiting {
                self.state.tasks_done_waiting = false;
                self.save_state(ctx)?;
            }
            if before > 0 && active == 0 && self.state.preset.is_some() {
                // A failed agent waiting to be continued is not done: its work is still to come.
                if self.continuations > 0 {
                    self.state.tasks_done_waiting = true;
                    self.save_state(ctx)?;
                } else {
                    self.leave(ctx, "tasks_done")?;
                }
            }
            return Ok(());
        }
        match event.topic.as_str() {
            "core.started" => self.publish_changed(ctx, self.kind())?,
            "power.lid" => {
                let closed = event.data["closed"].as_bool() == Some(true);
                let initial = event.data["initial"].as_bool() == Some(true);
                if closed
                    && !initial
                    && self.state.preset.is_none()
                    && self.tasks_active + self.protected_continuations > 0
                {
                    let preset = self.settings.default_preset.clone();
                    self.enter(ctx, &preset, "lid", false)?;
                }
            }
            "power.session" => {
                let unlocked = event.data["locked"].as_bool() == Some(false);
                if unlocked && self.current_preset().is_some_and(|p| p.lock_screen) {
                    self.leave(ctx, "unlock")?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
