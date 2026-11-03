//! Request handling without a transport: handshake, routing, the central permission check,
//! argument validation, the operation log and the catalog.

mod common;

use common::{AGENT_EXE, Env, UI_EXE, agent, agent_call, call, err_code, hello, raw_session, send, trusted};
use proto::catalog::HelloResult;
use proto::commands::COMMON_SHORT_NAMES;
use proto::{ErrorCode, Section, subcode};
use serde_json::{Value, json};

#[test]
fn first_request_must_be_the_handshake() {
    let env = Env::new();
    let d = env.start(daemon::modules::builtin());
    let mut s = raw_session(&d, AGENT_EXE);
    let resp = call(&d, &mut s, "status", json!({}));
    assert_eq!(err_code(&resp), (ErrorCode::BadRequest, Some(subcode::HANDSHAKE_REQUIRED.into())));
    hello(&d, &mut s);
    assert!(call(&d, &mut s, "status", json!({})).ok);
}

#[test]
fn handshake_negotiates_or_says_who_is_too_old() {
    let env = Env::new();
    let d = env.start(daemon::modules::builtin());

    let mut s = raw_session(&d, AGENT_EXE);
    let resp = send(&d, &mut s, 9, "core.hello", json!({"protocols": [0]}));
    assert_eq!(err_code(&resp), (ErrorCode::ProtocolVersion, Some(subcode::CLIENT_TOO_OLD.into())));
    let resp = send(&d, &mut s, 9, "core.hello", json!({"protocols": [7, 8]}));
    assert_eq!(err_code(&resp), (ErrorCode::ProtocolVersion, Some(subcode::DAEMON_TOO_OLD.into())));
    assert_eq!(resp.error.unwrap().details.unwrap()["daemon_max"], json!(1));

    // A newer client offering 1 and 2 gets 1.
    let resp = send(&d, &mut s, 2, "core.hello", json!({"protocols": [1, 2], "client": {"name": "new", "v9_field": 1}}));
    let h: HelloResult = serde_json::from_value(resp.into_result().unwrap()).unwrap();
    assert_eq!(h.protocol, 1);
    // After that, every request must use the agreed version.
    let resp = send(&d, &mut s, 2, "status", json!({}));
    assert_eq!(err_code(&resp).0, ErrorCode::ProtocolVersion);
    assert!(send(&d, &mut s, 1, "status", json!({})).ok);
}

#[test]
fn unknown_request_fields_are_ignored() {
    let env = Env::new();
    let d = env.start(daemon::modules::builtin());
    let mut s = agent(&d);
    let line = json!({"v": 1, "id": "x1", "cmd": "status", "args": {}, "added_in_v2": {"a": 1}}).to_string();
    let resp = d.handle_line(&mut s, line.as_bytes());
    assert!(resp.ok, "{resp:?}");
}

#[test]
fn malformed_and_unknown_requests_get_fixed_codes() {
    let env = Env::new();
    let d = env.start(daemon::modules::builtin());
    let mut s = agent(&d);
    assert_eq!(d.handle_line(&mut s, b"{not json").error.unwrap().code, ErrorCode::BadRequest);
    assert_eq!(d.handle_line(&mut s, br#"{"v":1,"cmd":"status"}"#).error.unwrap().code, ErrorCode::BadRequest);
    assert_eq!(err_code(&call(&d, &mut s, "frobnicate", json!({}))).0, ErrorCode::UnknownCommand);
    assert_eq!(err_code(&call(&d, &mut s, "task.frobnicate", json!({}))).0, ErrorCode::UnknownCommand);
}

#[test]
fn every_common_short_name_exists_and_routes() {
    let env = Env::new();
    let d = env.start(daemon::modules::builtin());
    let h = hello(&d, &mut raw_session(&d, AGENT_EXE));
    for short in COMMON_SHORT_NAMES {
        let c = h.command(short).unwrap_or_else(|| panic!("short name {short} missing"));
        assert!(c.callable && c.agent && !c.hidden, "{short}");
    }
    // Real modules answer through their short names.
    assert_eq!(agent_call(&d, "away", json!({})).into_result().unwrap()["mode"], "away");
    assert_eq!(agent_call(&d, "mode.back", json!({})).into_result().unwrap()["mode"], "normal");
    assert!(agent_call(&d, "tasks", json!({})).ok);
    // Short and canonical names reach the same handler (no such process or task here).
    for (cmd, args) in [
        ("protect", json!({"pid": 1})),
        ("renew", json!({"task_id": 1})),
        ("task.renew", json!({"task_id": 1})),
        ("unprotect", json!({"task_id": 1})),
        ("prioritize", json!({"task_id": 1})),
    ] {
        let resp = agent_call(&d, cmd, args);
        assert_eq!(err_code(&resp).0, ErrorCode::NotFound, "{cmd}");
        assert_eq!(resp.exit_code(), 16);
    }
    // Resource requests now return their state and measured result.
    let resp = agent_call(&d, "free", json!({"mb": 100}));
    assert!(resp.ok, "{resp:?}");
    assert_eq!(resp.result.unwrap()["state"], "needs_user");
    assert!(agent_call(&d, "status", json!({})).ok);
    assert!(agent_call(&d, "log", json!({"limit": 5})).ok);
}

#[test]
fn catalog_carries_sections_and_hides_the_handshake() {
    let env = Env::new();
    let d = env.start(daemon::modules::builtin());
    let h = hello(&d, &mut raw_session(&d, AGENT_EXE));
    let section = |m: &str| h.modules.iter().find(|x| x.name == m).unwrap().section;
    assert_eq!(section("core"), Some(Section::Common));
    assert_eq!(section("mode"), Some(Section::Usage));
    assert_eq!(section("power"), Some(Section::Usage));
    assert_eq!(section("network"), Some(Section::Usage));
    assert_eq!(section("task"), Some(Section::Usage));
    assert_eq!(section("resource"), Some(Section::Usage));
    assert_eq!(section("scope"), Some(Section::AccountsStorage));
    assert_eq!(section("permission"), Some(Section::Protection));
    assert_eq!(section("example"), Some(Section::Common));
    assert_eq!(h.command("protect").unwrap().section, Some(Section::Usage));
    assert!(h.command("core.hello").unwrap().hidden);
    // Off by default.
    assert!(!h.modules.iter().find(|m| m.name == "scope").unwrap().enabled);
    assert!(!h.command("example.echo").unwrap().module_enabled);
    assert_eq!(h.caller.kind, "agent");
}

#[test]
fn page_only_commands_are_checked_centrally() {
    let env = Env::new();
    let d = env.start(daemon::modules::builtin());

    let h = hello(&d, &mut raw_session(&d, AGENT_EXE));
    let set = h.command("permission.set").unwrap();
    assert!(!set.agent && !set.callable);
    let args = json!({"program": "D:\\Apps\\x.exe", "class": "never"});
    let resp = agent_call(&d, "permission.set", args.clone());
    assert_eq!(err_code(&resp), (ErrorCode::Denied, Some(subcode::AGENT_NOT_ALLOWED.into())));
    assert_eq!(resp.exit_code(), 15);

    // The page passes the check and can edit the allowlist.
    let h = hello(&d, &mut raw_session(&d, UI_EXE));
    assert_eq!(h.caller.kind, "trusted");
    assert!(h.command("permission.set").unwrap().callable);
    let resp = call(&d, &mut trusted(&d), "permission.set", args);
    assert!(resp.ok, "{resp:?}");

    let log = d.store().oplog_recent(10).unwrap();
    let kinds: Vec<_> = log.iter().filter(|r| r.cmd == "permission.set").map(|r| r.caller_kind.clone().unwrap()).collect();
    assert_eq!(kinds, ["trusted", "agent"]);
}

#[test]
fn arguments_are_validated_against_the_declaration() {
    let env = Env::new();
    let d = env.start(daemon::modules::builtin());
    let invalid = |cmd: &str, args: Value| {
        let resp = agent_call(&d, cmd, args.clone());
        assert_eq!(err_code(&resp).0, ErrorCode::InvalidArgs, "{cmd} {args}");
        assert_eq!(resp.exit_code(), 12);
    };
    invalid("renew", json!({}));
    invalid("renew", json!({"task_id": "seven"}));
    invalid("protect", json!({"pid": -1}));
    invalid("protect", json!({"deps": "wsl"}));
    invalid("protect", json!({"no_such_arg": 1}));
    // Not an object at all: the envelope is wrong.
    assert_eq!(err_code(&agent_call(&d, "status", json!([1, 2]))).0, ErrorCode::BadRequest);
    // null counts as absent.
    assert!(agent_call(&d, "tasks", json!({"all": null})).ok);
}

#[test]
fn operation_log_records_who_asked() {
    let env = Env::new();
    let d = env.start(daemon::modules::builtin());
    let mut s = agent(&d);
    assert_eq!(err_code(&call(&d, &mut s, "away", json!({"preset": "night"}))).0, ErrorCode::InvalidArgs);
    assert!(call(&d, &mut s, "status", json!({})).ok);
    assert_eq!(err_code(&call(&d, &mut s, "nope", json!({}))).0, ErrorCode::UnknownCommand);

    let log = d.store().oplog_recent(10).unwrap();
    // Successful read-only requests (hello, status) stay out of the log.
    let cmds: Vec<&str> = log.iter().map(|r| r.cmd.as_str()).collect();
    assert_eq!(cmds, ["nope", "mode.away"]);
    let away = &log[1];
    assert_eq!(away.client_pid, Some(4242));
    assert_eq!(away.client_exe.as_deref(), Some(AGENT_EXE));
    assert_eq!(away.client_name.as_deref(), Some("test"));
    assert_eq!(away.caller_kind.as_deref(), Some("agent"));
    assert_eq!(away.module.as_deref(), Some("mode"));
    assert_eq!(away.args, Some(json!({"preset": "night"})));
    assert_eq!(away.error_code.as_deref(), Some("INVALID_ARGS"));
    assert_eq!(away.subcode.as_deref(), Some("mode.UNKNOWN_PRESET"));

    let entries = agent_call(&d, "log", json!({})).into_result().unwrap();
    assert_eq!(entries["entries"][0]["cmd"], "nope");
}

#[test]
fn caller_exe_comes_from_the_system_layer() {
    let env = Env::new();
    env.system.processes.lock().unwrap().insert(77, UI_EXE.into());
    let d = env.start(daemon::modules::builtin());
    assert_eq!(d.session(Some(77)).caller.kind, daemon::CallerKind::Trusted);
    assert_eq!(d.session(Some(78)).caller.kind, daemon::CallerKind::Agent);
    assert_eq!(d.session(None).caller.kind, daemon::CallerKind::Agent);
}

#[test]
fn status_reports_modules_by_section_and_config_warnings() {
    let env = Env::new();
    env.write_config(json!({"modules": {"nonexistent": {"enabled": true}}}));
    let d = env.start(daemon::modules::builtin());
    let st = agent_call(&d, "status", json!({})).into_result().unwrap();
    assert_eq!(st["daemon"]["protocol"], json!({"min": 1, "max": 1}));
    assert!(st["daemon"]["sqlite"].as_str().unwrap().starts_with("winsqlite3.dll 3."));
    assert!(st["daemon"]["config_warnings"][0].as_str().unwrap().contains("nonexistent"));
    let task = common::module_status(&st, "task");
    assert_eq!(task["section"], "usage");
    assert_eq!(task["health"], "running");
    assert_eq!(task["schema_version"], 2);
    assert_eq!(task["state"]["implemented"], true);
    assert_eq!(common::module_status(&st, "scope")["health"], "disabled");
}
