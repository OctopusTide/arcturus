//! Claude Code adapter against synthetic transcripts and settings in a temporary folder
//! (shaped like the real 2.1.30x files on this machine; nothing real is read).

use std::io::Write;
use std::path::{Path, PathBuf};

use agents::hooks::{FailureClass, HookError, HookInstall, HookKind, TurnOutcome};
use agents::model::{Encoding, FileOp, ReadOptions, Record, Role, Shell, Stop};
use agents::{Adapter, Env};
use serde_json::{Value, json};

const SID: &str = "03e6260b-b3c1-4978-aa84-8bee22998188";

fn adapter() -> &'static dyn Adapter {
    agents::get("claude-code").unwrap()
}

fn rec(ty: &str, ts: &str, cwd: &str, message: Value, extra: Value) -> String {
    let mut v = json!({"parentUuid": null, "isSidechain": false, "message": message, "type": ty,
                       "uuid": "u", "timestamp": ts, "cwd": cwd, "sessionId": SID, "version": "2.1.309"});
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        for (k, x) in e {
            o.insert(k.clone(), x.clone());
        }
    }
    v.to_string()
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": name, "input": input, "caller": {"type": "direct"}}]})
}

fn tool_result(id: &str, is_error: bool) -> Value {
    json!({"role": "user", "content": [{"tool_use_id": id, "type": "tool_result", "content": "big output ...", "is_error": is_error}]})
}

fn write_lines(path: &Path, lines: &[String]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn setup() -> (tempfile::TempDir, Env, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let env = Env::with_home(dir.path());
    let main = dir.path().join(".claude").join("projects").join("D--work").join(format!("{SID}.jsonl"));
    (dir, env, main)
}

fn read_all(file: &agents::TranscriptFile, from: u64, text: bool) -> (Vec<Record>, agents::ReadOutcome) {
    let mut got = Vec::new();
    let out = adapter().read(file, from, ReadOptions { text }, &mut |r| got.push(r)).unwrap();
    (got, out)
}

#[test]
fn lists_main_and_subagent_transcripts() {
    let (dir, env, main) = setup();
    write_lines(&main, &[]);
    let sess = main.with_extension("");
    write_lines(&sess.join("subagents").join("agent-a1.jsonl"), &[]);
    write_lines(&sess.join("subagents").join("workflows").join("wf_1").join("agent-b2.jsonl"), &[]);
    write_lines(&sess.join("subagents").join("agent-a1.meta.json"), &[]);
    write_lines(&sess.join("tool-results").join("x.txt"), &[]);
    std::fs::write(dir.path().join(".claude").join("projects").join("stray.jsonl"), "").unwrap();
    let mut files = adapter().transcripts(&env).unwrap();
    files.sort_by(|a, b| a.subagent.cmp(&b.subagent));
    let summary: Vec<(String, Option<String>)> = files.iter().map(|f| (f.conversation_id.clone(), f.subagent.clone())).collect();
    assert_eq!(
        summary,
        [(SID.to_owned(), None), (SID.to_owned(), Some("a1".to_owned())), (SID.to_owned(), Some("b2".to_owned()))]
    );
    assert!(files.iter().all(|f| f.agent == "claude-code" && f.encoding == Encoding::Plain));
    // CLAUDE_CONFIG_DIR moves everything.
    let other = env.clone().set("CLAUDE_CONFIG_DIR", dir.path().join("elsewhere").to_string_lossy());
    assert!(adapter().transcripts(&other).unwrap().is_empty());
}

#[test]
fn reads_writes_commands_messages_and_errors() {
    let (_dir, env, main) = setup();
    let ts = "2026-11-01T22:19:21.644Z";
    write_lines(
        &main,
        &[
            json!({"type": "custom-title", "customTitle": "secret title", "sessionId": SID}).to_string(),
            rec("user", ts, r"D:\work", json!({"role": "user", "content": "please build it"}), json!({})),
            rec(
                "assistant",
                ts,
                r"D:\work",
                json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "hmm"}, {"type": "text", "text": "ok"},
                       {"type": "tool_use", "id": "t1", "name": "Write", "input": {"file_path": r"D:\work\a.txt", "content": "x"}}]}),
                json!({}),
            ),
            rec("user", ts, r"D:\work", tool_result("t1", false), json!({"toolUseResult": {"type": "create", "filePath": r"D:\work\a.txt", "content": "x", "originalFile": null}})),
            rec("assistant", ts, r"D:\work\sub", tool_use("t2", "PowerShell", json!({"command": "git clone https://x/y.git", "description": "d"})), json!({})),
            rec("user", ts, r"D:\work\sub", tool_result("t2", false), json!({"toolUseResult": {"stdout": "", "stderr": "", "interrupted": false}})),
            rec("assistant", ts, r"D:\work\sub", tool_use("t3", "Edit", json!({"file_path": r"D:\work\b.rs", "old_string": "a", "new_string": "b"})), json!({})),
            rec("user", ts, r"D:\work\sub", tool_result("t3", true), json!({"toolUseResult": "Error: no match"})),
            rec("assistant", ts, r"D:\work\sub", tool_use("t4", "Read", json!({"file_path": r"D:\work\c.rs"})), json!({})),
            rec("user", ts, r"D:\work\sub", tool_result("t4", false), json!({"toolUseResult": {"type": "text", "file": {"filePath": r"D:\work\c.rs"}}})),
            json!({"type": "api-request-blob", "message": tool_use("t9", "Write", json!({"file_path": r"D:\never.txt"})), "sessionId": SID}).to_string(),
            "{not json".to_owned(),
            rec(
                "assistant",
                ts,
                r"D:\work\sub",
                json!({"role": "assistant", "model": "<synthetic>", "content": [{"type": "text", "text": "API Error: Can't reach the API server"}]}),
                json!({"isApiErrorMessage": true, "error": "server_error"}),
            ),
        ],
    );
    let file = adapter().transcripts(&env).unwrap().remove(0);
    let (got, out) = read_all(&file, 0, false);
    assert_eq!(out.stop, None);
    assert_eq!(out.offset, file.size);
    assert_eq!(out.bad_lines, 1);
    assert_eq!(out.ignored.get("custom-title"), Some(&1));
    assert_eq!(out.ignored.get("api-request-blob"), Some(&1), "duplicated tool calls in request blobs are not read");
    assert_eq!(out.version.as_deref(), Some("2.1.309"));

    let writes: Vec<(String, FileOp)> =
        got.iter().filter_map(|r| if let Record::FileWrite { path, op, .. } = r { Some((path.clone(), op.clone())) } else { None }).collect();
    assert_eq!(writes, [(r"D:\work\a.txt".to_owned(), FileOp::Create)], "failed edits and reads are not writes");
    let cmds: Vec<(Shell, String, Option<String>)> = got
        .iter()
        .filter_map(|r| if let Record::Command { shell, text, cwd, .. } = r { Some((shell.clone(), text.clone(), cwd.clone())) } else { None })
        .collect();
    assert_eq!(cmds, [(Shell::PowerShell, "git clone https://x/y.git".to_owned(), Some(r"D:\work\sub".to_owned()))]);
    let sessions: Vec<Option<String>> =
        got.iter().filter_map(|r| if let Record::Session { cwd, .. } = r { Some(cwd.clone()) } else { None }).collect();
    assert_eq!(sessions, [Some(r"D:\work".to_owned()), Some(r"D:\work\sub".to_owned())], "a new Session when cwd changes");
    let messages: Vec<(Role, usize, String)> =
        got.iter().filter_map(|r| if let Record::Message { role, chars, text, .. } = r { Some((*role, *chars, text.clone())) } else { None }).collect();
    assert_eq!(messages, [(Role::User, 15, String::new()), (Role::Assistant, 2, String::new())], "no text unless asked");
    let failed: Vec<_> = got.iter().filter_map(|r| if let Record::TurnFailed { failure, .. } = r { Some(failure.clone()) } else { None }).collect();
    assert_eq!(failed.len(), 1);
    assert_eq!((failed[0].class, failed[0].network), (FailureClass::Network, Some(true)));
    let results = got.iter().filter(|r| matches!(r, Record::ToolResult { .. })).count();
    assert_eq!(results, 4);
    assert!(got.iter().all(|r| !matches!(r, Record::ToolCall { input, .. } if !input.is_null())));
    assert!(matches!(&got[0], Record::Session { ts: Some(1_793_571_561_644), version: Some(v), .. } if v == "2.1.309"));

    let (with_text, _) = read_all(&file, 0, true);
    assert!(with_text.iter().any(|r| matches!(r, Record::Message { text, .. } if text == "please build it")));
    assert!(with_text.iter().any(|r| matches!(r, Record::ToolCall { name, input, .. } if name == "Write" && input["content"] == "x")));
}

#[test]
fn incremental_reads_resume_and_finish_writes_started_earlier() {
    let (_dir, env, main) = setup();
    let ts = "2026-11-01T22:19:21.644Z";
    write_lines(&main, &[rec("assistant", ts, r"D:\p", tool_use("w1", "Write", json!({"file_path": r"D:\p\x.bin", "content": ""})), json!({}))]);
    // A line still being written.
    std::fs::OpenOptions::new().append(true).open(&main).unwrap().write_all(b"{\"type\":\"user\",").unwrap();
    let file = adapter().transcripts(&env).unwrap().remove(0);
    let (got, out) = read_all(&file, 0, false);
    assert!(got.iter().any(|r| matches!(r, Record::ToolCall { .. })));
    assert!(out.offset < file.size, "the unfinished line is left for next time");
    let first = out.offset;

    // The rest of the file arrives; the result answers a call read in the first pass.
    std::fs::OpenOptions::new().write(true).open(&main).unwrap().set_len(first).unwrap();
    write_lines(
        &main,
        &[
            rec("user", ts, r"D:\p", tool_result("w1", false), json!({"toolUseResult": {"type": "update", "filePath": r"D:\p\x.bin"}})),
            rec("user", ts, r"D:\p", tool_result("e7", false), json!({"toolUseResult": {"filePath": r"D:\p\y.rs", "oldString": "a", "newString": "b", "originalFile": "..."}})),
        ],
    );
    let file = adapter().transcripts(&env).unwrap().remove(0);
    let (got, out) = read_all(&file, first, false);
    assert_eq!(out.offset, file.size);
    let writes: Vec<(String, String, FileOp)> = got
        .iter()
        .filter_map(|r| if let Record::FileWrite { tool, path, op, .. } = r { Some((tool.clone(), path.clone(), op.clone())) } else { None })
        .collect();
    assert_eq!(
        writes,
        [
            ("Write".to_owned(), r"D:\p\x.bin".to_owned(), FileOp::Overwrite),
            ("Edit".to_owned(), r"D:\p\y.rs".to_owned(), FileOp::Modify)
        ]
    );
}

#[test]
fn unknown_versions_and_formats_stop_the_file() {
    let (_dir, env, main) = setup();
    let ts = "2026-11-01T22:19:21.644Z";
    let newer = rec("user", ts, r"D:\p", json!({"role": "user", "content": "hi"}), json!({})).replace("2.1.309", "2.1.400");
    write_lines(&main, &[rec("user", ts, r"D:\p", json!({"role": "user", "content": "hi"}), json!({})), newer.clone()]);
    let file = adapter().transcripts(&env).unwrap().remove(0);
    let (got, out) = read_all(&file, 0, false);
    let first_len = file.size - newer.len() as u64 - 1;
    match &out.stop {
        Some(Stop::UnknownVersion { version, at, detail }) => {
            assert_eq!(version, "2.1.400");
            assert_eq!(*at, first_len);
            assert!(detail.contains("not recognised"), "{detail}");
        }
        other => panic!("expected an unknown version, got {other:?}"),
    }
    assert_eq!(out.offset, first_len, "stopped before the unknown record");
    assert_eq!(got.iter().filter(|r| matches!(r, Record::Message { .. })).count(), 1);

    std::fs::remove_file(&main).unwrap();
    write_lines(&main, &[json!({"type": "assistant", "sessionId": SID, "version": "2.1.309", "message": "flat"}).to_string()]);
    let file = adapter().transcripts(&env).unwrap().remove(0);
    let (_, out) = read_all(&file, 0, false);
    assert!(matches!(out.stop, Some(Stop::UnknownFormat { at: 0, .. })), "{:?}", out.stop);
}

fn settings(env: &Env) -> PathBuf {
    env.home.join(".claude").join("settings.json")
}

fn install(program: &str, version: Option<&str>) -> HookInstall {
    HookInstall {
        program: PathBuf::from(program),
        kinds: vec![(HookKind::SessionStart, 10), (HookKind::TurnEnd, 30)],
        agent_version: version.map(str::to_owned),
    }
}

#[test]
fn hook_install_plan_apply_and_uninstall_keep_the_users_settings() {
    let (_dir, env, _) = setup();
    let original = "{\n  \"theme\": \"dark\",\n  \"autoCompactWindow\": 512000,\n  \"hooks\": {\n    \"Stop\": [\n      {\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"theirs.exe\"\n          }\n        ]\n      }\n    ]\n  }\n}\n";
    std::fs::create_dir_all(settings(&env).parent().unwrap()).unwrap();
    std::fs::write(settings(&env), original).unwrap();
    let hooks = adapter().hooks().unwrap();
    let program = r"C:\Tools\away-guard-hook.exe";

    assert!(matches!(hooks.plan_install(&env, &install(program, None)), Err(HookError::UnknownVersion(_))));
    assert!(matches!(hooks.plan_install(&env, &install(program, Some("2.2.0"))), Err(HookError::UnknownVersion(_))));

    let plan = hooks.plan_install(&env, &install(program, Some("2.1.309"))).unwrap();
    assert_eq!(std::fs::read_to_string(settings(&env)).unwrap(), original, "planning writes nothing");
    assert_eq!(plan.files.len(), 1);
    assert_eq!(plan.files[0].summary.len(), 3, "SessionStart, Stop, StopFailure: {:?}", plan.files[0].summary);
    plan.apply().unwrap();
    let after: Value = serde_json::from_str(&std::fs::read_to_string(settings(&env)).unwrap()).unwrap();
    assert_eq!(after["theme"], "dark");
    assert_eq!(after["hooks"]["Stop"][0]["hooks"][0]["command"], "theirs.exe");
    assert_eq!(after["hooks"]["Stop"][1]["hooks"][0], json!({"type": "command", "command": program, "args": ["--agent", "claude-code"], "timeout": 30}));
    assert_eq!(after["hooks"]["StopFailure"][0]["hooks"][0]["timeout"], 30);
    let text = std::fs::read_to_string(settings(&env)).unwrap();
    assert!(text.find("\"theme\"").unwrap() < text.find("\"autoCompactWindow\"").unwrap(), "key order kept");
    let backups: Vec<_> = std::fs::read_dir(settings(&env).parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".bak"))
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(std::fs::read_to_string(backups[0].path()).unwrap(), original);

    let installed = hooks.installed(&env, "away-guard-hook.exe").unwrap();
    let mut events: Vec<&str> = installed.iter().map(|h| h.native_event.as_str()).collect();
    events.sort();
    assert_eq!(events, ["SessionStart", "Stop", "StopFailure"]);
    assert!(hooks.plan_install(&env, &install(program, Some("2.1.309"))).unwrap().is_noop(), "installing twice changes nothing");

    // A plan goes stale when the user edits the file in between.
    let stale = hooks.plan_uninstall(&env, "away-guard-hook.exe").unwrap();
    std::fs::write(settings(&env), text.replace("dark", "light")).unwrap();
    assert!(matches!(stale.apply(), Err(HookError::Conflict(_))));
    std::fs::write(settings(&env), &text).unwrap();

    hooks.plan_uninstall(&env, "away-guard-hook.exe").unwrap().apply().unwrap();
    assert_eq!(std::fs::read_to_string(settings(&env)).unwrap(), original, "uninstall leaves exactly the user's file");
    assert!(hooks.plan_uninstall(&env, "away-guard-hook.exe").unwrap().is_noop());

    std::fs::write(settings(&env), "{ broken").unwrap();
    assert!(matches!(hooks.plan_install(&env, &install(program, Some("2.1.309"))), Err(HookError::UnknownFormat(_))));
}

#[test]
fn hook_input_becomes_the_unified_event() {
    let hooks = adapter().hooks().unwrap();
    let stop = hooks
        .parse(br#"{"session_id":"s1","transcript_path":"C:\\t.jsonl","cwd":"D:\\p","hook_event_name":"Stop","stop_hook_active":true,"last_assistant_message":"done"}"#)
        .unwrap();
    assert_eq!((stop.kind, stop.outcome.clone(), stop.continuing), (HookKind::TurnEnd, Some(TurnOutcome::Completed), Some(true)));
    assert_eq!(stop.cwd.as_deref(), Some(r"D:\p"));

    let failed = hooks
        .parse(br#"{"session_id":"s1","cwd":"D:\\p","hook_event_name":"StopFailure","error":"server_error","last_assistant_message":"API Error: Connection lost mid-response."}"#)
        .unwrap();
    let Some(TurnOutcome::ApiError { failure }) = failed.outcome else { panic!("{failed:?}") };
    assert_eq!((failure.class, failure.network, failure.code.as_str()), (FailureClass::Network, Some(true), "server_error"));

    let limited = hooks
        .parse(br#"{"session_id":"s1","hook_event_name":"StopFailure","error":"rate_limit","error_details":"429 Too Many Requests"}"#)
        .unwrap();
    let Some(TurnOutcome::ApiError { failure }) = limited.outcome else { panic!() };
    assert_eq!((failure.class, failure.network, failure.http_status), (FailureClass::RateLimit, Some(false), Some(429)));

    let start = hooks.parse(br#"{"session_id":"s1","hook_event_name":"SessionStart","source":"resume"}"#).unwrap();
    assert_eq!((start.kind, start.detail.as_deref()), (HookKind::SessionStart, Some("resume")));
    assert!(matches!(hooks.parse(br#"{"session_id":"s1","hook_event_name":"PreToolUse"}"#), Err(HookError::UnknownEvent(_))));
    assert!(matches!(hooks.parse(br#"{"hook_event_name":"Stop"}"#), Err(HookError::UnknownFormat(_))));
    assert!(matches!(hooks.parse(b"nope"), Err(HookError::UnknownFormat(_))));

    let reply = hooks.continue_reply("go on").unwrap();
    assert_eq!(serde_json::from_str::<Value>(&reply.stdout).unwrap(), json!({"decision": "block", "reason": "go on"}));
    assert_eq!(adapter().resume_args("s1", "go on", &Default::default()).unwrap(), ["--resume", "s1", "-p", "go on"]);

    let prompt = hooks
        .parse(br#"{"session_id":"s1","hook_event_name":"UserPromptSubmit","permission_mode":"acceptEdits","prompt":"secret"}"#)
        .unwrap();
    assert_eq!(prompt.permission_mode.as_deref(), Some("acceptEdits"));
}

fn line(args: &[&str]) -> Vec<String> {
    std::iter::once("claude.exe").chain(args.iter().copied()).map(str::to_owned).collect()
}

#[test]
fn a_person_ending_it_is_told_apart_from_a_run_that_just_finished() {
    let a = adapter();
    assert!(a.runs_in_a_window(&line(&[])));
    assert!(a.runs_in_a_window(&line(&["--model", "haiku", "--resume", "id"])));
    assert!(a.runs_in_a_window(&line(&["explain what -p does"])));
    assert!(!a.runs_in_a_window(&line(&["-p", "go"])));
    assert!(!a.runs_in_a_window(&line(&["--model", "haiku", "--print", "go"])));
    assert!(a.runs_in_a_window(&line(&["--", "-p"])), "a prompt, after the end of the options");

    for (reason, in_a_window, ended) in [
        ("prompt_input_exit", false, true),
        ("clear", false, true),
        ("logout", false, true),
        ("resume", false, true),
        ("other", true, true),
        ("other", false, false),
        ("something_new", true, false),
    ] {
        assert_eq!(a.ended_by_the_user(reason, in_a_window), ended, "{reason} {in_a_window}");
    }
}

#[test]
fn a_continuation_is_started_like_the_original_but_not_with_its_prompt() {
    let s = adapter().run_settings(&line(&[
        "-p",
        "--dangerously-skip-permissions",
        "--model",
        "claude-haiku-4-5",
        "--add-dir",
        r"D:\a",
        r"D:\b",
        "--session-id",
        "old",
        "--allowedTools=Write",
        "--strict-mcp-config",
        "--append-system-prompt",
        "be brief",
        "--output-format",
        "json",
        "the prompt itself",
    ]));
    assert_eq!(s.permission_mode.as_deref(), Some("bypassPermissions"));
    assert_eq!(s.model.as_deref(), Some("claude-haiku-4-5"));
    let carried = [
        "--add-dir",
        r"D:\a",
        r"D:\b",
        "--allowedTools=Write",
        "--strict-mcp-config",
        "--append-system-prompt",
        "be brief",
    ];
    assert_eq!(s.options, carried);
    let flags: Vec<&str> = ["--permission-mode", "bypassPermissions", "--model", "claude-haiku-4-5"].into_iter().chain(carried).collect();
    let mut resume = vec!["--resume", "s1"];
    resume.extend(&flags);
    resume.extend(["-p", "go on"]);
    assert_eq!(adapter().resume_args("s1", "go on", &s).unwrap(), resume);
    let mut fork = vec!["--resume", "s1", "--fork-session", "--session-id", "new"];
    fork.extend(&flags);
    fork.extend(["-p", "go on"]);
    assert_eq!(adapter().fork_args("s1", "new", "go on", &s).unwrap(), fork);
}

#[test]
fn the_default_mode_stays_default_and_an_unknown_mode_is_not_passed_on() {
    let args = |mode: &str| {
        let s = agents::RunSettings { permission_mode: Some(mode.to_owned()), ..Default::default() };
        adapter().resume_args("s1", "go on", &s).unwrap()
    };
    assert_eq!(args("default"), ["--resume", "s1", "--permission-mode", "default", "-p", "go on"]);
    assert_eq!(args("--dangerously-skip-permissions"), ["--resume", "s1", "-p", "go on"]);
    assert!(adapter().runs_unattended("bypassPermissions"));
    for mode in ["default", "acceptEdits", "plan", "something new"] {
        assert!(!adapter().runs_unattended(mode), "{mode}");
    }
}

#[test]
fn the_model_is_the_one_that_last_answered() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let assistant = |model: &str| json!({"type": "assistant", "message": {"role": "assistant", "model": model, "content": []}}).to_string();
    let refine = |model: Option<&str>, lines: &[String]| {
        std::fs::write(&path, lines.join("\n")).unwrap();
        let mut s = agents::RunSettings { model: model.map(str::to_owned), ..Default::default() };
        adapter().refine_from_transcript(&mut s, &path);
        s.model
    };
    // The first turn failed: only the error record exists, so the start option stands.
    assert_eq!(refine(Some("claude-haiku-4-5"), &[assistant("<synthetic>")]).as_deref(), Some("claude-haiku-4-5"));
    // An alias with a suffix keeps the suffix; the API only ever names the plain model.
    assert_eq!(refine(Some("opus[1m]"), &[assistant("claude-opus-5-5")]).as_deref(), Some("opus[1m]"));
    // Switched with /model after the start.
    assert_eq!(
        refine(Some("claude-haiku-4-5"), &[assistant("claude-haiku-4-5"), assistant("claude-sonnet-4-6"), assistant("<synthetic>")]).as_deref(),
        Some("claude-sonnet-4-6")
    );
    // No option and no change: nothing to pass, so a model set in the settings file still applies.
    assert_eq!(refine(None, &[assistant("claude-opus-5-5"), assistant("<synthetic>")]), None);
    assert_eq!(refine(None, &[assistant("claude-opus-5-5"), assistant("claude-haiku-4-5")]).as_deref(), Some("claude-haiku-4-5"));
    // No transcript: nothing to correct.
    std::fs::remove_file(&path).unwrap();
    let mut s = agents::RunSettings { model: Some("x".into()), ..Default::default() };
    adapter().refine_from_transcript(&mut s, &path);
    assert_eq!(s.model.as_deref(), Some("x"));
}
