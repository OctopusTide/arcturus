//! The interface every daemon module implements, and the context it gets from the framework.
//!
//! A module is described by a static [`ModuleSpec`] (name, section, commands with argument
//! schemas, events, tables, error codes) plus a [`Module`] implementation. The framework does
//! everything generic from the spec: routing, the agent permission check, argument
//! validation, the CLI catalog, migrations, event delivery and fault isolation.
//! Nothing here is OS-specific; system calls go through [`ModuleCtx::system`] and
//! [`ModuleCtx::elevated`].

use std::sync::Arc;

use proto::{ApiError, ErrorCode};
use serde::Serialize;
use serde_json::Value;

pub use proto::catalog::{ArgKind, Section};

use crate::bus::Bus;
use crate::sqlite::{self, Connection, Row};
use crate::store::{JournalEntry, Store, now_ms};
use crate::sys::{Elevated, Sys, System};

/// One argument of a command. The framework validates requests against it and clients
/// build their command lines from it.
#[derive(Debug, Clone, Copy)]
pub struct ArgSpec {
    pub name: &'static str,
    pub kind: ArgKind,
    pub required: bool,
    /// Given by position on the command line instead of `--name`.
    pub positional: bool,
    pub help: &'static str,
}

impl ArgSpec {
    pub const fn new(name: &'static str, kind: ArgKind, help: &'static str) -> Self {
        ArgSpec {
            name,
            kind,
            required: false,
            positional: false,
            help,
        }
    }

    pub const fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub const fn positional(mut self) -> Self {
        self.positional = true;
        self
    }
}

/// One command. Its wire name is `<module>.<name>`.
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    /// Verb, `[a-z][a-z0-9_]*`.
    pub name: &'static str,
    /// Optional top-level short name (`protect`); unique across the daemon.
    pub short: Option<&'static str>,
    pub summary: &'static str,
    pub args: &'static [ArgSpec],
    /// Read-only commands are kept out of the operation log when they succeed.
    pub read_only: bool,
    /// Agents may call it. Off by default: a command is page/tray-only unless it says so.
    pub agent: bool,
    /// Not shown on command lines (for example the handshake).
    pub hidden: bool,
}

impl CommandSpec {
    pub const fn new(name: &'static str, summary: &'static str) -> Self {
        CommandSpec {
            name,
            short: None,
            summary,
            args: &[],
            read_only: false,
            agent: false,
            hidden: false,
        }
    }

    pub const fn short(mut self, short: &'static str) -> Self {
        self.short = Some(short);
        self
    }

    pub const fn args(mut self, args: &'static [ArgSpec]) -> Self {
        self.args = args;
        self
    }

    pub const fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Opens the command to agents (CLI). Without this only the page and tray may call it.
    pub const fn agent(mut self) -> Self {
        self.agent = true;
        self
    }

    pub const fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }
}

/// An event a module publishes. Its topic is `<module>.<name>`.
#[derive(Debug, Clone, Copy)]
pub struct EventSpec {
    pub name: &'static str,
    pub summary: &'static str,
}

impl EventSpec {
    pub const fn new(name: &'static str, summary: &'static str) -> Self {
        EventSpec { name, summary }
    }
}

/// One schema step for the module's own tables. Versions run 1, 2, 3 … in order; a released
/// step is never edited, later changes are new steps. Every table, index, view or trigger a
/// step creates must be named `<module>_…`.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: u32,
    pub sql: &'static str,
}

impl Migration {
    pub const fn new(version: u32, sql: &'static str) -> Self {
        Migration { version, sql }
    }
}

/// A module-specific error. On the wire: `code` = the framework code it maps to,
/// `subcode` = `<module>.<NAME>`.
#[derive(Debug, Clone, Copy)]
pub struct ErrorSpec {
    /// `[A-Z][A-Z0-9_]*`.
    pub name: &'static str,
    pub code: ErrorCode,
    pub summary: &'static str,
}

impl ErrorSpec {
    pub const fn new(name: &'static str, code: ErrorCode, summary: &'static str) -> Self {
        ErrorSpec {
            name,
            code,
            summary,
        }
    }
}

/// Static description of a module. Build it with the `const` methods so it can live in a
/// `static`.
#[derive(Debug, Clone, Copy)]
pub struct ModuleSpec {
    /// Stable id, `[a-z][a-z0-9_]*`. Stored in the database: never rename once released.
    pub name: &'static str,
    /// Which part of the product the module belongs to.
    pub section: Section,
    pub summary: &'static str,
    /// Whether it runs when the config file does not say.
    pub enabled_by_default: bool,
    pub commands: &'static [CommandSpec],
    pub publishes: &'static [EventSpec],
    /// Topics to receive: `"network.down"`, `"power.*"` or `"*"`.
    pub subscribes: &'static [&'static str],
    pub migrations: &'static [Migration],
    pub error_codes: &'static [ErrorSpec],
}

impl ModuleSpec {
    pub const fn new(name: &'static str, section: Section, summary: &'static str) -> Self {
        ModuleSpec {
            name,
            section,
            summary,
            enabled_by_default: true,
            commands: &[],
            publishes: &[],
            subscribes: &[],
            migrations: &[],
            error_codes: &[],
        }
    }

    pub const fn off_by_default(mut self) -> Self {
        self.enabled_by_default = false;
        self
    }

    pub const fn commands(mut self, commands: &'static [CommandSpec]) -> Self {
        self.commands = commands;
        self
    }

    pub const fn publishes(mut self, events: &'static [EventSpec]) -> Self {
        self.publishes = events;
        self
    }

    pub const fn subscribes(mut self, topics: &'static [&'static str]) -> Self {
        self.subscribes = topics;
        self
    }

    pub const fn migrations(mut self, migrations: &'static [Migration]) -> Self {
        self.migrations = migrations;
        self
    }

    pub const fn error_codes(mut self, codes: &'static [ErrorSpec]) -> Self {
        self.error_codes = codes;
        self
    }

    pub fn command(&self, verb: &str) -> Option<&'static CommandSpec> {
        self.commands.iter().find(|c| c.name == verb)
    }
}

/// How the framework classifies a caller, from the OS-reported executable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallerKind {
    /// Anything that is not one of our own page/tray executables (CLI, scripts, agents).
    #[default]
    Agent,
    /// Our page or tray: may call page-only commands.
    Trusted,
}

impl CallerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CallerKind::Agent => "agent",
            CallerKind::Trusted => "trusted",
        }
    }
}

/// Who sent a request. `pid` and `exe` come from the OS; `name` is what the client says
/// about itself and is not trusted.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Caller {
    pub pid: Option<u32>,
    pub exe: Option<String>,
    pub name: Option<String>,
    pub kind: CallerKind,
}

/// A request routed to a module, already checked for permission and argument types.
#[derive(Debug, Clone, Copy)]
pub struct ModuleRequest<'a> {
    pub id: &'a str,
    /// The verb (`protect`), as declared in [`CommandSpec::name`].
    pub cmd: &'a str,
    /// Always a JSON object matching the command's [`ArgSpec`]s.
    pub args: &'a Value,
    pub caller: &'a Caller,
}

/// Something that happened, delivered to subscribers through the event bus.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    /// `<module>.<name>`.
    pub topic: String,
    pub source: String,
    pub ts: i64,
    pub data: Value,
}

/// What `recover` did with a journal entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverOutcome {
    /// The original value is back in place; the entry is closed.
    Restored,
    /// The module deliberately keeps the change (for example away mode is persisted and
    /// still on) and takes it over again; the entry stays open.
    Adopted,
}

/// What the framework gives a module. Cheap to clone, so a module may keep one for its own
/// background threads.
#[derive(Clone)]
pub struct ModuleCtx {
    spec: &'static ModuleSpec,
    store: Arc<Store>,
    bus: Arc<Bus>,
    sys: Sys,
    configured_enabled: bool,
    active: Arc<std::sync::atomic::AtomicBool>,
}

fn store_err(e: sqlite::Error) -> ApiError {
    ApiError::internal(format!("store: {e}"))
}

impl ModuleCtx {
    /// Serialize actions against the latest task, mode and permission changes. Never
    /// acquire a module's own state lock while holding this guard.
    pub fn arbitration(&self) -> std::sync::MutexGuard<'_, crate::arbitration::Arbitration> {
        self.store
            .arbitration
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn new(
        spec: &'static ModuleSpec,
        store: Arc<Store>,
        bus: Arc<Bus>,
        sys: Sys,
    ) -> Self {
        ModuleCtx {
            spec,
            store,
            bus,
            sys,
            configured_enabled: spec.enabled_by_default,
            active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    pub(crate) fn with_enabled(mut self, enabled: bool) -> Self {
        self.configured_enabled = enabled;
        self
    }
    pub fn configured_enabled(&self) -> bool {
        self.configured_enabled
    }
    pub fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub(crate) fn deactivate(&self) {
        self.active
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn name(&self) -> &'static str {
        self.spec.name
    }

    /// This module's recovery journal. Use it before every system change.
    pub fn journal(&self) -> Journal<'_> {
        Journal {
            module: self.spec.name,
            store: &self.store,
        }
    }

    /// This module's key/value settings.
    pub fn settings(&self) -> Settings<'_> {
        Settings {
            scope: self.spec.name,
            store: &self.store,
        }
    }

    /// The database, for this module's own `<module>_*` tables.
    pub fn db(&self) -> ModuleDb<'_> {
        ModuleDb { store: &self.store }
    }

    /// Publishes `<module>.<event>`. The event must be declared in [`ModuleSpec::publishes`].
    /// Delivery is asynchronous, so this never blocks on subscribers.
    pub fn publish(&self, event: &str, data: Value) -> Result<(), ApiError> {
        if !self.spec.publishes.iter().any(|e| e.name == event) {
            return Err(ApiError::internal(format!(
                "module '{}' publishes undeclared event '{event}'",
                self.spec.name
            )));
        }
        self.bus.publish(Event {
            topic: format!("{}.{event}", self.spec.name),
            source: self.spec.name.to_owned(),
            ts: now_ms(),
            data,
        });
        Ok(())
    }

    /// Ordinary (non-admin) system calls.
    pub fn system(&self) -> &dyn System {
        self.sys.system.as_ref()
    }

    /// The narrow layer for operations that need administrator rights.
    pub fn elevated(&self) -> &dyn Elevated {
        self.sys.elevated.as_ref()
    }

    /// A module-specific error declared in [`ModuleSpec::error_codes`]: `code` from the
    /// declaration, `subcode` = `<module>.<name>`.
    pub fn error(&self, name: &str, message: impl Into<String>) -> ApiError {
        match self.spec.error_codes.iter().find(|e| e.name == name) {
            Some(spec) => ApiError::new(spec.code, message)
                .with_subcode(format!("{}.{name}", self.spec.name))
                .with_module(self.spec.name),
            None => ApiError::internal(format!(
                "module '{}' raised undeclared error '{name}'",
                self.spec.name
            ))
            .with_module(self.spec.name),
        }
    }
}

/// The recovery journal, scoped to one module. Protocol for any system change:
///
/// 1. `let entry = ctx.journal().record("key", &original, Some(&new))?;` — before touching anything;
/// 2. make the change;
/// 3. when undoing it normally, put `entry.original` back, then `ctx.journal().mark_restored(entry.id)?`.
///
/// Anything still open when the daemon starts is handed to [`Module::recover`].
pub struct Journal<'a> {
    module: &'static str,
    store: &'a Store,
}

impl Journal<'_> {
    /// Durably records `original` for `key` before a change. If this key already has an
    /// open entry, that entry (with the true original) is returned and only `applied` is
    /// updated, so changing the same setting twice never loses the original.
    pub fn record(
        &self,
        key: &str,
        original: &Value,
        applied: Option<&Value>,
    ) -> Result<JournalEntry, ApiError> {
        self.record_with_note(key, original, applied, None)
    }

    pub fn record_with_note(
        &self,
        key: &str,
        original: &Value,
        applied: Option<&Value>,
        note: Option<&str>,
    ) -> Result<JournalEntry, ApiError> {
        if key.is_empty() {
            return Err(ApiError::internal("journal key must not be empty"));
        }
        self.store
            .journal_record(self.module, key, original, applied, note)
            .map_err(store_err)
    }

    /// Closes an entry after the module has put the original value back itself.
    pub fn mark_restored(&self, entry_id: i64) -> Result<(), ApiError> {
        match self
            .store
            .journal_mark_restored(self.module, entry_id, "module")
            .map_err(store_err)?
        {
            true => Ok(()),
            false => Err(ApiError::new(
                ErrorCode::NotFound,
                format!(
                    "no open journal entry {entry_id} for module '{}'",
                    self.module
                ),
            )),
        }
    }

    /// This module's open entries, newest first.
    pub fn open_entries(&self) -> Result<Vec<JournalEntry>, ApiError> {
        self.store
            .journal_open(Some(self.module))
            .map_err(store_err)
    }

    /// The open entry for `key`, if any.
    pub fn open_entry(&self, key: &str) -> Result<Option<JournalEntry>, ApiError> {
        Ok(self.open_entries()?.into_iter().find(|e| e.key == key))
    }
}

/// Settings of one module (JSON values).
pub struct Settings<'a> {
    scope: &'a str,
    store: &'a Store,
}

impl Settings<'_> {
    pub fn get(&self, key: &str) -> Result<Option<Value>, ApiError> {
        self.store.setting_get(self.scope, key).map_err(store_err)
    }

    pub fn set(&self, key: &str, value: &Value) -> Result<(), ApiError> {
        self.store
            .setting_set(self.scope, key, value)
            .map_err(store_err)
    }
}

/// SQL access for a module's own tables. Do not call other `ctx` store helpers (journal,
/// settings) from inside [`ModuleDb::transaction`]: the connection is already held.
pub struct ModuleDb<'a> {
    store: &'a Store,
}

impl ModuleDb<'_> {
    pub fn execute(&self, sql: &str, params: &[sqlite::Value]) -> Result<usize, ApiError> {
        self.store
            .with_conn(|c| c.execute(sql, params))
            .map_err(store_err)
    }

    pub fn query(&self, sql: &str, params: &[sqlite::Value]) -> Result<Vec<Row>, ApiError> {
        self.store
            .with_conn(|c| c.query(sql, params))
            .map_err(store_err)
    }

    pub fn transaction<T>(
        &self,
        f: impl FnOnce(&Connection) -> sqlite::Result<T>,
    ) -> Result<T, ApiError> {
        self.store
            .with_conn(|c| c.transaction(f))
            .map_err(store_err)
    }
}

/// A daemon module. The registry calls every method under the module's own lock, so
/// `&mut self` is safe; keep `handle`, `status` and `on_event` quick and run long work
/// (polling, waiting tens of seconds) on the module's own threads.
///
/// Life cycle: tables migrated → `recover` (once per open journal entry, before start, even
/// when the module is switched off) → `start` (only if on) → `handle` / `status` /
/// `on_event` → `stop`. A panic in any method marks only this module as faulted.
pub trait Module: Send {
    fn spec(&self) -> &'static ModuleSpec;

    fn start(&mut self, _ctx: &ModuleCtx) -> Result<(), ApiError> {
        Ok(())
    }

    /// Should undo the module's live changes (and mark them restored). Whatever stays open
    /// is recovered on the next start.
    fn stop(&mut self, _ctx: &ModuleCtx) -> Result<(), ApiError> {
        Ok(())
    }

    /// Handles one of the module's commands. The result becomes the response's `result`.
    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError>;

    /// Module section of `core.status` (any JSON).
    fn status(&self, _ctx: &ModuleCtx) -> Value {
        Value::Object(Default::default())
    }

    /// Puts back `entry.original` for a change this module made in an earlier run that was
    /// never restored (crash, power loss, kill). Must be idempotent: the change may or may
    /// not have been applied before the crash. Returning `Err` keeps the entry open and it
    /// is retried on the next start.
    fn recover(
        &mut self,
        entry: &JournalEntry,
        ctx: &ModuleCtx,
    ) -> Result<RecoverOutcome, ApiError>;

    /// One event matching [`ModuleSpec::subscribes`], delivered on the module's own event
    /// thread in publish order.
    fn on_event(&mut self, _event: &Event, _ctx: &ModuleCtx) -> Result<(), ApiError> {
        Ok(())
    }
}
