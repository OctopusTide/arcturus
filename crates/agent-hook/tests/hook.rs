//! The real hook program against an in-process daemon on a test pipe (fake system calls): what
//! an agent's hook sends arrives as one unified `agents.*` event, conversation text does not
//! travel, and the agent is never blocked or failed.

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use daemon::{BackgroundDaemon, DaemonConfig, Event, JournalEntry, Module, ModuleCtx, ModuleRequest, ModuleSpec, RecoverOutcome, Section, TrustPolicy};
use proto::{ApiError, names};
use serde_json::{Value, json};

static WATCH: ModuleSpec = ModuleSpec::new("watch", Section::Common, "Records agents.* events").subscribes(&["agents.*"]);

struct Watch(Arc<Mutex<Vec<Event>>>);

impl Module for Watch {
    fn spec(&self) -> &'static ModuleSpec {
        &WATCH
    }
    fn handle(&mut self, _req: &ModuleRequest<'_>, _ctx: &ModuleCtx) -> Result<Value, ApiError> {
        Ok(Value::Null)
    }
    fn recover(&mut self, _e: &JournalEntry, _ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        Ok(RecoverOutcome::Restored)
    }
    fn on_event(&mut self, event: &Event, _ctx: &ModuleCtx) -> Result<(), ApiError> {
        self.0.lock().unwrap().push(event.clone());
        Ok(())
    }
}

fn endpoint() -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!(r"\\.\pipe\away-guard-test-hook-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))
}

fn daemon(seen: Arc<Mutex<Vec<Event>>>) -> (String, tempfile::TempDir, BackgroundDaemon) {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = endpoint();
    let config = DaemonConfig {
        endpoint: endpoint.clone(),
        data_dir: dir.path().to_owned(),
        config_file: platform::paths::config_file(dir.path()),
        sys: daemon::sys::fake::sys().0,
        sqlite: platform::Winsqlite3::load().unwrap(),
        trust: TrustPolicy::none(),
    };
    let mut modules = daemon::modules::builtin();
    modules.push(Box::new(Watch(seen)));
    let d = daemon::spawn(platform::PipeTransport, config, modules).unwrap();
    (endpoint, dir, d)
}

fn run(endpoint: &str, agent: &str, stdin: &str) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_away-guard-hook"))
        .args(["--agent", agent, "--endpoint", endpoint, "--timeout", "5"])
        .env_remove(names::ENV_ENDPOINT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code().unwrap(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn wait_for(seen: &Arc<Mutex<Vec<Event>>>, n: usize) -> Vec<Event> {
    for _ in 0..100 {
        if seen.lock().unwrap().len() >= n {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    seen.lock().unwrap().clone()
}

#[test]
fn a_claude_code_stop_failure_arrives_as_a_unified_turn_end() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (ep, _dir, _d) = daemon(seen.clone());
    let input = json!({"session_id": "s-1", "transcript_path": r"C:\t\s-1.jsonl", "cwd": r"D:\work",
        "hook_event_name": "StopFailure", "error": "server_error",
        "last_assistant_message": "API Error: Unable to connect to API (secret text)"});
    let (code, stdout, stderr) = run(&ep, "claude-code", &input.to_string());
    assert_eq!((code, stdout.as_str()), (0, ""), "stderr: {stderr}");
    let events = wait_for(&seen, 1);
    assert_eq!(events.len(), 1, "{events:?}");
    let e = &events[0];
    assert_eq!(e.topic, "agents.turn_end");
    assert_eq!(e.data["agent"], "claude-code");
    assert_eq!(e.data["session_id"], "s-1");
    assert_eq!(e.data["cwd"], r"D:\work");
    assert_eq!(e.data["transcript"], r"C:\t\s-1.jsonl");
    assert_eq!(e.data["native_event"], "StopFailure");
    assert_eq!(e.data["outcome"]["kind"], "api_error");
    assert_eq!(e.data["outcome"]["failure"]["class"], "network");
    assert_eq!(e.data["outcome"]["failure"]["network"], true);
    assert!(e.data["hook_pid"].is_u64());
    assert!(!e.data.to_string().contains("secret text"), "no conversation text in the event");

    let codex = json!({"session_id": "t-9", "transcript_path": null, "cwd": "C:\\w", "hook_event_name": "SessionStart", "source": "startup"});
    let (code, _, _) = run(&ep, "codex", &codex.to_string());
    assert_eq!(code, 0);
    let events = wait_for(&seen, 2);
    assert_eq!(events[1].topic, "agents.session_start");
    assert_eq!((events[1].data["agent"].as_str(), events[1].data["detail"].as_str()), (Some("codex"), Some("startup")));
}

/// A real process named claude.exe above the hook makes the hook leave a broker behind, whoever
/// runs the tests. The agent gets the hook's pipes back at once all the same, and the daemon's
/// fake system cannot vouch for the broker but still hears about the failure.
#[test]
fn the_hook_lets_go_of_its_pipes_even_though_a_broker_stays_behind() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (ep, _dir, _d) = daemon(seen.clone());
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("claude.exe");
    let windows = std::env::var("SystemRoot").unwrap();
    std::fs::copy(format!(r"{windows}\System32\cmd.exe"), &agent).unwrap();
    let input = dir.path().join("input.json");
    std::fs::write(
        &input,
        json!({"session_id": "s-2", "cwd": r"D:\work", "hook_event_name": "StopFailure", "error": "server_error",
            "last_assistant_message": "API Error: Unable to connect to API"})
        .to_string(),
    )
    .unwrap();
    let line = format!(
        r#"/d /c ""{}" --agent claude-code --endpoint {ep} --timeout 5 < "{}"""#,
        env!("CARGO_BIN_EXE_away-guard-hook"),
        input.display()
    );
    let started = std::time::Instant::now();
    let out = Command::new(&agent)
        .raw_arg(&line)
        .env_remove(names::ENV_ENDPOINT)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "held for {:?}", started.elapsed());
    // Our own hook binary, still running with the agent's copy gone: that is the broker.
    let hook = std::path::Path::new(env!("CARGO_BIN_EXE_away-guard-hook")).file_name().unwrap().to_string_lossy().into_owned();
    let list = Command::new("tasklist").args(["/FI", &format!("IMAGENAME eq {hook}"), "/NH"]).output().unwrap();
    assert!(String::from_utf8_lossy(&list.stdout).contains(&hook), "no broker was started: the test proved nothing");
    let events = wait_for(&seen, 1);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].topic, "agents.turn_end");
    assert_eq!(events[0].data["session_id"], "s-2");
    assert_eq!(events[0].data["in_a_window"], true, "its command line has no -p");
}

#[test]
fn the_agent_is_never_blocked_or_failed() {
    // Input it does not recognise: exit 0, nothing on stdout, a reason on stderr.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (ep, _dir, _d) = daemon(seen.clone());
    let (code, stdout, stderr) = run(&ep, "claude-code", r#"{"hook_event_name":"PreToolUse","session_id":"s"}"#);
    assert_eq!((code, stdout.as_str()), (0, ""));
    assert!(stderr.contains("not recognised"), "{stderr}");
    let (code, _, stderr) = run(&ep, "no-such-agent", "{}");
    assert_eq!(code, 0);
    assert!(stderr.contains("no agent"), "{stderr}");
    // No daemon at all.
    let (code, stdout, _) = run(&endpoint(), "claude-code", r#"{"hook_event_name":"Stop","session_id":"s"}"#);
    assert_eq!((code, stdout.as_str()), (0, ""));
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn install_without_apply_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_away-guard-hook"))
        .args(["--json", "install", "--agent", "claude-code"])
        // No agent can be found in an empty home: planning refuses before anything is written.
        .env("USERPROFILE", dir.path())
        .env("HOME", dir.path())
        .env("LOCALAPPDATA", dir.path())
        .env("APPDATA", dir.path())
        .env("PATH", dir.path())
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(v["error"].as_str().unwrap().contains("not installed"), "{v}");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
