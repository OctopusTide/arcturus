//! Codex adapter against synthetic rollouts and hook files in a temporary folder (shaped like
//! the real 0.163/0.165 rollouts on this machine; nothing real is read).

use std::io::Write;
use std::path::{Path, PathBuf};

use agents::hooks::{HookError, HookInstall, HookKind, TurnOutcome};
use agents::model::{Encoding, FileOp, ReadOptions, Record, Role, Shell, Stop};
use agents::{Adapter, Env};
use serde_json::{Value, json};

const TID: &str = "01a18f48-65c4-7a83-ada9-5fb70f69a32e";

fn adapter() -> &'static dyn Adapter {
    agents::get("codex").unwrap()
}

fn line(ordinal: u64, ty: &str, payload: Value) -> String {
    json!({"timestamp": "2026-10-31T05:03:50.010Z", "ordinal": ordinal, "type": ty, "payload": payload}).to_string()
}

fn meta(version: &str) -> String {
    line(0, "session_meta", json!({"session_id": TID, "id": TID, "timestamp": "2026-10-31T05:03:49.679Z",
        "cwd": r"C:\work", "originator": "codex_exec", "cli_version": version, "source": "exec",
        "base_instructions": {"text": "long instructions"}}))
}

fn item(ordinal: u64, item: Value) -> String {
    line(ordinal, "event_msg", json!({"type": "item_completed", "thread_id": TID, "turn_id": "t", "item": item}))
}

fn write_lines(path: &Path, lines: &[String]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn rollout(home: &Path) -> PathBuf {
    home.join(".codex").join("sessions").join("2026").join("10").join("30").join(format!("rollout-2026-10-30T17-07-28-{TID}.jsonl"))
}

fn read_all(file: &agents::TranscriptFile, from: u64) -> (Vec<Record>, agents::ReadOutcome) {
    let mut got = Vec::new();
    let out = adapter().read(file, from, ReadOptions::default(), &mut |r| got.push(r)).unwrap();
    (got, out)
}

#[test]
fn lists_plain_compressed_and_archived_rollouts() {
    let dir = tempfile::tempdir().unwrap();
    let env = Env::with_home(dir.path());
    write_lines(&rollout(dir.path()), &[]);
    let day = rollout(dir.path()).parent().unwrap().to_path_buf();
    std::fs::write(day.join("rollout-2026-08-20T21-00-51-01a0227a-0a1d-7271-b631-bdcb0c284ff0.jsonl.zst"), b"\x28\xb5\x2f\xfd").unwrap();
    std::fs::write(day.join("notes.txt"), "").unwrap();
    let archived = dir.path().join(".codex").join("archived_sessions");
    write_lines(&archived.join("rollout-2026-08-21T01-00-00-01a0227a-0a1d-7271-b631-bdcb0c284ff1.jsonl"), &[]);
    let mut files = adapter().transcripts(&env).unwrap();
    files.sort_by(|a, b| a.conversation_id.cmp(&b.conversation_id));
    let got: Vec<(&str, Encoding)> = files.iter().map(|f| (f.conversation_id.as_str(), f.encoding)).collect();
    assert_eq!(
        got,
        [
            ("01a0227a-0a1d-7271-b631-bdcb0c284ff0", Encoding::Zstd),
            ("01a0227a-0a1d-7271-b631-bdcb0c284ff1", Encoding::Plain),
            (TID, Encoding::Plain)
        ]
    );
    let (_, out) = read_all(&files[0], 0);
    assert_eq!(out.stop, Some(Stop::Compressed));
}

#[test]
fn reads_commands_file_changes_and_messages() {
    let dir = tempfile::tempdir().unwrap();
    let env = Env::with_home(dir.path());
    let path = rollout(dir.path());
    write_lines(
        &path,
        &[
            meta("0.165.0-alpha.1.2"),
            line(1, "turn_context", json!({"turn_id": "t", "cwd": r"C:\work\sub", "model": "gpt"})),
            line(2, "response_item", json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]})),
            line(3, "response_item", json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "clone it"}]})),
            line(4, "response_item", json!({"type": "custom_tool_call", "call_id": "c1", "name": "exec", "input": "await tools.run()", "status": "completed"})),
            item(5, json!({"type": "CommandExecution", "id": "exec-1", "process_id": "1",
                "command": [r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe", "-Command", "git clone https://x/y.git"],
                "cwd": "file:///C:/work/sub%20dir", "parsed_cmd": [], "source": "unified_exec_startup", "status": "completed",
                "aggregated_output": "lots of output", "exit_code": 0})),
            item(6, json!({"type": "FileChange", "id": "exec-2", "status": "completed", "stdout": "", "stderr": "", "changes": {
                r"C:\work\new.txt": {"type": "add", "unified_diff": "+x", "move_path": null},
                r"C:\work\old.txt": {"type": "update", "unified_diff": "-a\n+b", "move_path": r"C:\work\renamed.txt"},
                r"C:\work\gone.txt": {"type": "delete", "unified_diff": "", "move_path": null}}})),
            item(7, json!({"type": "FileChange", "id": "exec-3", "status": "failed", "changes": {r"C:\work\never.txt": {"type": "add"}}})),
            line(8, "response_item", json!({"type": "custom_tool_call_output", "call_id": "c1", "output": "ok"})),
            line(9, "response_item", json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]})),
            line(10, "event_msg", json!({"type": "token_count", "info": null})),
            item(11, json!({"type": "Reasoning", "id": "r", "summary_text": []})),
        ],
    );
    let file = adapter().transcripts(&env).unwrap().remove(0);
    let (got, out) = read_all(&file, 0);
    assert_eq!(out.stop, None);
    assert_eq!(out.offset, file.size);
    assert_eq!(out.version.as_deref(), Some("0.165.0-alpha.1.2"));
    assert_eq!(out.ignored.get("event_msg/token_count"), Some(&1));
    assert_eq!(out.ignored.get("item_completed/Reasoning"), Some(&1));
    assert_eq!(out.ignored.get("response_item/message/developer"), Some(&1));

    let sessions: Vec<Option<String>> =
        got.iter().filter_map(|r| if let Record::Session { cwd, .. } = r { Some(cwd.clone()) } else { None }).collect();
    assert_eq!(sessions, [Some(r"C:\work".to_owned()), Some(r"C:\work\sub".to_owned())]);
    let cmds: Vec<(Shell, String, Option<String>)> = got
        .iter()
        .filter_map(|r| if let Record::Command { shell, text, cwd, .. } = r { Some((shell.clone(), text.clone(), cwd.clone())) } else { None })
        .collect();
    assert_eq!(cmds, [(Shell::PowerShell, "git clone https://x/y.git".to_owned(), Some(r"C:\work\sub dir".to_owned()))]);
    let mut writes: Vec<(String, FileOp)> =
        got.iter().filter_map(|r| if let Record::FileWrite { path, op, .. } = r { Some((path.clone(), op.clone())) } else { None }).collect();
    writes.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        writes,
        [
            (r"C:\work\gone.txt".to_owned(), FileOp::Delete),
            (r"C:\work\new.txt".to_owned(), FileOp::Create),
            (r"C:\work\renamed.txt".to_owned(), FileOp::Move { from: r"C:\work\old.txt".to_owned() }),
        ],
        "a failed change writes nothing"
    );
    let roles: Vec<Role> = got.iter().filter_map(|r| if let Record::Message { role, .. } = r { Some(*role) } else { None }).collect();
    assert_eq!(roles, [Role::User, Role::Assistant]);
    assert!(got.iter().any(|r| matches!(r, Record::ToolCall { name, .. } if name == "exec")));
    assert!(got.iter().any(|r| matches!(r, Record::ToolResult { call_id, is_error: None, .. } if call_id == "c1")));

    // Incremental: later passes still check the header's version.
    let first = out.offset;
    write_lines(&path, &[item(12, json!({"type": "CommandExecution", "id": "exec-9", "command": ["git", "status"], "status": "completed"}))]);
    let file = adapter().transcripts(&env).unwrap().remove(0);
    let (got, out) = read_all(&file, first);
    assert_eq!(out.version.as_deref(), Some("0.165.0-alpha.1.2"));
    assert!(
        matches!(&got[..], [Record::Command { shell: Shell::Direct, text, cwd: None, .. }] if text == "git status"),
        "mid-file, a command without its own cwd gets none rather than a guess: {got:?}"
    );
}

#[test]
fn unknown_header_or_version_stops() {
    let dir = tempfile::tempdir().unwrap();
    let env = Env::with_home(dir.path());
    let path = rollout(dir.path());
    write_lines(&path, &[meta("0.170.0"), line(1, "turn_context", json!({"cwd": "x"}))]);
    let file = adapter().transcripts(&env).unwrap().remove(0);
    let (got, out) = read_all(&file, 0);
    assert!(got.is_empty());
    assert!(matches!(&out.stop, Some(Stop::UnknownVersion { version, at: 0, .. }) if version == "0.170.0"), "{:?}", out.stop);
    assert_eq!(out.offset, 0);
    let (_, later) = read_all(&file, 100);
    assert!(matches!(later.stop, Some(Stop::UnknownVersion { .. })), "a later pass checks the header too");

    std::fs::remove_file(&path).unwrap();
    write_lines(&path, &[line(0, "response_item", json!({"type": "message", "role": "user", "content": []}))]);
    let file = adapter().transcripts(&env).unwrap().remove(0);
    let (_, out) = read_all(&file, 0);
    assert!(matches!(out.stop, Some(Stop::UnknownFormat { at: 0, .. })), "{:?}", out.stop);
}

#[test]
fn hooks_plan_a_new_file_and_parse_stop() {
    let dir = tempfile::tempdir().unwrap();
    let env = Env::with_home(dir.path());
    let hooks = adapter().hooks().unwrap();
    std::fs::create_dir_all(dir.path().join(".codex")).unwrap();
    std::fs::write(dir.path().join(".codex").join("config.toml"), "[features]\nhooks = false\n").unwrap();
    let req = HookInstall {
        program: PathBuf::from(r"C:\Tools\away-guard-hook.exe"),
        kinds: vec![(HookKind::TurnEnd, 30)],
        agent_version: Some("0.165.0-alpha.1.2".to_owned()),
    };
    let plan = hooks.plan_install(&env, &req).unwrap();
    assert_eq!(plan.files[0].before, None);
    assert!(plan.notes.iter().any(|n| n.contains("trust")));
    assert!(plan.notes.iter().any(|n| n.contains("hooks = false")));
    plan.apply().unwrap();
    let written: Value = serde_json::from_str(&std::fs::read_to_string(hooks.config_file(&env)).unwrap()).unwrap();
    assert_eq!(
        written,
        json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": r"C:\Tools\away-guard-hook.exe --agent codex", "timeout": 30}]}]}})
    );
    assert_eq!(hooks.installed(&env, "away-guard-hook.exe").unwrap().len(), 1);
    let old_version = HookInstall { kinds: vec![(HookKind::SessionStart, 5)], agent_version: Some("0.1.0".to_owned()), ..req.clone() };
    assert!(matches!(hooks.plan_install(&env, &old_version), Err(HookError::UnknownVersion(_))));
    hooks.plan_uninstall(&env, "away-guard-hook.exe").unwrap().apply().unwrap();
    assert_eq!(std::fs::read_to_string(hooks.config_file(&env)).unwrap(), "{}\n");

    let stop = hooks
        .parse(br#"{"session_id":"s","transcript_path":null,"cwd":"C:\\w","hook_event_name":"Stop","model":"m","turn_id":"t","stop_hook_active":false,"last_assistant_message":"x"}"#)
        .unwrap();
    assert_eq!((stop.kind, stop.outcome, stop.continuing, stop.transcript_path), (HookKind::TurnEnd, Some(TurnOutcome::Unknown), Some(false), None));
    assert!(matches!(hooks.parse(br#"{"session_id":"s","hook_event_name":"StopFailure"}"#), Err(HookError::UnknownEvent(_))));
    assert_eq!(adapter().resume_args("s", "go", &Default::default()).unwrap(), ["exec", "resume", "s", "go"]);
}
