//! The task module on the fake process table: following a tree, the activity rules,
//! renew, who ended what, priorities through the journal, dependencies, the hook switch, and
//! a daemon crash. No real process is touched.

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use common::{AGENT_EXE, Env, agent_call, call, err_code, module_status, trusted, wait_events};
use daemon::dispatch::Dispatcher;
use daemon::modules::NEEDS_ADMIN_NOTICE;
use daemon::sys::fake::RefusingElevated;
use daemon::sys::{ProcessPriority, System};
use daemon::{Event, JournalEntry, Module, ModuleCtx, ModuleRequest, ModuleSpec, RecoverOutcome, Section};
use proto::{ApiError, ErrorCode};
use serde_json::{Value, json};

const MIN: i64 = 60_000;

static RECORDER: ModuleSpec =
    ModuleSpec::new("recorder", Section::Usage, "Test module that keeps every task event").subscribes(&["task.*"]);

struct Recorder(Arc<Mutex<Vec<Event>>>);

impl Module for Recorder {
    fn spec(&self) -> &'static ModuleSpec {
        &RECORDER
    }

    fn handle(&mut self, _req: &ModuleRequest<'_>, _ctx: &ModuleCtx) -> Result<Value, ApiError> {
        Ok(json!({}))
    }

    fn on_event(&mut self, event: &Event, _ctx: &ModuleCtx) -> Result<(), ApiError> {
        self.0.lock().unwrap().push(event.clone());
        Ok(())
    }

    fn recover(&mut self, _entry: &JournalEntry, _ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        Ok(RecoverOutcome::Restored)
    }
}

struct T {
    env: Env,
    d: Arc<Dispatcher>,
    events: Arc<Mutex<Vec<Event>>>,
}

impl T {
    fn new() -> T {
        T::with(Env::new())
    }

    fn with(env: Env) -> T {
        let events = Arc::new(Mutex::new(Vec::new()));
        let d = env.start(vec![
            daemon::modules::mode::create(),
            Box::new(Recorder(events.clone())),
            daemon::modules::task::create(),
        ]);
        wait_events(&d);
        T { env, d, events }
    }

    /// Drops the daemon without stopping anything, like a crash.
    fn crash(self) -> Env {
        let T { env, d, .. } = self;
        drop(d);
        env
    }

    fn ok(&self, cmd: &str, args: Value) -> Value {
        let r = agent_call(&self.d, cmd, args);
        r.clone().into_result().unwrap_or_else(|e| panic!("{cmd}: {e:?}"))
    }

    fn protect(&self, args: Value) -> i64 {
        self.ok("protect", args)["task"]["task_id"].as_i64().unwrap()
    }

    fn check(&self) {
        self.ok("task.check", json!({}));
        wait_events(&self.d);
    }

    fn tasks(&self) -> Vec<Value> {
        self.ok("tasks", json!({}))["tasks"].as_array().unwrap().clone()
    }

    fn task(&self, id: i64) -> Option<Value> {
        self.tasks().into_iter().find(|t| t["task_id"] == id)
    }

    fn ended(&self) -> Vec<Value> {
        self.ok("tasks", json!({"all": true}))["ended"].as_array().unwrap().clone()
    }

    fn topics(&self, topic: &str) -> Vec<Value> {
        self.events.lock().unwrap().iter().filter(|e| e.topic == topic).map(|e| e.data.clone()).collect()
    }

    fn advance(&self, minutes: i64) {
        self.env.system.advance_ms(minutes * MIN);
    }
}

#[test]
fn a_protected_tree_takes_in_children_and_ends_when_everything_exits() {
    let t = T::new();
    let sys = &t.env.system;
    sys.spawn(100, 1, "python.exe").spawn(101, 100, "python.exe");
    let id = t.protect(json!({"pid": 100, "label": "training"}));
    let task = t.task(id).unwrap();
    assert_eq!((task["pids"].clone(), task["tracking"].as_str()), (json!([100, 101]), Some("job")));
    assert_eq!(task["requested_by"]["exe"], AGENT_EXE);
    wait_events(&t.d);
    assert_eq!(t.topics("task.protected")[0]["active"], 1);
    let st = agent_call(&t.d, "status", json!({})).into_result().unwrap();
    let mode = module_status(&st, "mode")["state"]["summary"].as_str().unwrap().to_owned();
    assert!(mode.contains("1 protected task"), "the mode module counts it: {mode}");

    // Started later, by a member: joins by itself; the root exiting is not the end.
    sys.spawn(102, 101, "nvcc.exe");
    sys.exit(100);
    t.check();
    assert_eq!(t.task(id).unwrap()["pids"], json!([101, 102]));
    assert!(t.topics("task.snapshot").last().unwrap()["tasks"][0]["pids"] == json!([101, 102]));

    sys.exit(102);
    sys.exit(101);
    t.check();
    assert!(t.task(id).is_none());
    let ended = t.ended();
    assert_eq!((ended[0]["reason"].as_str(), ended[0]["by"]["kind"].as_str()), (Some("exited"), Some("daemon")));
    let gone = t.topics("task.unprotected");
    assert_eq!((gone[0]["reason"].as_str(), gone[0]["active"].as_u64()), (Some("exited"), Some(0)));
    assert_eq!(sys.trackers(), 0);
}

#[test]
fn ten_minutes_without_any_activity_ends_the_protection() {
    let t = T::new();
    let sys = &t.env.system;
    sys.spawn(200, 1, "agent.exe");
    sys.touch(r"C:\t\conv.jsonl");
    sys.spawn(900, 1, "postgres.exe");
    let id = t.protect(json!({"pid": 200, "transcript": r"C:\t\conv.jsonl", "deps": ["postgres"]}));
    // Each kind of activity keeps it alive on its own.
    let kinds: [(&str, Box<dyn Fn()>); 6] = [
        ("cpu", Box::new(|| sys.use_cpu(200, 10_000))),
        ("io", Box::new(|| sys.use_io(200, 1 << 20))),
        ("gpu", Box::new(|| sys.set_gpu(200, 30.0))),
        ("transcript", Box::new(|| sys.touch(r"C:\t\conv.jsonl"))),
        ("processes", Box::new(|| {
            sys.spawn(201, 200, "git.exe");
        })),
        ("dependencies", Box::new(|| sys.use_cpu(900, 10_000))),
    ];
    for (kind, act) in kinds {
        t.advance(9);
        act();
        t.check();
        let task = t.task(id).unwrap_or_else(|| panic!("ended despite {kind}"));
        assert!(task["last_activity"].as_array().unwrap().contains(&json!(kind)), "{kind}: {task}");
        sys.gpu.lock().unwrap().clear();
    }
    // A little CPU (under 1 % of a core) is not activity.
    t.advance(9);
    sys.use_cpu(200, 1_000);
    t.check();
    assert!(t.task(id).is_some(), "9 minutes is not 10");
    assert_eq!(t.task(id).unwrap()["last_activity"], json!([]));
    t.advance(2);
    t.check();
    assert!(t.task(id).is_none());
    assert_eq!(t.ended()[0]["reason"], "idle");
    assert!(sys.alive(200), "ending a protection never ends a process");
}

#[test]
fn renew_extends_the_quiet_time_up_to_the_limit() {
    let t = T::new();
    t.env.system.spawn(300, 1, "wait.exe");
    let id = t.protect(json!({"pid": 300, "ttl_minutes": 30}));
    t.advance(25);
    t.check();
    assert!(t.task(id).is_some(), "ttl from protect");
    let r = t.ok("renew", json!({"task_id": id, "ttl_minutes": 120}));
    assert!(r["task"]["quiet_until"].as_i64().unwrap() > t.env.system.now_ms() + 119 * MIN);
    t.advance(100);
    t.check();
    assert!(t.task(id).is_some());
    t.advance(21);
    t.check();
    assert!(t.task(id).is_none());

    t.env.system.spawn(301, 1, "wait.exe");
    let id = t.protect(json!({"pid": 301}));
    let resp = agent_call(&t.d, "renew", json!({"task_id": id, "ttl_minutes": 100_000}));
    assert_eq!(err_code(&resp), (ErrorCode::InvalidArgs, Some("task.TTL_TOO_LONG".into())));
    let resp = agent_call(&t.d, "renew", json!({"task_id": 999}));
    assert_eq!(err_code(&resp), (ErrorCode::NotFound, Some("task.NO_SUCH_TASK".into())));
}

#[test]
fn anyone_may_end_a_protection_and_it_is_recorded_who() {
    let t = T::new();
    t.env.system.spawn(400, 1, "x.exe");
    let mut page = trusted(&t.d);
    let r = call(&t.d, &mut page, "protect", json!({"pid": 400})).into_result().unwrap();
    let id = r["task"]["task_id"].as_i64().unwrap();
    assert_eq!(r["task"]["requested_by"]["kind"], "trusted");
    // An agent ends the protection the user set up.
    let r = t.ok("unprotect", json!({"task_id": id}));
    assert_eq!(r["ended"][0]["by"]["kind"], "agent");
    assert_eq!(r["ended"][0]["by"]["pid"], 4242);
    wait_events(&t.d);
    let ev = &t.topics("task.unprotected")[0];
    assert_eq!((ev["reason"].as_str(), ev["by"]["exe"].as_str()), (Some("cancelled"), Some(AGENT_EXE)));
    let ended = &t.ended()[0];
    assert_eq!((ended["reason"].as_str(), ended["by"]["kind"].as_str()), (Some("cancelled"), Some("agent")));
    let resp = agent_call(&t.d, "unprotect", json!({"task_id": id}));
    assert_eq!(err_code(&resp).1.as_deref(), Some("task.NO_SUCH_TASK"));
    assert_eq!(err_code(&agent_call(&t.d, "unprotect", json!({}))).1.as_deref(), Some("task.BAD_TARGET"));
    // The op log has it too.
    let log = t.ok("log", json!({}));
    assert!(log["entries"].as_array().unwrap().iter().any(|e| e["cmd"] == "task.unprotect" && e["ok"] == true));
}

#[test]
fn priority_goes_up_to_high_at_most_and_back_when_the_protection_ends() {
    let t = T::new();
    let sys = &t.env.system;
    sys.spawn(500, 1, "train.exe").spawn(501, 500, "worker.exe").spawn(502, 500, "rt.exe");
    sys.set_priority(502, None); // real-time: left alone
    let id = t.protect(json!({"pid": 500}));
    let r = t.ok("prioritize", json!({"task_id": id}));
    assert_eq!(r["task"]["priority"], "high");
    assert_eq!((sys.priority(500), sys.priority(501), sys.priority(502)), (Some(ProcessPriority::High), Some(ProcessPriority::High), None));
    let open: Vec<String> = t.d.store().journal_open(Some("task")).unwrap().into_iter().map(|e| e.key).collect();
    assert_eq!(open.len(), 2, "{open:?}");
    assert!(open.iter().all(|k| k.starts_with("priority:")));
    // A later child is raised too.
    sys.spawn(503, 501, "helper.exe");
    t.check();
    assert_eq!(sys.priority(503), Some(ProcessPriority::High));
    // Back to normal: everything put back, nothing left open.
    t.ok("unprotect", json!({"task_id": id}));
    for pid in [500, 501, 503] {
        assert_eq!(sys.priority(pid), Some(ProcessPriority::Normal), "{pid}");
    }
    assert!(t.d.store().journal_open(Some("task")).unwrap().is_empty());
    assert_eq!(err_code(&agent_call(&t.d, "prioritize", json!({"task_id": id}))).1.as_deref(), Some("task.NO_SUCH_TASK"));

    let id = t.protect(json!({"pid": 501}));
    let resp = agent_call(&t.d, "prioritize", json!({"task_id": id, "level": "realtime"}));
    assert_eq!(err_code(&resp).1.as_deref(), Some("task.BAD_LEVEL"));
    t.ok("prioritize", json!({"task_id": id, "level": "above_normal"}));
    assert_eq!(sys.priority(501), Some(ProcessPriority::AboveNormal));
    t.ok("prioritize", json!({"task_id": id, "level": "normal"}));
    assert_eq!(sys.priority(501), Some(ProcessPriority::Normal));
}

#[test]
fn an_elevated_program_needs_the_administrator_layer() {
    let mut env = Env::new();
    env.config.sys.elevated = Arc::new(RefusingElevated::default());
    let t = T::with(env);
    let sys = &t.env.system;
    // An elevated root: no job object for it either, so it is followed through parent ids.
    sys.spawn(600, 1, "admin.exe").spawn(601, 600, "worker.exe");
    sys.set_elevated(600);
    let id = t.protect(json!({"pid": 600}));
    assert_eq!(t.task(id).unwrap()["tracking"], "parent_ids");
    t.ok("prioritize", json!({"task_id": id}));
    assert_eq!((sys.priority(600), sys.priority(601)), (Some(ProcessPriority::Normal), Some(ProcessPriority::High)));
    let st = agent_call(&t.d, "status", json!({})).into_result().unwrap();
    let notices = module_status(&st, "task")["state"]["notices"].to_string();
    assert!(notices.contains("pid 600") && notices.contains(NEEDS_ADMIN_NOTICE), "{notices}");
    // Nothing changed for it, so nothing is left open in the journal for it.
    assert_eq!(t.d.store().journal_open(Some("task")).unwrap().len(), 1);
}

#[test]
fn a_crash_puts_priorities_back_and_the_protection_carries_on() {
    let t = T::new();
    let sys = t.env.system.clone();
    sys.spawn(700, 1, "long.exe").spawn(701, 700, "child.exe");
    sys.spawn(710, 1, "short.exe");
    let id = t.protect(json!({"pid": 700, "label": "long"}));
    let short = t.protect(json!({"pid": 710}));
    t.ok("prioritize", json!({"task_id": id}));
    let env = t.crash();
    assert_eq!(sys.priority(700), Some(ProcessPriority::High));
    assert!(sys.alive(700) && sys.alive(701), "a crash ends no process");
    sys.exit(710); // ends while the daemon is down

    let t = T::with(env);
    assert!(t.d.recovery().restored >= 2, "{:?}", t.d.recovery());
    let task = t.task(id).expect("still protected");
    assert_eq!((task["label"].as_str(), task["pids"].clone()), (Some("long"), json!([700, 701])));
    // Put back by recovery, then raised again because the task is still prioritized.
    assert_eq!(sys.priority(700), Some(ProcessPriority::High));
    assert!(t.task(short).is_none());
    let ended = t.ended();
    assert!(ended.iter().any(|e| e["task_id"] == short && e["reason"] == "exited"), "{ended:?}");
    assert_eq!(t.topics("task.active")[0]["active"], 1);

    // A clean stop puts the priorities back but keeps the protection for the next start.
    t.d.shutdown();
    assert_eq!(sys.priority(700), Some(ProcessPriority::Normal));
    let t = T::with(t.crash());
    assert!(t.task(id).is_some());
    assert_eq!(sys.priority(700), Some(ProcessPriority::High));
}

#[test]
fn without_a_job_object_the_tree_is_followed_through_parent_ids() {
    let env = Env::new();
    env.system.refuse_jobs.store(true, Ordering::SeqCst);
    let t = T::with(env);
    let sys = &t.env.system;
    sys.spawn(800, 1, "a.exe").spawn(801, 800, "b.exe");
    let id = t.protect(json!({"pid": 800}));
    assert_eq!(t.task(id).unwrap()["tracking"], "parent_ids");
    sys.spawn(802, 801, "c.exe");
    sys.use_cpu(801, 20_000);
    t.advance(1);
    t.check();
    let task = t.task(id).unwrap();
    assert_eq!(task["pids"], json!([800, 801, 802]));
    assert!(task["last_activity"].as_array().unwrap().contains(&json!("cpu")));
    let st = agent_call(&t.d, "status", json!({})).into_result().unwrap();
    assert!(module_status(&st, "task")["state"]["notices"].to_string().contains("parent ids"));
}

#[test]
fn dependencies_are_given_or_found_and_published() {
    let t = T::new();
    let sys = &t.env.system;
    sys.spawn(1000, 1, "bash.exe").spawn(1001, 1000, "wsl.exe");
    sys.spawn(1100, 1, "wslservice.exe").spawn(1101, 1, "vmmemWSL").spawn(1200, 1, "postgres.exe");
    let id = t.protect(json!({"pid": 1000, "deps": ["postgres.exe"], "mem_limit_mb": 10}));
    let deps = t.task(id).unwrap()["deps"].clone();
    assert_eq!(deps, json!([
        {"name": "postgres", "auto": false, "pids": [1200]},
        {"name": "wsl", "auto": true, "pids": [1100, 1101]},
    ]));
    wait_events(&t.d);
    let snap = t.topics("task.snapshot").last().unwrap().clone();
    assert_eq!(snap["tasks"][0]["dep_pids"], json!([1100, 1101, 1200]));
    sys.set_memory_mb(1001, 50);
    t.check();
    let task = t.task(id).unwrap();
    assert_eq!((task["memory_mb"].as_u64(), task["over_mem_limit"].as_bool()), (Some(50), Some(true)));
    let st = agent_call(&t.d, "status", json!({})).into_result().unwrap();
    assert!(module_status(&st, "task")["state"]["notices"].to_string().contains("over its own limit"));
}

#[test]
fn conversations_register_themselves_only_when_switched_on() {
    let t = T::new();
    t.env.system.spawn(1300, 1, "claude.exe");
    let hook = json!({"pid": 1300, "source": "hook", "session_id": "s-1", "transcript": r"C:\t\s-1.jsonl"});
    let r = t.ok("protect", hook.clone());
    assert_eq!(r["registered"], false);
    assert!(t.tasks().is_empty());
    let resp = agent_call(&t.d, "task.configure", json!({"settings": {"auto_register_conversations": true}}));
    assert_eq!(err_code(&resp).0, ErrorCode::Denied, "page only");
    let mut page = trusted(&t.d);
    assert!(call(&t.d, &mut page, "task.configure", json!({"settings": {"auto_register_conversations": true}})).ok);
    let first = t.ok("protect", hook.clone());
    assert_eq!((first["registered"].as_bool(), first["already"].as_bool()), (Some(true), Some(false)));
    let again = t.ok("protect", hook);
    assert_eq!(again["already"], true, "the same conversation is one task");
    let r = t.ok("unprotect", json!({"session_id": "s-1"}));
    assert_eq!(r["ended"].as_array().unwrap().len(), 1);
    assert_eq!(t.ok("unprotect", json!({"session_id": "s-1"}))["ended"], json!([]));
}

#[test]
fn ancestor_protects_the_caller_s_parents() {
    let t = T::new();
    let sys = &t.env.system;
    // The test session calls from pid 4242.
    sys.spawn(4000, 1, "claude.exe").spawn(5000, 4000, "bash.exe").spawn(4242, 5000, "away-guard.exe");
    let r = t.ok("protect", json!({"ancestor": 2}));
    assert_eq!(r["task"]["root_pid"], 4000);
    let r = t.ok("protect", json!({"ancestor": 1}));
    assert_eq!(r["already"], true, "bash is inside the protected tree already");
    let resp = agent_call(&t.d, "protect", json!({"ancestor": 9}));
    assert_eq!(err_code(&resp).1.as_deref(), Some("task.NO_SUCH_PROCESS"));
    for bad in [json!({}), json!({"pid": 4000, "ancestor": 1}), json!({"ancestor": 0}), json!({"pid": 0})] {
        assert_eq!(err_code(&agent_call(&t.d, "protect", bad.clone())).1.as_deref(), Some("task.BAD_TARGET"), "{bad}");
    }
    let resp = agent_call(&t.d, "protect", json!({"pid": 31337}));
    assert_eq!(err_code(&resp), (ErrorCode::NotFound, Some("task.NO_SUCH_PROCESS".into())));
}
