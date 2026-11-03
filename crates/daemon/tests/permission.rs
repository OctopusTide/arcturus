mod common;
use common::{Env, agent_call, call, trusted, wait_events};
use daemon::arbitration::ProgramRule;
use proto::ErrorCode;
use serde_json::{Value, json};
fn ui(d: &daemon::dispatch::Dispatcher, cmd: &str, args: Value) -> Value {
    call(d, &mut trusted(d), cmd, args).into_result().unwrap()
}
fn list(d: &daemon::dispatch::Dispatcher) -> Value {
    agent_call(d, "permission.list", json!({})).into_result().unwrap()
}

#[test]
fn full_paths_immutable_entries_and_removable_defaults_persist() {
    let env = Env::new();
    env.system.rules.lock().unwrap().extend([
        ProgramRule {
            path: r"C:\Windows\explorer.exe".into(),
            signer: Some("Microsoft Corporation".into()),
            source: "builtin".into(),
        },
        ProgramRule { path: r"D:\Apps\Code.exe".into(), signer: None, source: "default".into() },
    ]);
    let d = env.start(daemon::modules::builtin());
    wait_events(&d);
    assert_eq!(
        call(&d, &mut trusted(&d), "permission.set", json!({"program":r"c:\windows\EXPLORER.exe","class":"may_end"}))
            .error
            .unwrap()
            .code,
        ErrorCode::Denied
    );
    ui(&d, "permission.set", json!({"program":r"D:\Apps\Code.exe","class":"may_end"}));
    for path in ["x.exe", r"D:relative.exe", r"D:\Apps\..\x.exe", r"D:\Apps\*.exe"] {
        assert_eq!(
            call(&d, &mut trusted(&d), "permission.set", json!({"program":path,"class":"never"})).error.unwrap().code,
            ErrorCode::InvalidArgs
        );
    }
    ui(&d, "permission.set", json!({"program":"D:/Apps/custom.exe","class":"never"}));
    d.registry().stop_all();
    drop(d);
    let d = env.start(daemon::modules::builtin());
    wait_events(&d);
    let v = list(&d);
    let entries = v["entries"].as_array().unwrap();
    assert_eq!(entries.iter().find(|r| r["program"] == r"d:\apps\code.exe").unwrap()["protected"], false);
    assert_eq!(entries.iter().find(|r| r["program"] == r"d:\apps\custom.exe").unwrap()["protected"], true);
    d.registry().stop_all();
}

#[test]
fn agent_requests_have_no_effect_until_user_approves_and_are_audited() {
    let env = Env::new();
    let d = env.start(daemon::modules::builtin());
    wait_events(&d);
    let args = json!({"program":r"D:\Apps\custom.exe","class":"never","reason":"Keep unsaved work"});
    let r = agent_call(&d, "permission.request", args).into_result().unwrap();
    assert_eq!(r["effective"], false);
    assert_eq!(list(&d)["entries"].as_array().unwrap().len(), 0);
    let id = r["request_id"].clone();
    assert_eq!(
        agent_call(&d, "permission.review", json!({"request_id":id,"approve":true})).error.unwrap().code,
        ErrorCode::Denied
    );
    ui(&d, "permission.review", json!({"request_id":id,"approve":true}));
    assert_eq!(list(&d)["entries"][0]["protected"], true);
    assert_eq!(
        call(&d, &mut trusted(&d), "permission.review", json!({"request_id":id,"approve":true})).error.unwrap().code,
        ErrorCode::NotFound
    );
    let log = d.store().oplog_recent(20).unwrap();
    assert!(log.iter().any(|r| r.cmd == "permission.request"));
    d.registry().stop_all();
}

#[test]
fn path_identity_is_not_a_name_rule_and_signature_mismatch_fails_closed() {
    let env = Env::new();
    env.system.spawn(100, 0, "Code.exe").set_exe(100, Some(r"D:\Apps\Code.exe"));
    let info = env.config.sys.system.process_info(100).unwrap();
    let rules = vec![ProgramRule {
        path: r"d:\apps\code.exe".into(),
        signer: Some("Microsoft Corporation".into()),
        source: "default".into(),
    }];
    assert_eq!(
        daemon::modules::permission::blocked(env.config.sys.system.as_ref(), &info, &rules).as_deref(),
        Some("signature_unverified")
    );
    env.system.set_signer(r"D:\Apps\Code.exe", "Microsoft Corporation");
    assert_eq!(
        daemon::modules::permission::blocked(env.config.sys.system.as_ref(), &info, &rules).as_deref(),
        Some("default")
    );
    env.system.set_exe(100, Some(r"D:\Other\Code.exe"));
    let info = env.config.sys.system.process_info(100).unwrap();
    assert!(daemon::modules::permission::blocked(env.config.sys.system.as_ref(), &info, &rules).is_none());
}
