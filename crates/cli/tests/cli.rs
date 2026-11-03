//! The real CLI binary against an in-process daemon on a test pipe (fake system calls).
//! Checks exit codes, JSON output, the catalog-driven commands and help grouped by section.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use daemon::{BackgroundDaemon, DaemonConfig, Module, TrustPolicy};
use daemon::{ArgKind, ArgSpec, CommandSpec, JournalEntry, ModuleCtx, ModuleRequest, ModuleSpec, RecoverOutcome};
use proto::{ApiError, Section, names};
use serde_json::{Value, json};

fn endpoint() -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!(r"\\.\pipe\away-guard-test-cli-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))
}

struct Daemon {
    endpoint: String,
    _dir: tempfile::TempDir,
    _daemon: BackgroundDaemon,
}

fn daemon_with(config_json: Option<Value>, extra: Vec<Box<dyn Module>>) -> Daemon {
    daemon_with_config_text(config_json.map(|c| c.to_string()), extra)
}

fn daemon_with_config_text(config: Option<String>, extra: Vec<Box<dyn Module>>) -> Daemon {
    daemon_with_sys(config, extra, daemon::sys::fake::sys().0)
}

fn daemon_with_sys(config: Option<String>, extra: Vec<Box<dyn Module>>, sys: daemon::sys::Sys) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let config_file = platform::paths::config_file(dir.path());
    if let Some(c) = config {
        std::fs::write(&config_file, c).unwrap();
    }
    let endpoint = endpoint();
    let config = DaemonConfig {
        endpoint: endpoint.clone(),
        data_dir: dir.path().to_owned(),
        config_file,
        sys,
        sqlite: platform::Winsqlite3::load().unwrap(),
        trust: TrustPolicy::none(),
    };
    let mut modules = daemon::modules::builtin();
    modules.extend(extra);
    let d = daemon::spawn(platform::PipeTransport, config, modules).unwrap();
    Daemon { endpoint, _dir: dir, _daemon: d }
}

fn daemon() -> Daemon {
    daemon_with(None, vec![])
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim()).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.stdout))
    }
}

fn cli(endpoint: &str, args: &[&str]) -> Out {
    let out = Command::new(env!("CARGO_BIN_EXE_away-guard"))
        .arg("--endpoint")
        .arg(endpoint)
        .args(args)
        .env_remove(names::ENV_ENDPOINT)
        .output()
        .unwrap();
    Out {
        code: out.status.code().unwrap(),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn without_a_daemon() {
    let ep = endpoint();
    let o = cli(&ep, &["status", "--json"]);
    assert_eq!(o.code, 3);
    assert_eq!(o.json()["error"]["code"], "DAEMON_NOT_RUNNING");
    assert_eq!(cli(&ep, &["status"]).code, 3);

    // Help and version still work and say where the command list comes from.
    let o = cli(&ep, &["--help"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("could not be reached") && o.stdout.contains("protect"), "{}", o.stdout);
    assert_eq!(cli(&ep, &["help"]).code, 0);
    assert_eq!(cli(&ep, &["-V"]).code, 0);
    let o = cli(&ep, &["--timeout", "soon", "status", "--json"]);
    assert_eq!((o.code, o.json()["error"]["code"].as_str()), (2, Some("USAGE")));
}

#[test]
fn status_and_log() {
    let d = daemon();
    let o = cli(&d.endpoint, &["status", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v = o.json();
    assert_eq!(v["ok"], true);
    assert_eq!(v["result"]["daemon"]["endpoint"], d.endpoint.as_str());
    // `--json` may go before the command too.
    assert_eq!(cli(&d.endpoint, &["--json", "core", "status"]).json()["ok"], true);

    let o = cli(&d.endpoint, &["status"]);
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("使用:") && o.stdout.contains("保护:"), "{}", o.stdout);

    assert_eq!(cli(&d.endpoint, &["free"]).code, 0);
    let o = cli(&d.endpoint, &["log"]);
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("resource.free"), "{}", o.stdout);
    assert!(o.stdout.contains(" agent pid "), "{}", o.stdout);
}

#[test]
fn status_says_what_did_not_take_effect_for_lack_of_administrator_rights() {
    let mut sys = daemon::sys::fake::sys().0;
    sys.elevated = std::sync::Arc::new(daemon::sys::fake::RefusingElevated::default());
    let d = daemon_with_sys(None, vec![], sys);
    let o = cli(&d.endpoint, &["away", "--json"]);
    assert_eq!((o.code, o.json()["result"]["mode"].as_str()), (0, Some("away")), "{}", o.stderr);
    // The power module applies the preset right after the mode change.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        let o = cli(&d.endpoint, &["status"]);
        if o.stdout.contains("! update_pause: 这一项需要管理员权限，没生效") || std::time::Instant::now() > deadline {
            break o.stdout;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(status.contains("! update_pause: 这一项需要管理员权限，没生效"), "{status}");
    assert!(status.contains("away (preset away"), "{status}");
    assert!(status.contains("battery 80%"), "{status}");
    assert_eq!(cli(&d.endpoint, &["back"]).code, 0);
}

#[test]
fn exit_codes_follow_the_table() {
    let d = daemon();
    let ep = d.endpoint.as_str();
    // Resource requests now succeed.
    assert_eq!(cli(ep, &["free"]).code, 0);
    assert_eq!(cli(ep, &["tasks"]).code, 0);
    // No such process (the fake process table is empty) or task.
    for args in [&["protect", "--pid", "5", "--deps", "wsl"][..], &["renew", "3"], &["unprotect", "3"]] {
        let o = cli(ep, args);
        assert_eq!(o.code, 16, "{args:?}: {}", o.stderr);
    }
    let o = cli(ep, &["task", "renew", "3", "--ttl-minutes", "30", "--json"]);
    assert_eq!((o.code, o.json()["error"]["subcode"].as_str()), (16, Some("task.NO_SUCH_TASK")));

    let o = cli(ep, &["frobnicate", "--json"]);
    assert_eq!((o.code, o.json()["error"]["code"].as_str()), (11, Some("UNKNOWN_COMMAND")));
    assert_eq!(cli(ep, &["task", "frobnicate"]).code, 11);

    for args in [&["renew"][..], &["renew", "abc"], &["protect", "--pid", "x"], &["protect", "--nope"], &[]] {
        assert_eq!(cli(ep, args).code, 2, "{args:?}");
    }
    let o = cli(ep, &["renew", "--json"]);
    assert_eq!(o.json()["error"]["code"], "USAGE");

    // The example module is off by default.
    assert_eq!(cli(ep, &["example", "echo", "hi"]).code, 14);
}

#[test]
fn broken_config_file_is_shown_and_modules_answer_disabled() {
    let d = daemon_with_config_text(Some("{ broken".into()), vec![]);
    let o = cli(&d.endpoint, &["status"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("CONFIG ERROR") && o.stdout.contains("config_error"), "{}", o.stdout);
    let o = cli(&d.endpoint, &["away", "--json"]);
    assert_eq!((o.code, o.json()["error"]["subcode"].as_str()), (14, Some("core.CONFIG_INVALID")));
}

#[test]
fn page_only_commands_are_not_offered() {
    let d = daemon();
    let o = cli(&d.endpoint, &["permission", "set", "--program", "x.exe", "--class", "protected", "--json"]);
    assert_eq!(o.code, 15);
    assert_eq!(o.json()["error"]["subcode"], "core.AGENT_NOT_ALLOWED");
    let help = cli(&d.endpoint, &["--help"]).stdout;
    assert!(help.contains("permission list") && !help.contains("permission set"), "{help}");
}

#[test]
fn help_is_grouped_by_section() {
    let d = daemon();
    let o = cli(&d.endpoint, &["--help"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let h = &o.stdout;
    let at = |s: &str| h.find(s).unwrap_or_else(|| panic!("{s} missing from:\n{h}"));
    assert!(at("公共部分 (common):") < at("使用 (usage):") && at("使用 (usage):") < at("保护 (protection):"));
    for short in proto::commands::COMMON_SHORT_NAMES {
        assert!(h.contains(&format!("  {short}, ")), "{short} missing from:\n{h}");
    }
    assert!(at("protect, task protect") > at("使用 (usage):"));
    assert!(at("status, core status") < at("使用 (usage):"));
    assert!(h.contains("example echo") && h.contains("[module off]"));
    assert!(!h.contains("core hello"));

    let o = cli(&d.endpoint, &["help", "protect"]);
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("--pid") && o.stdout.contains("--ttl-minutes"), "{}", o.stdout);
}

#[test]
fn example_module_switched_on_by_config_appears_without_cli_changes() {
    let d = daemon_with(Some(json!({"modules": {"example": {"enabled": true}}})), vec![]);
    let o = cli(&d.endpoint, &["example", "echo", "hello there", "--shout", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.json()["result"]["echo"], "HELLO THERE");
    let o = cli(&d.endpoint, &["example", "history", "--limit", "1", "--json"]);
    assert_eq!(o.json()["result"]["items"][0]["text"], "hello there");
    let o = cli(&d.endpoint, &["example", "echo", " ", "--json"]);
    assert_eq!((o.code, o.json()["error"]["subcode"].as_str()), (12, Some("example.EMPTY_TEXT")));
    assert!(!cli(&d.endpoint, &["--help"]).stdout.contains("[module off]"));
}

static WEATHER: ModuleSpec = ModuleSpec::new("weather", Section::Protection, "Exists only in this test").commands(&[
    CommandSpec::new("forecast", "Tomorrow's weather").short("forecast").agent().args(&[
        ArgSpec::new("days", ArgKind::Uint, "How many days").required(),
        ArgSpec::new("places", ArgKind::StrList, "Where"),
    ]),
]);

struct Weather;

impl Module for Weather {
    fn spec(&self) -> &'static ModuleSpec {
        &WEATHER
    }

    fn handle(&mut self, req: &ModuleRequest<'_>, _ctx: &ModuleCtx) -> Result<Value, ApiError> {
        Ok(json!({ "got": req.args }))
    }

    fn recover(&mut self, _entry: &JournalEntry, _ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        Ok(RecoverOutcome::Restored)
    }
}

#[test]
fn a_module_defined_elsewhere_gets_cli_commands() {
    let d = daemon_with(None, vec![Box::new(Weather)]);
    let o = cli(&d.endpoint, &["forecast", "--days", "2", "--places", "Kelowna", "--places", "Tokyo", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.json()["result"]["got"], json!({"days": 2, "places": ["Kelowna", "Tokyo"]}));
    assert_eq!(cli(&d.endpoint, &["weather", "forecast", "--days", "1"]).code, 0);
    assert_eq!(cli(&d.endpoint, &["forecast"]).code, 2);
    let h = cli(&d.endpoint, &["--help"]).stdout;
    let protection = h.find("保护 (protection):").unwrap();
    assert!(h.find("forecast, weather forecast").unwrap() > protection, "{h}");
}
