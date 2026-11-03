use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use daemon::dlog;
use proto::names;

/// Background process. Runs in the foreground of this console; stop it with Ctrl+C.
/// For a windowless daemon that is started again after a crash, run the launcher instead.
#[derive(Parser)]
#[command(name = names::DAEMON_BIN, version)]
struct Args {
    /// Endpoint to listen on (default: \\.\pipe\away-guard-<your user SID>)
    #[arg(long, env = names::ENV_ENDPOINT)]
    endpoint: Option<String>,
    /// Data folder holding the database and config.json (default: %LOCALAPPDATA%\away-guard)
    #[arg(long, env = names::ENV_DATA_DIR)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start at logon and again after a crash (a Task Scheduler task for this user)
    Autostart {
        #[command(subcommand)]
        action: Autostart,
    },
}

#[derive(Subcommand)]
enum Autostart {
    /// Register the task that runs the launcher next to this executable
    Install,
    /// Remove it
    Remove,
    /// Show what is registered
    Show,
}

fn autostart(action: Autostart) -> ExitCode {
    let result = match action {
        Autostart::Install => launcher_path().and_then(|l| platform::autostart::install(&l)).map(|i| match i {
            platform::autostart::Installed::Task(name) => {
                format!("installed scheduled task {name}: starts at logon and again if it stops")
            }
            platform::autostart::Installed::RunKey(why) => format!(
                "the scheduled task was refused ({why}); using the Run key instead: starts at logon and the \
                 launcher restarts the daemon, but nothing restarts the launcher itself"
            ),
        }),
        Autostart::Remove => platform::autostart::remove().map(|removed| {
            if removed.is_empty() { "nothing was installed".to_owned() } else { format!("removed {}", removed.join(", ")) }
        }),
        Autostart::Show => platform::autostart::show(),
    };
    match result {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn launcher_path() -> Result<PathBuf, String> {
    let me = std::env::current_exe().map_err(|e| format!("cannot find this executable: {e}"))?;
    let launcher = me.with_file_name(format!("{}{}", names::LAUNCHER_BIN, std::env::consts::EXE_SUFFIX));
    if launcher.exists() {
        Ok(launcher)
    } else {
        Err(format!("{} is missing; build or install it next to {}", launcher.display(), me.display()))
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    if let Some(Cmd::Autostart { action }) = args.command {
        return autostart(action);
    }
    platform::init_logging();
    let endpoint = match args.endpoint.map_or_else(platform::paths::default_endpoint, Ok) {
        Ok(p) => p,
        Err(e) => {
            dlog!("ERROR", "cannot work out the endpoint: {e}");
            return ExitCode::FAILURE;
        }
    };
    let data_dir = match args.data_dir.map_or_else(platform::paths::default_data_dir, Ok) {
        Ok(d) => d,
        Err(e) => {
            dlog!("ERROR", "cannot work out the data folder: {e}");
            return ExitCode::FAILURE;
        }
    };
    let config = match platform::daemon_config(endpoint, data_dir) {
        Ok(c) => c,
        Err(e) => {
            dlog!("ERROR", "{e}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            dlog!("ERROR", "cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    dlog!("INFO", "{} {} starting (pid {})", names::DAEMON_BIN, env!("CARGO_PKG_VERSION"), std::process::id());
    let shutdown = async {
        platform::stop_signal().await;
        dlog!("INFO", "stopping");
    };
    match runtime.block_on(daemon::run(platform::PipeTransport, config, daemon::modules::builtin(), shutdown)) {
        Ok(()) => {
            dlog!("INFO", "stopped");
            platform::stopped();
            ExitCode::SUCCESS
        }
        Err(e @ daemon::DaemonError::AlreadyRunning(_)) => {
            dlog!("ERROR", "{e}");
            ExitCode::from(names::DAEMON_EXIT_ALREADY_RUNNING)
        }
        Err(e) => {
            dlog!("ERROR", "{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn binary_name_matches_names_table() {
        assert_eq!(env!("CARGO_BIN_NAME"), proto::names::DAEMON_BIN);
    }
}
