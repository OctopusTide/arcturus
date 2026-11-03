//! The files module against synthetic Claude Code and Codex transcripts in a temporary home
//! folder: who owns which path, sizes without double counting, shared caches, incremental
//! reading, unknown versions, and that no conversation text reaches the database.

mod common;

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use proto::ErrorCode;
use serde_json::{Value, json};

const A: &str = "aaaaaaaa-1111-4111-8111-000000000001";
const C: &str = "cccccccc-3333-4333-8333-000000000003";
const D: &str = "aaaaaaaa-2222-4222-8222-000000000004";
const B: &str = "01a18f48-65c4-7a83-ada9-5fb70f69a32e";
const TS: &str = "2026-01-02T03:04:05.000Z";
/// Far after any folder made by this test: the folder "existed before" the event.
const LATE: &str = "2099-01-01T00:00:00.000Z";

fn cc(sid: &str, version: &str, ts: &str, ty: &str, cwd: &Path, message: Value, extra: Value) -> String {
    let mut v = json!({"parentUuid": null, "isSidechain": false, "message": message, "type": ty, "uuid": "u",
                       "timestamp": ts, "cwd": cwd, "sessionId": sid, "version": version});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v.to_string()
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]})
}

fn tool_result(id: &str) -> Value {
    json!({"role": "user", "content": [{"tool_use_id": id, "type": "tool_result", "content": "output", "is_error": false}]})
}

fn cx(ordinal: u64, ty: &str, payload: Value) -> String {
    cx_at(TS, ordinal, ty, payload)
}

fn cx_at(ts: &str, ordinal: u64, ty: &str, payload: Value) -> String {
    json!({"timestamp": ts, "ordinal": ordinal, "type": ty, "payload": payload}).to_string()
}

fn append(path: &Path, lines: &[String]) -> u64 {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    let mut n = 0;
    for l in lines {
        writeln!(f, "{l}").unwrap();
        n += l.len() as u64 + 1;
    }
    n
}

fn file(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// Sets a folder's creation time (as if it had been made at `iso`).
fn set_created(dir: &Path, iso: &str) {
    use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
    const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let f = std::fs::OpenOptions::new().access_mode(FILE_WRITE_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(dir).unwrap();
    let at = std::time::UNIX_EPOCH + Duration::from_millis(agents::model::parse_utc_ms(iso).unwrap() as u64);
    f.set_times(std::fs::FileTimes::new().set_created(at)).unwrap();
}

fn scan(d: &daemon::dispatch::Dispatcher) -> Value {
    scan_within(d, Duration::from_secs(60))
}

fn scan_within(d: &daemon::dispatch::Dispatcher, limit: Duration) -> Value {
    let r = agent_call(d, "files.scan", json!({})).into_result().unwrap();
    assert_eq!(r["started"], true, "{r}");
    let deadline = Instant::now() + limit;
    loop {
        let s = agent_call(d, "files.scans", json!({})).into_result().unwrap();
        if s["progress"]["running"] == false && s["scans"][0]["ended_at"].is_i64() {
            return s["scans"][0].clone();
        }
        assert!(Instant::now() < deadline, "scan did not finish: {s}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn transcripts_attribute_paths_to_conversations() {
    let env = Env::new();
    let home = env.dir.path().join("home");
    let work = env.dir.path().join("work");
    let w = |p: &str| work.join(p);
    // What is on disk: the files the conversations made.
    file(&w("a.txt"), 10);
    file(&w(r"r\f1.bin"), 100);
    file(&w(r"r\new.py"), 5);
    file(&w("out.tar"), 20);
    file(&w(r"old\big.bin"), 1000);
    // Made by A's write of proj\notes.md (Write creates missing folders); a program then
    // filled it.
    file(&w(r"proj\notes.md"), 2);
    file(&w(r"proj\big.bin"), 50);
    file(&w(r"proj\b.txt"), 4);
    file(&w(r"proj\b2.txt"), 4);
    set_created(&w("proj"), TS);
    file(&home.join(r"AppData\Local\pip\Cache\wheel.whl"), 7);

    let projects = home.join(r".claude\projects\D--w");
    let main = projects.join(format!("{A}.jsonl"));
    let v = "2.1.309";
    append(
        &main,
        &[
            cc(A, v, TS, "user", &work, json!({"role": "user", "content": "SECRET-MESSAGE-TEXT please"}), json!({})),
            cc(A, v, TS, "assistant", &work, tool_use("t1", "Write", json!({"file_path": w("a.txt"), "content": "SECRET-CONTENT"})), json!({})),
            cc(A, v, TS, "user", &work, tool_result("t1"), json!({"toolUseResult": {"type": "create", "filePath": w("a.txt"), "content": "SECRET-CONTENT"}})),
            cc(A, v, TS, "assistant", &work, tool_use("t2", "PowerShell", json!({"command": "git clone https://me:ghp_SECRETTOKEN0123456789abcdef@github.com/o/r.git"})), json!({})),
            cc(A, v, TS, "user", &work, tool_result("t2"), json!({"toolUseResult": {"stdout": "", "stderr": "", "interrupted": false}})),
            cc(A, v, TS, "assistant", &work, tool_use("t3", "Bash", json!({"command": "pip install -r req.txt"})), json!({})),
            cc(A, v, LATE, "assistant", &work, tool_use("t4", "Bash", json!({"command": "mkdir -p old"})), json!({})),
            cc(A, v, TS, "assistant", &work, tool_use("t6", "Write", json!({"file_path": w(r"proj\notes.md")})), json!({})),
            cc(A, v, TS, "user", &work, tool_result("t6"), json!({"toolUseResult": {"type": "create", "filePath": w(r"proj\notes.md")}})),
        ],
    );
    // An agent version the adapter was not tested with: stopped, nothing attributed.
    append(
        &projects.join(format!("{C}.jsonl")),
        &[
            cc(C, "9.9.9", TS, "assistant", &work, tool_use("t1", "Write", json!({"file_path": w("c.txt")})), json!({})),
            cc(C, "9.9.9", TS, "user", &work, tool_result("t1"), json!({"toolUseResult": {"type": "create", "filePath": w("c.txt")}})),
        ],
    );
    // Later `mkdir -p` of A's clone: does not take it over (the folder existed by then).
    append(
        &projects.join(format!("{D}.jsonl")),
        &[
            cc(D, v, TS, "user", &work, json!({"role": "user", "content": "hi"}), json!({})),
            cc(D, v, LATE, "assistant", &work, tool_use("t1", "Bash", json!({"command": "mkdir -p r"})), json!({})),
        ],
    );
    let rollout = home.join(format!(r".codex\sessions\2026\10\30\rollout-2026-10-30T17-07-28-{B}.jsonl"));
    let item_at = |ts: &str, o: u64, item: Value| cx_at(ts, o, "event_msg", json!({"type": "item_completed", "thread_id": B, "turn_id": "t", "item": item}));
    let item = |o: u64, item: Value| item_at(TS, o, item);
    append(
        &rollout,
        &[
            cx(0, "session_meta", json!({"session_id": B, "id": B, "timestamp": TS, "cwd": work, "originator": "codex_exec", "cli_version": "0.165.0-alpha.1.2", "source": "exec"})),
            cx(1, "turn_context", json!({"turn_id": "t", "cwd": work, "model": "gpt"})),
            item(2, json!({"type": "FileChange", "id": "e1", "status": "completed", "changes": {w(r"r\new.py").to_string_lossy(): {"type": "add", "unified_diff": "+SECRET-DIFF"}}})),
            item(3, json!({"type": "CommandExecution", "id": "e2", "command": ["powershell.exe", "-Command", "scp box:/x/out.tar ."], "cwd": work, "status": "completed", "exit_code": 0})),
            // B made more in the folder A's write made (five minutes later) than A did: the
            // folder is shared, and what nobody made explicitly in it is unattributed.
            item_at("2026-01-02T03:09:05.000Z", 4, json!({"type": "FileChange", "id": "e3", "status": "completed", "changes": {
                w(r"proj\b.txt").to_string_lossy(): {"type": "add"}, w(r"proj\b2.txt").to_string_lossy(): {"type": "add"}}})),
        ],
    );

    let d = env.start(daemon::modules::builtin());
    let settings = json!({"settings": {"home": home.to_string_lossy(), "auto_scan": false}});
    call(&d, &mut trusted(&d), "files.configure", settings).into_result().unwrap();
    let first = scan(&d);
    assert_eq!(first["outcome"], "done", "{first}");
    assert_eq!(first["totals"]["transcripts"], 4);
    assert_eq!(first["totals"]["stopped"], 1);
    assert_eq!((first["totals"]["folders"].as_u64(), first["totals"]["reowned"].as_u64()), (Some(1), Some(1)), "{first}");

    // A: its files and the clone minus B's file inside it; `old` existed before `mkdir`;
    // what is left of `proj` (both put files there) belongs to nobody.
    let top = agent_call(&d, "files.top", json!({})).into_result().unwrap();
    let items = top["items"].as_array().unwrap();
    assert_eq!((items[0]["conversation_id"].as_str(), items[0]["bytes"].as_i64()), (Some(A), Some(112)), "{top}");
    assert_eq!((items[1]["conversation_id"].as_str(), items[1]["bytes"].as_i64()), (Some(B), Some(33)), "{top}");
    assert_eq!(items[1]["downloaded_bytes"], 20);
    assert_eq!((top["total"]["bytes"].as_i64(), top["total"]["unattributed_bytes"].as_i64()), (Some(145), Some(50)));
    assert!(top["shared_folders"][0]["path"].as_str().unwrap().ends_with("proj"), "{top}");
    let conv = agent_call(&d, "files.conversation", json!({"id": "aaaaaaaa-1111"})).into_result().unwrap();
    let paths = conv["paths"].as_array().unwrap();
    let at = |end: &str| paths.iter().find(|p| p["path"].as_str().unwrap().ends_with(end)).unwrap_or_else(|| panic!("{end}: {conv}"));
    let old = at("old");
    assert_eq!((old["pre_existing"].as_bool(), old["bytes"].as_i64(), old["counted"].as_i64()), (Some(true), Some(1000), Some(0)));
    let proj = at("proj");
    assert_eq!((proj["how"].as_str(), proj["counted"].as_i64(), proj["note"].as_str()), (Some("created_folder"), Some(0), Some("shared")));
    assert_eq!(proj["unattributed"], 50);
    assert_eq!((at(r"\r")["how"].as_str(), at(r"\r")["command"].as_str()), (Some("cloned"), Some("git clone https://***@github.com/o/r.git")));
    assert_eq!(conv["caches"][0]["cache"], "pip");
    assert_eq!(conv["cwd"], work.to_string_lossy().as_ref());

    // Downloads with redacted commands, for agents.
    let dl = agent_call(&d, "files.downloads", json!({"id": A})).into_result().unwrap();
    assert_eq!(dl["downloads"][0]["how"], "cloned");
    assert_eq!(dl["downloads"][0]["command"], "git clone https://***@github.com/o/r.git");
    let dl = agent_call(&d, "files.downloads", json!({"id": B})).into_result().unwrap();
    assert_eq!(dl["downloads"][0]["source"], "box:/x/out.tar");
    let found = agent_call(&d, "files.find", json!({"query": "OUT.TAR"})).into_result().unwrap();
    assert_eq!(found["items"][0]["owner"]["conversation_id"], B);
    let caches = agent_call(&d, "files.caches", json!({})).into_result().unwrap();
    let pip = caches["caches"].as_array().unwrap().iter().find(|c| c["cache"] == "pip").unwrap();
    assert_eq!((pip["bytes"].as_i64(), pip["conversations"].as_i64()), (Some(7), Some(1)));

    // Ids: a prefix shared by two conversations is refused; unknown ids are not found.
    let r = agent_call(&d, "files.conversation", json!({"id": "aaaaaaaa"}));
    assert_eq!(err_code(&r), (ErrorCode::InvalidArgs, Some("files.AMBIGUOUS_CONVERSATION".into())));
    let r = agent_call(&d, "files.downloads", json!({"id": C}));
    assert_eq!(err_code(&r), (ErrorCode::NotFound, Some("files.NO_SUCH_CONVERSATION".into())));

    // Only what was added is read next time.
    file(&w("later.txt"), 3);
    let added = append(
        &main,
        &[
            cc(A, v, TS, "assistant", &work, tool_use("t5", "Write", json!({"file_path": w("later.txt")})), json!({})),
            cc(A, v, TS, "user", &work, tool_result("t5"), json!({"toolUseResult": {"type": "create", "filePath": w("later.txt")}})),
        ],
    );
    let second = scan(&d);
    assert_eq!(second["totals"]["read_bytes"].as_u64(), Some(added), "{second}");
    let conv = agent_call(&d, "files.conversation", json!({"id": A})).into_result().unwrap();
    assert_eq!((conv["owned"]["bytes"].as_i64(), conv["file_writes"].as_i64()), (Some(115), Some(3)), "{conv}");

    // Paths, sizes and redacted commands only.
    drop(d);
    for entry in std::fs::read_dir(env.dir.path()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // The -wal / -shm files may go away while the database closes.
        if name.starts_with(proto::names::DB_FILE)
            && let Ok(bytes) = std::fs::read(entry.path())
        {
            let text = String::from_utf8_lossy(&bytes);
            for secret in ["SECRET-MESSAGE-TEXT", "SECRET-CONTENT", "SECRET-DIFF", "ghp_SECRETTOKEN"] {
                assert!(!text.contains(secret), "{secret} reached {name}");
            }
        }
    }
}

/// Reads this machine's real transcripts (read-only; the database is a temporary one) and
/// writes a report: `cargo test --release -p daemon --test files -- --ignored`, with
/// `FILES_REPORT` naming the output file (and `FILES_REPORT_DB` a copy of the database).
#[test]
#[ignore]
fn real_machine_report() {
    let env = Env::new();
    let d = env.start(daemon::modules::builtin());
    call(&d, &mut trusted(&d), "files.configure", json!({"settings": {"auto_scan": false}})).into_result().unwrap();
    let started = Instant::now();
    let scan = scan_within(&d, Duration::from_secs(3600));
    let q = |cmd: &str, args: Value| agent_call(&d, cmd, args).into_result().unwrap();
    let top = q("files.top", json!({"limit": 10}));
    let details: Vec<Value> = top["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| q("files.conversation", json!({"id": i["conversation_id"], "agent": i["agent"], "limit": 8})))
        .collect();
    // `FILES_FIND`: path fragments to look up, separated by `;`.
    let finds: Vec<Value> = std::env::var("FILES_FIND")
        .unwrap_or_default()
        .split(';')
        .filter(|f| !f.is_empty())
        .map(|f| json!({"query": f, "result": q("files.find", json!({"query": f, "limit": 15}))}))
        .collect();
    let report = json!({
        "seconds": started.elapsed().as_secs(),
        "finds": finds,
        "scan": scan,
        "top": top,
        "details": details,
        "folders": q("files.top", json!({"limit": 10, "by_folder": true})),
        "caches": q("files.caches", json!({})),
    });
    let out = std::env::var("FILES_REPORT").unwrap_or_else(|_| "files-report.json".into());
    std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    drop(d);
    if let Ok(copy) = std::env::var("FILES_REPORT_DB") {
        std::fs::copy(env.db_path(), copy).unwrap();
    }
}

#[test]
fn nothing_is_scanned_until_asked_or_due() {
    let env = Env::new();
    let d = env.start(daemon::modules::builtin());
    let s = agent_call(&d, "files.scans", json!({})).into_result().unwrap();
    assert_eq!(s["scans"], json!([]));
    assert!(s["next_auto_scan"].as_i64().unwrap() > daemon::store::now_ms() + 5 * 60_000, "{s}");
    let r = call(&d, &mut agent(&d), "files.configure", json!({"settings": {"interval_hours": 2}}));
    assert_eq!(err_code(&r).0, ErrorCode::Denied, "settings are changed from the page only");
    let r = call(&d, &mut trusted(&d), "files.configure", json!({"settings": {"interval_hours": 0}}));
    assert_eq!(err_code(&r).0, ErrorCode::InvalidArgs);
}
