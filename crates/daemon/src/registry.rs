//! Holds the modules: validates their specs, routes commands, runs their migrations,
//! recovery and life cycle, delivers their events, and isolates their faults.
//!
//! Fault isolation: every call into a module runs under that module's own lock inside
//! `catch_unwind`. A panic (or a failed start or migration) marks only that module
//! `faulted`: its commands answer `MODULE_FAULTED`, it gets no more events, `core.status`
//! shows why, and `core.module_faulted` is published. The daemon and the other modules carry
//! on. A faulted module stays off until the daemon restarts, but startup recovery still
//! calls its `recover` (a module whose migration failed must still put settings back).

use std::collections::{BTreeSet, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use proto::catalog::{ArgInfo, CommandInfo, ModuleInfo};
use proto::commands::module::CORE;
use proto::{ApiError, ErrorCode, subcode};
use serde::Serialize;
use serde_json::{Value, json};

use crate::bus::{Bus, valid_pattern};
use crate::config::Config;
use crate::dlog;
use crate::module::{
    CallerKind, CommandSpec, Event, Module, ModuleCtx, ModuleRequest, ModuleSpec, RecoverOutcome,
};
use crate::store::{Store, now_ms};
use crate::sys::Sys;

/// How long `core.status` waits for a busy module before reporting it as busy.
const STATUS_LOCK_WAIT: Duration = Duration::from_millis(200);

/// What startup recovery did.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RecoveryReport {
    pub restored: u32,
    pub adopted: u32,
    pub failed: u32,
    /// Entries whose module is not registered (renamed or removed).
    pub orphaned: u32,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Health {
    /// Switched off by config or default.
    Disabled,
    /// Not started because the config file is broken (see [`Registry::config_error`]).
    ConfigError,
    /// Registered, not started yet.
    Ready,
    Running,
    Stopped,
    Faulted(String),
}

impl Health {
    fn name(&self) -> &'static str {
        match self {
            Health::Disabled => "disabled",
            Health::ConfigError => "config_error",
            Health::Ready => "ready",
            Health::Running => "running",
            Health::Stopped => "stopped",
            Health::Faulted(_) => "faulted",
        }
    }
}

pub struct Slot {
    spec: &'static ModuleSpec,
    enabled: bool,
    health: Mutex<Health>,
    was_started: Mutex<bool>,
    module: Mutex<Box<dyn Module>>,
    ctx: ModuleCtx,
    bus: Arc<Bus>,
}

fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

impl Slot {
    pub fn spec(&self) -> &'static ModuleSpec {
        self.spec
    }

    fn health(&self) -> Health {
        self.health
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn set_health(&self, h: Health) {
        *self.health.lock().unwrap_or_else(|p| p.into_inner()) = h;
    }

    fn lock(&self) -> MutexGuard<'_, Box<dyn Module>> {
        self.module.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Marks the module faulted (first reason wins) and tells everyone.
    fn fault(&self, reason: String) {
        {
            let mut h = self.health.lock().unwrap_or_else(|p| p.into_inner());
            if matches!(*h, Health::Faulted(_)) {
                return;
            }
            *h = Health::Faulted(reason.clone());
            self.ctx.deactivate();
        }
        dlog!("ERROR", "module {} faulted: {reason}", self.spec.name);
        self.bus.publish(Event {
            topic: format!("{CORE}.module_faulted"),
            source: CORE.into(),
            ts: now_ms(),
            data: json!({ "module": self.spec.name, "reason": reason }),
        });
    }

    /// Runs `f` on the module under its lock. A panic faults the module.
    fn guarded<T>(
        &self,
        what: &str,
        f: impl FnOnce(&mut dyn Module, &ModuleCtx) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let mut module = self.lock();
        match catch_unwind(AssertUnwindSafe(|| f(module.as_mut(), &self.ctx))) {
            Ok(r) => r.map_err(|e| self.checked_error(e)),
            Err(panic) => {
                drop(module);
                let reason = format!("panicked in {what}: {}", panic_text(panic.as_ref()));
                self.fault(reason.clone());
                Err(ApiError::new(
                    ErrorCode::ModuleFaulted,
                    format!("module '{}' {reason}", self.spec.name),
                )
                .with_module(self.spec.name))
            }
        }
    }

    /// Tags the error with the module and enforces the subcode namespace: a module may only
    /// use its own declared subcodes or framework (`core.`) ones.
    fn checked_error(&self, mut e: ApiError) -> ApiError {
        if e.module.is_none() {
            e.module = Some(self.spec.name.into());
        }
        if let Some(sub) = &e.subcode {
            let own = sub
                .strip_prefix(self.spec.name)
                .and_then(|s| s.strip_prefix('.'))
                .is_some_and(|n| self.spec.error_codes.iter().any(|d| d.name == n));
            if !own && !sub.starts_with("core.") {
                return ApiError::internal(format!(
                    "module '{}' returned undeclared subcode '{sub}'",
                    self.spec.name
                ))
                .with_module(self.spec.name)
                .with_details(json!({ "original": e }));
            }
        }
        e
    }

    /// Delivers one event if the module is running.
    fn deliver(&self, event: &Event) {
        if self.health() != Health::Running {
            return;
        }
        if let Err(e) = self.guarded("on_event", |m, ctx| m.on_event(event, ctx)) {
            if e.code != ErrorCode::ModuleFaulted {
                dlog!(
                    "WARN",
                    "module {} failed on event {}: {}",
                    self.spec.name,
                    event.topic,
                    e.message
                );
            }
        }
    }

    /// `status()` without waiting long for a busy module.
    fn status_value(&self) -> Value {
        let deadline = Instant::now() + STATUS_LOCK_WAIT;
        loop {
            match self.module.try_lock() {
                Ok(m) => {
                    return match catch_unwind(AssertUnwindSafe(|| m.status(&self.ctx))) {
                        Ok(v) => v,
                        Err(p) => {
                            drop(m);
                            let reason = format!("panicked in status: {}", panic_text(p.as_ref()));
                            self.fault(reason.clone());
                            json!({ "error": reason })
                        }
                    };
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return json!({ "error": "lock poisoned" });
                }
                Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(std::sync::TryLockError::WouldBlock) => return json!({ "busy": true }),
            }
        }
    }
}

/// Argument names taken by the CLI's global options.
const RESERVED_ARG_NAMES: [&str; 5] = ["json", "endpoint", "timeout", "help", "version"];

fn valid_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
        && s.len() <= 32
}

fn valid_error_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('A'..='Z'))
        && chars.all(|c| matches!(c, 'A'..='Z' | '0'..='9' | '_'))
}

/// Checks one spec on its own. Names that clients use as words must be identifiers, and
/// `help` is reserved by command lines.
pub fn validate_spec(spec: &ModuleSpec) -> Result<(), String> {
    let m = spec.name;
    if !valid_ident(m) || m == "help" {
        return Err(format!("invalid module name '{m}'"));
    }
    let mut verbs = BTreeSet::new();
    for c in spec.commands {
        if !valid_ident(c.name) || !verbs.insert(c.name) {
            return Err(format!("{m}: invalid or duplicate command '{}'", c.name));
        }
        if let Some(s) = c.short {
            if !valid_ident(s) || s == "help" {
                return Err(format!("{m}.{}: invalid short name '{s}'", c.name));
            }
        }
        let mut names = BTreeSet::new();
        for a in c.args {
            if !valid_ident(a.name) || !names.insert(a.name) || RESERVED_ARG_NAMES.contains(&a.name)
            {
                return Err(format!(
                    "{m}.{}: invalid or duplicate argument '{}'",
                    c.name, a.name
                ));
            }
            if a.positional && matches!(a.kind, proto::ArgKind::Bool | proto::ArgKind::StrList) {
                return Err(format!(
                    "{m}.{}: argument '{}' cannot be positional",
                    c.name, a.name
                ));
            }
        }
    }
    let mut events = BTreeSet::new();
    for e in spec.publishes {
        if !valid_ident(e.name) || !events.insert(e.name) {
            return Err(format!("{m}: invalid or duplicate event '{}'", e.name));
        }
    }
    if let Some(p) = spec.subscribes.iter().find(|p| !valid_pattern(p)) {
        return Err(format!("{m}: invalid subscription '{p}'"));
    }
    for (i, mig) in spec.migrations.iter().enumerate() {
        if mig.version as usize != i + 1 {
            return Err(format!(
                "{m}: migrations must be numbered 1, 2, 3 … in order"
            ));
        }
    }
    let mut codes = BTreeSet::new();
    for e in spec.error_codes {
        if !valid_error_name(e.name) || !codes.insert(e.name) {
            return Err(format!("{m}: invalid or duplicate error code '{}'", e.name));
        }
    }
    Ok(())
}

/// Where a command goes.
#[derive(Clone, Copy)]
pub struct Resolved<'a> {
    /// `None` for the daemon's own commands.
    pub slot: Option<&'a Arc<Slot>>,
    pub module: &'static ModuleSpec,
    pub command: &'static CommandSpec,
}

impl Resolved<'_> {
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.module.name, self.command.name)
    }
}

pub struct Registry {
    core: &'static ModuleSpec,
    slots: Vec<Arc<Slot>>,
    /// Canonical and short names → (slot index or `None` for core, command).
    routes: HashMap<String, (Option<usize>, &'static CommandSpec)>,
    store: Arc<Store>,
    bus: Arc<Bus>,
    config_warnings: Vec<String>,
    config_error: Option<String>,
}

impl Registry {
    /// Registers `modules` in order after validating every spec and checking that module
    /// names, command names and short names are unique. Subscriptions start here.
    ///
    /// `config` is the loaded config file, or why it could not be read. A broken file holds
    /// every module (none starts, not even with default switches), but migrations and
    /// recovery still run.
    pub fn new(
        core: &'static ModuleSpec,
        modules: Vec<Box<dyn Module>>,
        store: Arc<Store>,
        bus: Arc<Bus>,
        sys: Sys,
        config: Result<Config, String>,
    ) -> Result<Registry, String> {
        let (config, config_error) = match config {
            Ok(c) => (c, None),
            Err(e) => (Config::default(), Some(e)),
        };
        let mut slots: Vec<Arc<Slot>> = Vec::new();
        let mut routes: HashMap<String, (Option<usize>, &'static CommandSpec)> = HashMap::new();
        let mut add_routes =
            |spec: &'static ModuleSpec, idx: Option<usize>| -> Result<(), String> {
                for c in spec.commands {
                    let full = format!("{}.{}", spec.name, c.name);
                    for name in std::iter::once(full.clone()).chain(c.short.map(str::to_owned)) {
                        if routes.insert(name.clone(), (idx, c)).is_some() {
                            return Err(format!("command name '{name}' ({full}) is used twice"));
                        }
                    }
                }
                Ok(())
            };
        add_routes(core, None)?;
        for module in modules {
            let spec = module.spec();
            validate_spec(spec)?;
            if spec.name == core.name || slots.iter().any(|s| s.spec.name == spec.name) {
                return Err(format!("module '{}' registered twice", spec.name));
            }
            add_routes(spec, Some(slots.len()))?;
            let enabled = config_error.is_none()
                && config
                    .module_enabled(spec.name)
                    .unwrap_or(spec.enabled_by_default);
            let health = match (&config_error, enabled) {
                (Some(_), _) => Health::ConfigError,
                (None, true) => Health::Ready,
                (None, false) => Health::Disabled,
            };
            slots.push(Arc::new(Slot {
                spec,
                enabled,
                health: Mutex::new(health),
                was_started: Mutex::new(false),
                ctx: ModuleCtx::new(spec, store.clone(), bus.clone(), sys.clone())
                    .with_enabled(enabled || config_error.is_some()),
                module: Mutex::new(module),
                bus: bus.clone(),
            }));
        }
        // Short names share the command line with module names.
        for (name, (idx, c)) in &routes {
            if c.short == Some(name.as_str()) && slots.iter().any(|s| s.spec.name == name) {
                let owner = idx.map_or(core.name, |i| slots[i].spec.name);
                return Err(format!(
                    "short name '{name}' of {owner}.{} collides with a module name",
                    c.name
                ));
            }
        }
        for slot in slots.iter().filter(|s| !s.spec.subscribes.is_empty()) {
            let target = slot.clone();
            bus.subscribe(slot.spec.name, slot.spec.subscribes, move |e| {
                target.deliver(e)
            });
        }
        let config_warnings = config
            .modules
            .keys()
            .filter(|name| !slots.iter().any(|s| s.spec.name == name.as_str()))
            .map(|name| format!("config.json mentions unknown module '{name}'"))
            .collect();
        Ok(Registry {
            core,
            slots,
            routes,
            store,
            bus,
            config_warnings,
            config_error,
        })
    }

    pub fn slots(&self) -> &[Arc<Slot>] {
        &self.slots
    }

    pub fn bus(&self) -> &Arc<Bus> {
        &self.bus
    }

    pub fn config_warnings(&self) -> &[String] {
        &self.config_warnings
    }

    /// Why the config file could not be used; `Some` means no module was started.
    pub fn config_error(&self) -> Option<&str> {
        self.config_error.as_deref()
    }

    /// Brings every module's tables up to date, switched off or not. A module whose
    /// migration fails is faulted; the others are unaffected.
    pub fn migrate_all(&self) {
        for slot in &self.slots {
            if let Err(e) = self.store.migrate(slot.spec.name, slot.spec.migrations) {
                slot.fault(format!("database migration failed: {}", e.message));
            }
        }
    }

    /// Hands every open journal entry, newest first, to the module that wrote it. Runs for
    /// switched-off, held (broken config) and faulted modules too: putting settings back
    /// matters more than any of that. A failure or panic is recorded on the entry and
    /// affects only that module. Only entries whose module is not registered are skipped.
    pub fn recover(&self) -> RecoveryReport {
        let mut report = RecoveryReport::default();
        let entries = match self.store.journal_open(None) {
            Ok(e) => e,
            Err(e) => {
                report
                    .errors
                    .push(format!("cannot read recovery journal: {e}"));
                return report;
            }
        };
        for entry in entries {
            let label = format!("entry {} ({}/{})", entry.id, entry.module, entry.key);
            let Some(slot) = self.slots.iter().find(|s| s.spec.name == entry.module) else {
                let msg = format!("no module named '{}' is registered", entry.module);
                let _ = self.store.journal_record_failure(entry.id, &msg);
                report.orphaned += 1;
                report.errors.push(format!("{label}: {msg}"));
                continue;
            };
            match slot.guarded("recover", |m, ctx| m.recover(&entry, ctx)) {
                Ok(RecoverOutcome::Restored) => {
                    match self
                        .store
                        .journal_mark_restored(slot.spec.name, entry.id, "startup")
                    {
                        Ok(_) => report.restored += 1,
                        Err(e) => {
                            report.failed += 1;
                            report
                                .errors
                                .push(format!("{label}: restored but not marked: {e}"));
                        }
                    }
                }
                Ok(RecoverOutcome::Adopted) => report.adopted += 1,
                Err(e) => {
                    let _ = self.store.journal_record_failure(entry.id, &e.message);
                    report.failed += 1;
                    report.errors.push(format!("{label}: {}", e.message));
                }
            }
        }
        report
    }

    /// Starts every switched-on, healthy module in registration order. A module whose start
    /// fails is faulted; the others keep going.
    pub fn start_all(&self) {
        for slot in &self.slots {
            if slot.health() != Health::Ready {
                continue;
            }
            *slot.was_started.lock().unwrap_or_else(|p| p.into_inner()) = true;
            match slot.guarded("start", |m, ctx| m.start(ctx)) {
                Ok(()) => slot.set_health(Health::Running),
                Err(e) if e.code == ErrorCode::ModuleFaulted => {}
                Err(e) => slot.fault(format!("failed to start: {}", e.message)),
            }
        }
    }

    /// Stops modules in reverse order. Faulted modules that had started get a best-effort
    /// `stop` too, since it may put settings back.
    pub fn stop_all(&self) {
        for slot in self.slots.iter().rev() {
            let started = *slot.was_started.lock().unwrap_or_else(|p| p.into_inner());
            match slot.health() {
                Health::Running => {
                    if let Err(e) = slot.guarded("stop", |m, ctx| m.stop(ctx)) {
                        dlog!(
                            "ERROR",
                            "module {} failed to stop cleanly: {}",
                            slot.spec.name,
                            e.message
                        );
                    }
                    if slot.health() == Health::Running {
                        slot.set_health(Health::Stopped);
                    }
                }
                Health::Faulted(_) if started => {
                    let _ = slot.guarded("stop", |m, ctx| m.stop(ctx));
                }
                _ => {}
            }
        }
    }

    /// Finds a command by canonical (`task.protect`) or short (`protect`) name.
    pub fn resolve(&self, cmd: &str) -> Option<Resolved<'_>> {
        self.routes.get(cmd).map(|&(idx, command)| match idx {
            Some(i) => Resolved {
                slot: Some(&self.slots[i]),
                module: self.slots[i].spec,
                command,
            },
            None => Resolved {
                slot: None,
                module: self.core,
                command,
            },
        })
    }

    /// Calls the module behind a resolved command.
    pub fn call(&self, slot: &Slot, req: &ModuleRequest<'_>) -> Result<Value, ApiError> {
        let name = slot.spec.name;
        match slot.health() {
            Health::Running => slot.guarded("handle", |m, ctx| m.handle(req, ctx)),
            Health::Disabled => Err(ApiError::new(
                ErrorCode::ModuleDisabled,
                format!("module '{name}' is switched off (config.json: modules.{name}.enabled)"),
            )
            .with_module(name)),
            Health::ConfigError => Err(ApiError::new(
                ErrorCode::ModuleDisabled,
                format!(
                    "module '{name}' was not started because the config file is broken: {}; fix it and restart the daemon",
                    self.config_error.as_deref().unwrap_or("?")
                ),
            )
            .with_subcode(subcode::CONFIG_INVALID)
            .with_module(name)),
            Health::Faulted(why) => {
                Err(ApiError::new(ErrorCode::ModuleFaulted, format!("module '{name}' is faulted: {why}")).with_module(name))
            }
            other => Err(ApiError::internal(format!("module '{name}' is {}", other.name())).with_module(name)),
        }
    }

    /// Per-module section of `core.status`.
    pub fn status(&self) -> Vec<Value> {
        self.slots
            .iter()
            .map(|slot| {
                let health = slot.health();
                let fault = match &health {
                    Health::Faulted(why) => Some(why.clone()),
                    _ => None,
                };
                json!({
                    "name": slot.spec.name,
                    "section": slot.spec.section,
                    "enabled": slot.enabled,
                    "health": health.name(),
                    "fault": fault,
                    "schema_version": self.store.schema_version(slot.spec.name).ok(),
                    "commands": slot.spec.commands.iter().map(|c| format!("{}.{}", slot.spec.name, c.name)).collect::<Vec<_>>(),
                    "state": slot.status_value(),
                })
            })
            .collect()
    }

    /// The handshake catalog, with `callable` worked out for `kind`.
    pub fn catalog(&self, kind: CallerKind) -> (Vec<ModuleInfo>, Vec<CommandInfo>) {
        let mut modules = Vec::new();
        let mut commands = Vec::new();
        let core_entry = (self.core, true, "running".to_owned());
        let entries = std::iter::once(core_entry).chain(
            self.slots
                .iter()
                .map(|s| (s.spec, s.enabled, s.health().name().to_owned())),
        );
        for (spec, enabled, health) in entries {
            modules.push(ModuleInfo {
                name: spec.name.into(),
                section: Some(spec.section),
                summary: spec.summary.into(),
                enabled,
                health,
                error_codes: spec
                    .error_codes
                    .iter()
                    .map(|e| format!("{}.{}", spec.name, e.name))
                    .collect(),
                events: spec
                    .publishes
                    .iter()
                    .map(|e| format!("{}.{}", spec.name, e.name))
                    .collect(),
            });
            for c in spec.commands {
                commands.push(CommandInfo {
                    name: format!("{}.{}", spec.name, c.name),
                    module: spec.name.into(),
                    verb: c.name.into(),
                    section: Some(spec.section),
                    short: c.short.map(Into::into),
                    summary: c.summary.into(),
                    args: c
                        .args
                        .iter()
                        .map(|a| ArgInfo {
                            name: a.name.into(),
                            kind: a.kind,
                            required: a.required,
                            positional: a.positional,
                            help: a.help.into(),
                        })
                        .collect(),
                    read_only: c.read_only,
                    agent: c.agent,
                    callable: c.agent || kind == CallerKind::Trusted,
                    hidden: c.hidden,
                    module_enabled: enabled,
                });
            }
        }
        (modules, commands)
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        // Ends the delivery threads, which hold references to the slots.
        self.bus.close();
    }
}
