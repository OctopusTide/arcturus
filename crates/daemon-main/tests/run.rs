//! The daemon binary in the foreground on a test pipe and a temp data folder. Only reads
//! the system; the child is ended by the test when done.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use proto::{ClientInfo, ErrorCode, names};

fn endpoint(tag: &str) -> String {
    format!(r"\\.\pipe\away-guard-test-bin-{tag}-{}", std::process::id())
}

fn spawn(endpoint: &str, data: &std::path::Path) -> Child {
    read_only_config(data);
    Command::new(env!("CARGO_BIN_EXE_away-guard-daemon"))
        .arg("--endpoint")
        .arg(endpoint)
        .arg("--data-dir")
        .arg(data)
        .env_remove(names::ENV_ENDPOINT)
        .env_remove(names::ENV_DATA_DIR)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn read_only_config(data: &std::path::Path) {
    let path = platform::paths::config_file(data);
    if !path.exists() {
        std::fs::write(path, r#"{"modules":{"network":{"enabled":false}}}"#).unwrap();
    }
}

struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn info() -> ClientInfo {
    ClientInfo { name: "bin-test".into(), version: None }
}

fn wait_until_up(endpoint: &str) -> proto::client::Client {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match platform::connect_client(endpoint, info(), Duration::from_secs(2)) {
            Ok(c) => return c,
            Err(e) if e.code == ErrorCode::DaemonNotRunning && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("daemon did not come up: {e}"),
        }
    }
}

#[test]
fn runs_in_the_foreground_and_answers() {
    let dir = tempfile::tempdir().unwrap();
    let ep = endpoint("run");
    let child = Guard(spawn(&ep, dir.path()));
    let mut c = wait_until_up(&ep);
    assert_eq!(c.hello().daemon.pid, child.0.id());
    let status = c.call("status", serde_json::json!({})).unwrap().into_result().unwrap();
    assert_eq!(status["daemon"]["endpoint"], ep.as_str());
    assert_eq!(status["daemon"]["data_dir"], dir.path().to_str().unwrap());
    assert!(dir.path().join(names::DB_FILE).exists());

    // A second daemon on the same endpoint gives up instead of sharing it.
    let dir2 = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_away-guard-daemon"))
        .arg("--endpoint")
        .arg(&ep)
        .arg("--data-dir")
        .arg(dir2.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(i32::from(names::DAEMON_EXIT_ALREADY_RUNNING)));
    assert!(String::from_utf8_lossy(&out.stderr).contains("already owns"), "{}", String::from_utf8_lossy(&out.stderr));
}

fn kill_pid(pid: u32) {
    // Only ever a daemon this test started (through its launcher).
    let _ = Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).output();
}

/// The launcher first, so it cannot start the daemon again; then the daemon.
struct LauncherGuard {
    launcher: Child,
    daemon_pid: std::cell::Cell<Option<u32>>,
}

impl Drop for LauncherGuard {
    fn drop(&mut self) {
        let _ = self.launcher.kill();
        let _ = self.launcher.wait();
        if let Some(pid) = self.daemon_pid.get() {
            kill_pid(pid);
        }
    }
}

#[test]
fn the_launcher_starts_the_daemon_again_after_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    read_only_config(dir.path());
    let ep = endpoint("launcher");
    let launcher = Command::new(env!("CARGO_BIN_EXE_away-guard-launcher"))
        .arg("--endpoint")
        .arg(&ep)
        .arg("--data-dir")
        .arg(dir.path())
        .env_remove(names::ENV_ENDPOINT)
        .env_remove(names::ENV_DATA_DIR)
        .spawn()
        .unwrap();
    let g = LauncherGuard { launcher, daemon_pid: std::cell::Cell::new(None) };
    let first = wait_until_up(&ep).hello().daemon.pid;
    g.daemon_pid.set(Some(first));
    assert_ne!(first, g.launcher.id(), "the daemon is a separate process");

    kill_pid(first);
    let deadline = Instant::now() + Duration::from_secs(20);
    let second = loop {
        if let Ok(c) = platform::connect_client(&ep, info(), Duration::from_millis(500))
            && c.hello().daemon.pid != first
        {
            break c.hello().daemon.pid;
        }
        assert!(Instant::now() < deadline, "the daemon was not started again");
        std::thread::sleep(Duration::from_millis(100));
    };
    g.daemon_pid.set(Some(second));
    let log = std::fs::read_to_string(dir.path().join(names::DAEMON_LOG_FILE)).unwrap();
    assert!(log.contains("stopped unexpectedly") && log.contains("starting again in 2s"), "{log}");
    assert!(log.contains("starting (pid"), "the daemon's own output goes to the log too:\n{log}");
}

#[test]
fn broken_config_file_keeps_running_and_reports_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(platform::paths::config_file(dir.path()), "{ nope").unwrap();
    let ep = endpoint("cfg");
    let mut child = Guard(spawn(&ep, dir.path()));
    let mut c = wait_until_up(&ep);
    let status = c.call("status", serde_json::json!({})).unwrap().into_result().unwrap();
    assert!(status["daemon"]["config_error"].as_str().unwrap().contains(names::CONFIG_FILE), "{status}");
    assert!(status["modules"].as_array().unwrap().iter().all(|m| m["health"] == "config_error"));
    let resp = c.call("away", serde_json::json!({})).unwrap();
    assert_eq!(resp.error.unwrap().subcode.as_deref(), Some(proto::subcode::CONFIG_INVALID));
    assert!(child.0.try_wait().unwrap().is_none(), "daemon is still running");
}
