//! Real executable fixture: no model calls, only real hook/daemon/launcher IPC.
use std::{
    env,
    fs::OpenOptions,
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};
fn quote(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}
fn hook(session: &str, event: &str) {
    let cwd = env::current_dir().unwrap().to_string_lossy().into_owned();
    let body = format!(
        r#"{{"session_id":{},"cwd":{},"hook_event_name":{},"reason":"other","error":"server_error","last_assistant_message":"Network connection error: fixture disconnected"}}"#,
        quote(session),
        quote(&cwd),
        quote(event)
    );
    let mut p = Command::new(env::var("STUB_HOOK").unwrap())
        .args(["--agent", "claude-code"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    p.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    let _ = p.wait();
}
fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|s| s == "--version") {
        println!("2.1.309 (Claude Code)");
        return;
    }
    if args.first().map(String::as_str) == Some("seed") {
        let session = &args[1];
        hook(session, "StopFailure");
        if args.get(2).map(String::as_str) == Some("hold") {
            std::thread::sleep(Duration::from_secs(45));
        } else if args.get(2).map(String::as_str) != Some("crash") {
            hook(session, "SessionEnd");
        }
        return;
    }
    let session = args
        .windows(2)
        .find(|w| w[0] == "--session-id")
        .or_else(|| args.windows(2).find(|w| w[0] == "--resume"))
        .unwrap()[1]
        .clone();
    let row = format!(
        r#"{{"session":{},"args":[{}],"base":{},"proxy":{},"config":{},"fixture_auth_preserved":{}}}"#,
        quote(&session),
        args.iter().map(|s| quote(s)).collect::<Vec<_>>().join(","),
        quote(&env::var("ANTHROPIC_BASE_URL").unwrap_or_default()),
        quote(&env::var("HTTPS_PROXY").unwrap_or_default()),
        quote(&env::var("CLAUDE_CONFIG_DIR").unwrap_or_default()),
        env::var("ANTHROPIC_API_KEY").ok().as_deref() == Some("fixture-dummy")
    );
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(env::var("STUB_LOG").unwrap())
        .unwrap();
    writeln!(f, "{row}").unwrap();
    if env::var("STUB_REPEAT").ok().as_deref() == Some("1") {
        hook(&session, "StopFailure");
        hook(&session, "SessionEnd");
    }
}
