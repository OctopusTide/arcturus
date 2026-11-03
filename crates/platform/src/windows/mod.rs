//! Windows: named pipes, winsqlite3.dll, Win32 system calls, `%LOCALAPPDATA%`.

pub mod admin;
pub mod autostart;
mod events;
pub mod exit_firewall;
mod ffi;
mod network_watch;
pub mod paths;
mod pipe;
mod power;
mod process;
mod resource;
mod resume;
mod resume_broker;
mod wlan;
pub use resume_broker::{agent_runs_in_a_window, create_resume_broker, run_resume_broker};
mod sensors;
mod sqlite;
mod system;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use daemon::sys::Sys;
use daemon::{DaemonConfig, TrustPolicy};
use proto::client::Client;
use proto::{ApiError, ClientInfo};

pub use ffi::{format_local_time, pipe_exists};
pub use pipe::{PipeListener, PipeTransport, connect};
pub use sqlite::Winsqlite3;
pub use system::{UserModeElevated, WindowsSystem};

/// The real system layer.
pub fn sys() -> Sys {
    Sys {
        system: Arc::new(WindowsSystem),
        elevated: Arc::new(admin::ServiceElevated),
    }
}

/// Everything the daemon needs on this machine.
pub fn daemon_config(endpoint: String, data_dir: PathBuf) -> Result<DaemonConfig, String> {
    Ok(DaemonConfig {
        config_file: paths::config_file(&data_dir),
        endpoint,
        data_dir,
        sys: sys(),
        sqlite: Winsqlite3::load()?,
        trust: TrustPolicy::beside_daemon(),
    })
}

/// Connects to the daemon and runs the handshake.
pub fn connect_client(endpoint: &str, info: ClientInfo, wait: Duration) -> Result<Client, ApiError> {
    Client::handshake(connect(endpoint, wait)?, info)
}

/// Daemon log timestamps in local time.
pub fn init_logging() {
    daemon::log::set_time_format(format_local_time);
}

/// Resolves on Ctrl+C, Ctrl+Break, the console window closing, or the session ending
/// (logoff, shutdown, restart). After the daemon has stopped, call [`stopped`].
pub async fn stop_signal() {
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close};
    let end = Arc::new(tokio::sync::Notify::new());
    let notify = end.clone();
    if let Err(e) = events::on_session_end(move || notify.notify_one()) {
        daemon::dlog!(
            "WARN",
            "no logoff/shutdown notification ({e}); settings are put back on the next start"
        );
    }
    match (ctrl_c(), ctrl_break(), ctrl_close()) {
        (Ok(mut c), Ok(mut b), Ok(mut close)) => {
            tokio::select! {
                _ = c.recv() => {}
                _ = b.recv() => {}
                _ = close.recv() => {}
                _ = end.notified() => daemon::dlog!("INFO", "the session is ending"),
            }
        }
        _ => {
            daemon::dlog!(
                "WARN",
                "no console handlers; the daemon stops at logoff or shutdown"
            );
            end.notified().await;
        }
    }
}

/// The daemon has fully stopped: a logoff or shutdown waiting for it may go ahead.
pub fn stopped() {
    events::session_end_done();
}
