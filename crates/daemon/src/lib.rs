//! The portable daemon framework. Pages, tray and CLI never touch the system themselves; they
//! send requests and this framework hands them to modules.
//!
//! Nothing in this crate is OS-specific: the connection layer ([`transport`]), system calls
//! ([`sys`]) and the SQLite library ([`sqlite::SqliteLibrary`]) are traits implemented by
//! `crates/platform`.
//!
//! Layout:
//! - [`module`]: the [`Module`] trait, [`ModuleSpec`] and the context modules get
//! - [`registry`]: spec validation, routing, migrations, recovery, life cycle, fault isolation
//! - [`dispatch`]: sessions and handshake, permission and argument checks, `core.*` commands
//! - [`bus`]: events between modules
//! - [`access`]: caller classification (agent vs. page/tray)
//! - [`store`] / [`sqlite`]: database, per-component migrations, recovery journal
//! - [`server`] / [`transport`]: startup sequence and the connection loop
//! - [`modules`]: built-in modules, one directory each

pub mod access;
pub mod args;
pub mod arbitration;
pub mod bus;
pub mod config;
pub mod dispatch;
pub mod log;
pub mod module;
pub mod modules;
pub mod registry;
pub mod server;
pub mod sqlite;
pub mod store;
pub mod sys;
pub mod transport;

pub use access::TrustPolicy;
pub use module::{
    ArgKind, ArgSpec, Caller, CallerKind, CommandSpec, ErrorSpec, Event, EventSpec, Journal, Migration, Module,
    ModuleCtx, ModuleRequest, ModuleSpec, RecoverOutcome, Section, Settings,
};
pub use server::{BackgroundDaemon, DaemonConfig, DaemonError, run, spawn, start};
pub use store::JournalEntry;
