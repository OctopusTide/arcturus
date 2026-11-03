#![cfg(all(windows, feature = "daemon-mode"))]
use daemon_types::{System, TreeTracker};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
// Platform reuses these traits through its sys return type; no administrator service is used.
mod daemon_types {
    pub use daemon::sys::{System, TreeTracker};
}
struct Own {
    parent: Child,
    system: Arc<dyn System>,
    job: Option<Box<dyn TreeTracker>>,
}
impl Drop for Own {
    fn drop(&mut self) {
        let _ = self.parent.kill();
        let _ = self.parent.wait();
        if let Some(job) = &self.job {
            for pid in job.members().unwrap_or_default() {
                if let Ok(info) = self.system.process_info(pid) {
                    let _ = self.system.terminate_process(pid, info.started_at);
                }
            }
        }
    }
}
async fn health(client: &reqwest::Client, url: &str, old: Option<u32>) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok(response) = client.get(format!("{url}/_arcturus/health")).send().await {
            if let Ok(text) = response.text().await {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(pid) = value["worker_pid"].as_u64().map(|n| n as u32) {
                        if Some(pid) != old {
                            return pid;
                        }
                    }
                }
            }
        }
        assert!(Instant::now() < deadline, "watchdog worker did not become ready");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
#[tokio::test]
async fn an_owned_worker_is_restarted_after_it_is_terminated() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = port.local_addr().unwrap();
    drop(port);
    // Create suspended, attach to our tracking job, then resume: every later worker belongs
    // to this test's tree and is reliably cleaned up even if an assertion fails.
    let parent = Command::new(env!("CARGO_BIN_EXE_away-guard-relay"))
        .args(["--listen", &address.to_string(), "--upstream", "http://127.0.0.1:9", "--mode", "normal"])
        .creation_flags(0x08000000 | 0x00000004)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let system = platform::sys().system;
    let mut own = Own { parent, system, job: None };
    let pid = own.parent.id();
    let info = own.system.process_info(pid).unwrap();
    own.job = Some(own.system.track_tree(pid, &[]).unwrap());
    let mut threads = own.system.suspension_state(pid, info.started_at).unwrap();
    for thread in &mut threads {
        thread.suspend_count = thread.suspend_count.saturating_sub(1);
    }
    own.system.thaw_process(pid, info.started_at, &threads).unwrap();
    let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_millis(400)).build().unwrap();
    let url = format!("http://{address}");
    let first = health(&client, &url, None).await;
    let process = own.system.process_info(first).unwrap();
    own.system.terminate_process(first, process.started_at).unwrap();
    let started = Instant::now();
    let second = health(&client, &url, Some(first)).await;
    assert_ne!(first, second);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn automatic_buffering_follows_daemon_mode_over_its_real_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = format!(r"\\.\pipe\away-guard-relay-mode-test-{}", std::process::id());
    let (sys, _, _) = daemon::sys::fake::sys();
    let config = daemon::DaemonConfig {
        endpoint: endpoint.clone(),
        data_dir: dir.path().into(),
        config_file: platform::paths::config_file(dir.path()),
        sys,
        sqlite: platform::Winsqlite3::load().unwrap(),
        trust: daemon::TrustPolicy::trusting(std::iter::empty::<&str>()),
    };
    // Only the mode module, and every system operation is fake: no real away effects.
    let running = daemon::spawn(platform::PipeTransport, config, vec![daemon::modules::mode::create()]).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = port.local_addr().unwrap();
    drop(port);
    let parent = Command::new(env!("CARGO_BIN_EXE_away-guard-relay"))
        .args([
            "--worker",
            "--listen",
            &address.to_string(),
            "--upstream",
            "http://127.0.0.1:9",
            "--mode",
            "auto",
            "--endpoint",
            &endpoint,
        ])
        .creation_flags(0x08000000)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _own = Own { parent, system: platform::sys().system, job: None };
    let http = reqwest::Client::builder().no_proxy().timeout(Duration::from_millis(400)).build().unwrap();
    let url = format!("http://{address}/_arcturus/health");
    async fn wait_mode(http: &reqwest::Client, url: &str, wanted: bool) {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if let Ok(response) = http.get(url).send().await {
                if let Ok(text) = response.text().await {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        if value["buffering"] == wanted {
                            return;
                        }
                    }
                }
            }
            assert!(Instant::now() < deadline, "relay failed to follow mode buffering={wanted}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    let mut client = platform::connect_client(
        &endpoint,
        proto::ClientInfo { name: "relay-test".into(), version: None },
        Duration::from_secs(2),
    )
    .unwrap();
    wait_mode(&http, &url, false).await;
    client.call("away", serde_json::json!({})).unwrap().into_result().unwrap();
    wait_mode(&http, &url, true).await;
    client.call("back", serde_json::json!({})).unwrap().into_result().unwrap();
    wait_mode(&http, &url, false).await;
    drop(client);
    running.stop().unwrap();
}
