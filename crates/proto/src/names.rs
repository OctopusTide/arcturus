//! Product-level names, so a rename touches this file plus the `[[bin]] name` lines in
//! `crates/cli/Cargo.toml` and `crates/daemon-main/Cargo.toml`. Crate names are deliberately
//! product-neutral. Where files and endpoints live on disk is platform convention and is
//! decided in one place per platform (`crates/platform/src/<os>/paths.rs`).

/// Product id. Used for the data folder and the endpoint name.
pub const PRODUCT: &str = "away-guard";
/// Human display name (provisional).
pub const DISPLAY_NAME: &str = "离开守护";
/// CLI executable name (without extension). Must match `[[bin]] name` in crates/cli.
pub const CLI_BIN: &str = "away-guard";
/// Daemon executable name. Must match `[[bin]] name` in crates/daemon-main.
pub const DAEMON_BIN: &str = "away-guard-daemon";
/// Narrow LocalSystem helper, installed only through an explicit user command.
pub const ADMIN_BIN: &str = "away-guard-admin";
pub const RELAY_BIN: &str = "away-guard-relay";
pub const ENV_ADMIN_DIR: &str = "AWAY_GUARD_ADMIN_DIR";
/// Starts the daemon without a window and again after a crash; what autostart runs.
/// Must match `[[bin]] name` in crates/daemon-main.
pub const LAUNCHER_BIN: &str = "away-guard-launcher";
/// Daemon exit code when another daemon already owns the endpoint.
pub const DAEMON_EXIT_ALREADY_RUNNING: u8 = 3;
/// Daemon log the launcher writes (inside the data folder).
pub const DAEMON_LOG_FILE: &str = "daemon.log";
/// Page (Tauri) executable name; trusted to call page-only commands.
pub const UI_BIN: &str = "away-guard-ui";
/// Tray executable name; trusted to call page-only commands.
pub const TRAY_BIN: &str = "away-guard-tray";
/// What agents' hooks run (installs our hooks too). Must match `[[bin]] name` in crates/agent-hook.
pub const HOOK_BIN: &str = "away-guard-hook";
/// SQLite file inside the data folder.
pub const DB_FILE: &str = "away-guard.db";
/// Configuration file inside the data folder.
pub const CONFIG_FILE: &str = "config.json";
/// Overrides the endpoint (pipe name / socket path), for development and tests.
pub const ENV_ENDPOINT: &str = "AWAY_GUARD_ENDPOINT";
/// Overrides the data folder, for development and tests.
pub const ENV_DATA_DIR: &str = "AWAY_GUARD_DATA_DIR";
