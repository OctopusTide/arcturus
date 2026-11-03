#![cfg_attr(windows, windows_subsystem = "windows")]
//! Starts the daemon without a console window, its output appended to
//! `<data folder>\daemon.log`, and starts it again when it stops unexpectedly. This is what
//! autostart runs (`away-guard-daemon autostart install`).
//!
//! - The daemon stopped on its own (exit 0: logoff, shutdown): the launcher stops too.
//! - Another daemon already owns the endpoint: wait until it is gone, then take over.
//! - Anything else is a crash: start again after 2, 5, 10, 30, then every 60 seconds; a run
//!   of ten minutes or more counts as healthy and starts the sequence over.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use proto::names;

/// Starts the daemon in the background and again after a crash.
#[derive(Parser)]
#[command(name = names::LAUNCHER_BIN, version)]
struct Args {
    /// Passed on to the daemon
    #[arg(long, env = names::ENV_ENDPOINT)]
    endpoint: Option<String>,
    /// Passed on to the daemon; the log goes here too
    #[arg(long, env = names::ENV_DATA_DIR)]
    data_dir: Option<PathBuf>,
}

const DELAYS_S: [u64; 5] = [2, 5, 10, 30, 60];
const HEALTHY_AFTER: Duration = Duration::from_secs(600);
const OTHER_DAEMON_POLL: Duration = Duration::from_secs(5);
/// Above this the log is moved to `daemon.log.old` before the next start.
const LOG_LIMIT: u64 = 10 * 1024 * 1024;

#[derive(Debug, PartialEq)]
enum After {
    Stop,
    WaitForOther,
    Restart(Duration),
}

#[derive(Default)]
struct Policy {
    failures: usize,
}

impl Policy {
    /// What to do once the daemon has exited with `code` (None: killed) after `ran_for`.
    fn after(&mut self, code: Option<i32>, ran_for: Duration) -> After {
        match code {
            Some(0) => After::Stop,
            Some(c) if c == i32::from(names::DAEMON_EXIT_ALREADY_RUNNING) => {
                self.failures = 0;
                After::WaitForOther
            }
            _ => {
                if ran_for >= HEALTHY_AFTER {
                    self.failures = 0;
                }
                let delay = DELAYS_S[self.failures.min(DELAYS_S.len() - 1)];
                self.failures += 1;
                After::Restart(Duration::from_secs(delay))
            }
        }
    }
}

fn open_log(data_dir: &Path) -> std::io::Result<File> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join(names::DAEMON_LOG_FILE);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_LIMIT) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    OpenOptions::new().create(true).append(true).open(path)
}

fn note(log: &mut File, line: &str) {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let _ = writeln!(log, "{} INFO  launcher: {line}", platform::format_local_time(ms));
}

fn start(daemon: &Path, args: &Args, log: &File) -> std::io::Result<std::process::Child> {
    let mut cmd = Command::new(daemon);
    if let Some(e) = &args.endpoint {
        cmd.arg("--endpoint").arg(e);
    }
    if let Some(d) = &args.data_dir {
        cmd.arg("--data-dir").arg(d);
    }
    cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log.try_clone()?);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
}

fn main() -> ExitCode {
    let args = Args::parse();
    // Nowhere to report to before the log is open: a GUI-subsystem process has no console.
    let Ok(data_dir) = args.data_dir.clone().map_or_else(platform::paths::default_data_dir, Ok) else {
        return ExitCode::FAILURE;
    };
    let Ok(endpoint) = args.endpoint.clone().map_or_else(platform::paths::default_endpoint, Ok) else {
        return ExitCode::FAILURE;
    };
    let Ok(me) = std::env::current_exe() else {
        return ExitCode::FAILURE;
    };
    let daemon = me.with_file_name(format!("{}{}", names::DAEMON_BIN, std::env::consts::EXE_SUFFIX));
    let mut policy = Policy::default();
    loop {
        let Ok(mut log) = open_log(&data_dir) else {
            return ExitCode::FAILURE;
        };
        let started = Instant::now();
        let status = match start(&daemon, &args, &log) {
            Ok(mut child) => {
                note(&mut log, &format!("started {} (pid {})", daemon.display(), child.id()));
                child.wait().ok()
            }
            Err(e) => {
                note(&mut log, &format!("cannot start {}: {e}", daemon.display()));
                None
            }
        };
        let code = status.and_then(|s| s.code());
        match policy.after(code, started.elapsed()) {
            After::Stop => {
                note(&mut log, "the daemon stopped normally; launcher done");
                return ExitCode::SUCCESS;
            }
            After::WaitForOther => {
                note(&mut log, &format!("another daemon owns {endpoint}; waiting for it to go"));
                drop(log);
                while platform::pipe_exists(&endpoint) {
                    std::thread::sleep(OTHER_DAEMON_POLL);
                }
            }
            After::Restart(delay) => {
                note(&mut log, &format!("the daemon stopped unexpectedly (exit {code:?}); starting again in {delay:?}"));
                drop(log);
                std::thread::sleep(delay);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CRASH: Option<i32> = Some(101);

    #[test]
    fn binary_name_matches_names_table() {
        assert_eq!(env!("CARGO_BIN_NAME"), names::LAUNCHER_BIN);
    }

    #[test]
    fn crashes_back_off_and_a_healthy_run_starts_over() {
        let mut p = Policy::default();
        let quick = Duration::from_secs(1);
        let delays: Vec<After> = (0..7).map(|_| p.after(CRASH, quick)).collect();
        let secs = |s| After::Restart(Duration::from_secs(s));
        assert_eq!(delays, [secs(2), secs(5), secs(10), secs(30), secs(60), secs(60), secs(60)]);
        // Killed from outside counts as a crash too.
        assert_eq!(p.after(None, quick), secs(60));
        assert_eq!(p.after(CRASH, HEALTHY_AFTER), secs(2));
        assert_eq!(p.after(CRASH, quick), secs(5));
    }

    #[test]
    fn a_normal_stop_ends_the_launcher_and_another_daemon_is_waited_for() {
        let mut p = Policy::default();
        assert_eq!(p.after(CRASH, Duration::ZERO), After::Restart(Duration::from_secs(2)));
        assert_eq!(p.after(Some(i32::from(names::DAEMON_EXIT_ALREADY_RUNNING)), Duration::ZERO), After::WaitForOther);
        assert_eq!(p.after(CRASH, Duration::ZERO), After::Restart(Duration::from_secs(2)), "starts over");
        assert_eq!(p.after(Some(0), Duration::ZERO), After::Stop);
    }

    #[test]
    fn a_big_log_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(names::DAEMON_LOG_FILE);
        std::fs::write(&path, vec![b'x'; LOG_LIMIT as usize + 1]).unwrap();
        let mut log = open_log(dir.path()).unwrap();
        note(&mut log, "hello");
        drop(log);
        assert!(std::fs::read_to_string(&path).unwrap().contains("launcher: hello"));
        assert_eq!(std::fs::metadata(dir.path().join("daemon.log.old")).unwrap().len(), LOG_LIMIT + 1);
    }
}
