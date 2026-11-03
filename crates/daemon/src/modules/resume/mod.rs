//! Continuations use adapter-neutral failure events. Failure hooks stay short; a durable
//! queue waits for connectivity and reopens the same session when a hook cannot continue it.
use crate::module::{
    ArgKind, ArgSpec, CommandSpec, Event, EventSpec, Migration, Module, ModuleCtx, ModuleRequest, ModuleSpec,
    RecoverOutcome, Section,
};
use crate::modules::settings;
use crate::store::JournalEntry;
use crate::sys::{AgentResume, ProcessInfo};
use proto::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

static SPEC:ModuleSpec=ModuleSpec::new("resume",Section::Usage,"Continue network-failed agent conversations")
    .commands(&[
        CommandSpec::new("show","Pending continuations and recent results").agent().read_only(),
        CommandSpec::new("settings","Continuation settings").agent().read_only(),
        CommandSpec::new("configure","Change continuation settings (page only)").args(&[ArgSpec::new("settings",ArgKind::Json,"Partial settings object").required()]),
        CommandSpec::new("check","Process waiting continuations now").agent(),
        CommandSpec::new("cancel","Cancel a queued continuation").agent().args(&[ArgSpec::new("id",ArgKind::Int,"Queue id").required()]),
    ])
    .publishes(&[EventSpec::new("continued","A network-failed session was reopened"),EventSpec::new("forked","A new conversation took over"),EventSpec::new("notice","Continuation requires user attention"),EventSpec::new("expired","A continuation timed out"),
        EventSpec::new("active","How many continuations are waiting or running, and how many of those are for protected conversations")])
    .subscribes(&["agents.*","network.up","network.reading","network.down","core.started"])
    .migrations(&[Migration::new(1,"CREATE TABLE resume_queue (id INTEGER PRIMARY KEY, agent TEXT NOT NULL, session TEXT NOT NULL, incident TEXT NOT NULL, context TEXT NOT NULL,
        created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, state TEXT NOT NULL, process TEXT, last_at INTEGER, detail TEXT,
        UNIQUE(agent,session,incident));"),
        Migration::new(2,"CREATE TABLE resume_lineage(agent TEXT NOT NULL, session TEXT NOT NULL, root_session TEXT NOT NULL, incident TEXT NOT NULL, parent_session TEXT NOT NULL, PRIMARY KEY(agent,session));"),
        Migration::new(3,"CREATE TABLE resume_session_mode(agent TEXT NOT NULL, session TEXT NOT NULL, permission_mode TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY(agent,session));")
    ]);
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResumeSettings {
    pub enabled: bool,
    pub max_attempts: u32,
    pub wait_minutes: u32,
    pub retry_delay_seconds: u32,
    pub prompt: String,
}
impl Default for ResumeSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_attempts: 3,
            wait_minutes: 120,
            retry_delay_seconds: 2,
            prompt: "网络已经恢复。请重试刚才因为断网失败的那一步，然后接着做原来的任务。".into(),
        }
    }
}
impl ResumeSettings {
    fn check(&self) -> Result<(), String> {
        if !(1..=3).contains(&self.max_attempts)
            || !(1..=120).contains(&self.wait_minutes)
            || !(1..=60).contains(&self.retry_delay_seconds)
            || self.prompt.trim().is_empty()
            || self.prompt.len() > 4096
        {
            return Err(
                "attempts 1..=3, wait_minutes 1..=120, retry delay 1..=60; nonempty prompt up to 4096 bytes"
                    .into(),
            );
        }
        Ok(())
    }
}
struct Resume {
    settings: Arc<Mutex<ResumeSettings>>,
    gate: Arc<Mutex<()>>,
    announced: Arc<Mutex<Option<(u64, u64)>>>,
    tx: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}
pub fn create() -> Box<dyn Module> {
    Box::new(Resume {
        settings: Arc::new(Mutex::new(ResumeSettings::default())),
        gate: Arc::new(Mutex::new(())),
        announced: Arc::new(Mutex::new(None)),
        tx: None,
        worker: None,
    })
}
/// Tells mode and power when the number of continuations still to come changes: while any is
/// left, the work is not done.
fn announce_active(ctx: &ModuleCtx, announced: &Mutex<Option<(u64, u64)>>) -> Result<(), ApiError> {
    let rows = ctx.db().query(
        "SELECT context FROM resume_queue WHERE state IN ('waiting','launching','running')",
        &[],
    )?;
    let protected = rows.iter().filter(|r| {
        r.text(0)
            .ok()
            .and_then(|c| serde_json::from_str::<Value>(&c).ok())
            .is_some_and(|c| c["protection"].is_object())
    });
    let now = (protected.count() as u64, rows.len() as u64);
    let mut last = announced.lock().unwrap_or_else(|p| p.into_inner());
    if *last != Some(now) {
        *last = Some(now);
        ctx.publish("active", json!({"protected":now.0,"active":now.1}))?;
    }
    Ok(())
}
/// What the queue must remember of a protected task until the continuation can take it over.
fn protection_of(ctx: &ModuleCtx, event: &Value) -> Option<Value> {
    let pid = event["agent_pid"].as_u64()?;
    let a = ctx.arbitration();
    let task = a
        .tasks
        .iter()
        .find(|t| t["pids"].as_array().is_some_and(|p| p.iter().any(|x| x.as_u64() == Some(pid))))?;
    Some(json!({"label":task["label"],"deps":task["given_deps"],"mem_limit_mb":task["mem_limit_mb"]}))
}
/// A conversation not heard from for this long will not be continued with what it last said.
const MODE_KEPT_MS: i64 = 30 * 24 * 3_600_000;
fn sql(e: crate::sqlite::Error) -> ApiError {
    ApiError::internal(e.to_string())
}
fn notify(ctx: &ModuleCtx, reason: &str, data: Value) {
    let message = match reason {
        "continuation_attempt_limit" => "这次断网已自动续跑 3 次，现已停止。请检查网络后手动继续。",
        "network_wait_timeout" => "等待网络恢复已超过两小时，已放弃自动续跑。",
        "continuation_launch_uncertain" => "后台在启动续跑时中断，无法确认是否已经启动，请先检查会话。",
        "continuation_fork_unsupported" => "原窗口仍开着，此 agent 不支持安全分叉，请在原窗口手动继续。",
        "continuation_forked" => {
            "已分叉新对话接着做。原窗口看不到新进度；两边使用同一目录，请勿在原窗口继续这项任务。"
        }
        "continuation_both_active" => "原窗口和分叉对话现在都在运行，请勿让两边同时修改同一项任务。",
        "continuation_context_unavailable" => "原窗口的续跑辅助进程已不可用，请在原环境手动继续。",
        "continuation_not_started" => "自动续跑启动失败，请查看状态后手动继续。",
        "continuation_needs_approval" => {
            "已自动续跑。这个对话的权限模式需要有人批准操作，后台没人批准，这类操作会被拒绝，回来后请检查它做到了哪一步。"
        }
        "continuation_mode_unknown" => {
            "已自动续跑，但查不到这个对话后来的权限模式，按启动时的设置运行。需要批准的操作会被拒绝，回来后请检查它做到了哪一步。"
        }
        _ => "此对话需要手动继续，请查看续跑状态。",
    };
    let message = if let Some(session) = data["new_session_id"].as_str() {
        format!(
            "{message}\n原编号：{}；新编号：{session}\n接手：claude --resume {session}",
            data["original_session_id"].as_str().unwrap_or("")
        )
    } else {
        message.to_owned()
    };
    let _ = ctx.publish(
        "notice",
        json!({"reason":reason,"message":message,"details":data}),
    );
    let _ = ctx.system().notify_user("Arcturus 续跑", &message);
}
fn queue(ctx: &ModuleCtx, s: &ResumeSettings, event: &Value) -> Result<(), ApiError> {
    if !s.enabled
        || event["continuing"] == true
        || event["outcome"]["kind"] != "api_error"
        || event["outcome"]["failure"]["network"] != true
    {
        return Ok(());
    }
    let agent = event["agent"]
        .as_str()
        .ok_or_else(|| ApiError::invalid_args("missing agent"))?;
    let session = event["session_id"]
        .as_str()
        .ok_or_else(|| ApiError::invalid_args("missing session"))?;
    let Some(adapter) = agents::get(agent) else {
        return Ok(());
    };
    if !adapter.info().hooks.reports_api_errors
        || adapter
            .resume_args(session, &s.prompt, &agents::RunSettings::default())
            .is_none()
    {
        notify(
            ctx,
            "agent_can_only_be_notified",
            json!({"agent":agent,"session_id":session}),
        );
        return Ok(());
    }
    let exe = event["agent_exe"]
        .as_str()
        .filter(|p| crate::sys::programs::normalize(p).is_ok());
    let identity = event["agent_pid"]
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
        .and_then(|p| ctx.system().process_info(p).ok());
    if exe.is_none()
        || identity
            .as_ref()
            .is_none_or(|p| p.own_user != Some(true) || p.exe.as_deref() != exe)
    {
        notify(
            ctx,
            "cannot_verify_agent_process",
            json!({"agent":agent,"session_id":session}),
        );
        return Ok(());
    }
    let now = ctx.system().now_ms();
    let lineage = ctx.db().query(
        "SELECT root_session,incident FROM resume_lineage WHERE agent=? AND session=?",
        &[agent.into(), session.into()],
    )?;
    let root = lineage
        .first()
        .map(|r| r.text(0))
        .transpose()
        .map_err(sql)?
        .unwrap_or_else(|| session.into());
    let previous = ctx.db().query(
        "SELECT incident,state FROM resume_queue WHERE agent=? AND session=? ORDER BY id DESC LIMIT 1",
        &[agent.into(), root.as_str().into()],
    )?;
    let active = previous.first().filter(|r| {
        !matches!(
            r.text(1).ok().as_deref(),
            Some("completed" | "cancelled" | "user_active" | "user_ended")
        )
    });
    let incident = ctx
        .arbitration()
        .network_outage
        .map(|id| format!("outage:{id}"))
        .or_else(|| active.and_then(|r| r.text(0).ok()))
        .unwrap_or_else(|| format!("unobserved:{now}"));
    if active.is_some_and(|r| {
        r.text(1).ok().as_deref() == Some("exhausted") && r.text(0).ok().as_deref() == Some(incident.as_str())
    }) {
        return Ok(());
    }
    let permission_mode = ctx
        .db()
        .query(
            "SELECT permission_mode FROM resume_session_mode WHERE agent=? AND session=?",
            &[agent.into(), session.into()],
        )?
        .first()
        .and_then(|r| r.text(0).ok());
    // A retry's process may already have lost its task; the first failure's protection stands.
    let protection = protection_of(ctx, event).or_else(|| {
        ctx.db()
            .query(
                "SELECT context FROM resume_queue WHERE agent=? AND session=? AND incident=?",
                &[agent.into(), root.as_str().into(), incident.as_str().into()],
            )
            .ok()?
            .first()
            .and_then(|r| r.text(0).ok())
            .and_then(|c| serde_json::from_str::<Value>(&c).ok())
            .map(|c| c["protection"].clone())
            .filter(Value::is_object)
    });
    let context = json!({"agent":agent,"executable":exe,"session_id":session,"cwd":event["cwd"],"origin":identity,"root_session":root,"queued_at":now,"permission_mode":permission_mode,"protection":protection,"in_a_window":event["in_a_window"]});
    ctx.db().execute("INSERT INTO resume_queue(agent,session,incident,context,created_at,expires_at,state) VALUES(?,?,?,?,?,?,'waiting')
        ON CONFLICT(agent,session,incident) DO UPDATE SET context=excluded.context,process=NULL,state=CASE WHEN state IN ('expired','needs_user','exhausted') THEN state ELSE 'waiting' END",
        &[agent.into(),root.as_str().into(),incident.into(),context.to_string().into(),now.into(),now.saturating_add(i64::from(s.wait_minutes)*60_000).into()])?;
    Ok(())
}
fn tick(ctx: &ModuleCtx, s: &ResumeSettings) -> Result<(), ApiError> {
    if !ctx.is_active() {
        return Ok(());
    }
    let now = ctx.system().now_ms();
    for r in ctx.db().query("SELECT id,context,expires_at,attempts,state,process,last_at,session,incident FROM resume_queue WHERE state IN ('waiting','running') ORDER BY id", &[])? {
        let id=r.int(0).map_err(sql)?;
        let state=r.text(4).map_err(sql)?;
        let root=r.text(7).map_err(sql)?;
        let incident=r.text(8).map_err(sql)?;
        if state=="running" {
            if let Some(process)=r.opt_text(5).map_err(sql)?.and_then(|p|serde_json::from_str::<ProcessInfo>(&p).ok()) {
                match ctx.system().process_info(process.pid) {
                    Ok(p) if p.started_at==process.started_at => continue,
                    Err(e) if e.code!=ErrorCode::NotFound => continue,
                    _=>{}
                }
            }
            ctx.db().execute("UPDATE resume_queue SET state='continued' WHERE id=? AND state='running'", &[id.into()])?;
            continue;
        }
        if r.int(2).map_err(sql)?<=now {
            ctx.db().execute("UPDATE resume_queue SET state='expired',detail='network wait exceeded' WHERE id=?", &[id.into()])?;
            let data=json!({"id":id,"original_session_id":root,"reason":"network_wait_timeout"});
            ctx.publish("expired",data.clone())?; notify(ctx,"network_wait_timeout",data); continue;
        }
        let attempts=r.int(3).map_err(sql)?;
        if attempts>=i64::from(s.max_attempts) {
            ctx.db().execute("UPDATE resume_queue SET state='exhausted' WHERE id=?", &[id.into()])?;
            notify(ctx,"continuation_attempt_limit",json!({"id":id,"original_session_id":root})); continue;
        }
        if !s.enabled || ctx.arbitration().network_online!=Some(true) { continue; }
        if r.opt_int(6).map_err(sql)?.is_some_and(|at|now.saturating_sub(at)<i64::from(s.retry_delay_seconds)*1000) { continue; }
        let mut value:Value=serde_json::from_str(&r.text(1).map_err(sql)?).map_err(|e|ApiError::internal(e.to_string()))?;
        let origin:ProcessInfo=serde_json::from_value(value["origin"].clone()).map_err(|e|ApiError::internal(e.to_string()))?;
        let alive=match ctx.system().process_info(origin.pid) {
            Ok(p) if p.started_at==origin.started_at && p.exe==origin.exe => true,
            Ok(p) if p.started_at==origin.started_at => continue,
            Err(e) if e.code!=ErrorCode::NotFound => continue,
            _=>false
        };
        // Give print-mode SessionEnd/exit its short tail; an open window then forks promptly.
        if alive && value["queued_at"].as_i64().is_some_and(|t|now.saturating_sub(t)<2000) { continue; }
        value["prompt"]=json!(s.prompt);
        let mut request:AgentResume=serde_json::from_value(value.clone()).map_err(|e|ApiError::internal(e.to_string()))?;
        let key=(request.agent.clone(),request.session_id.clone());
        request.broker=ctx.arbitration().resume_brokers.get(&key).cloned();
        request.environment=ctx.arbitration().resume_environments.get(&key)
            .filter(|(p,_)|p.pid==origin.pid&&p.started_at==origin.started_at).map(|(_,e)|e.clone());
        if request.broker.is_none() && request.environment.is_none() {
            ctx.db().execute("UPDATE resume_queue SET state='needs_user',detail='original continuation context unavailable' WHERE id=?", &[id.into()])?;
            notify(ctx,"continuation_context_unavailable",json!({"id":id})); continue;
        }
        if alive {
            let new=ctx.system().new_session_id()?;
            if agents::get(&request.agent).and_then(|a|a.fork_args(&request.session_id,&new,&s.prompt,&agents::RunSettings::default())).is_none() {
                ctx.db().execute("UPDATE resume_queue SET state='needs_user',detail='open agent has no fork support' WHERE id=?", &[id.into()])?;
                notify(ctx,"continuation_fork_unsupported",json!({"id":id,"session_id":request.session_id})); continue;
            }
            request.fork_session_id=Some(new.clone());
            // Mapping is committed before the child can emit SessionStart(source=fork).
            ctx.db().execute("INSERT INTO resume_lineage(agent,session,root_session,incident,parent_session) VALUES(?,?,?,?,?)",
                &[request.agent.as_str().into(),new.as_str().into(),root.as_str().into(),incident.as_str().into(),request.session_id.as_str().into()])?;
        }
        let claimed=ctx.db().execute("UPDATE resume_queue SET state='launching',attempts=attempts+1,last_at=? WHERE id=? AND state='waiting'", &[now.into(),id.into()])?;
        if claimed!=1 { continue; }
        match ctx.system().resume_agent(&request) {
            Ok(process)=>{
                let target=request.fork_session_id.as_ref().unwrap_or(&request.session_id);
                value["session_id"]=json!(target); value["origin"]=json!(process);
                ctx.db().execute("UPDATE resume_queue SET state='running',process=?,context=?,detail=NULL WHERE id=?",
                    &[json!(process).to_string().into(),value.to_string().into(),id.into()])?;
                ctx.arbitration().resume_brokers.remove(&key);
                let data=json!({"id":id,"agent":request.agent,"session_id":target,"original_session_id":root,"parent_session_id":request.session_id,"new_session_id":request.fork_session_id,"pid":process.pid,"attempt":attempts+1,"protection":value["protection"]});
                ctx.publish("continued",data.clone())?;
                if request.fork_session_id.is_some() { ctx.publish("forked",data.clone())?; notify(ctx,"continuation_forked",data.clone()); }
                // A continuation runs with nobody at the keyboard, whatever the window could ask.
                match request.permission_mode.as_deref() {
                    Some(mode) if agents::get(&request.agent).is_some_and(|a|a.runs_unattended(mode)) => {}
                    Some(mode) => notify(ctx,"continuation_needs_approval",json!({"id":id,"session_id":target,"permission_mode":mode})),
                    None => notify(ctx,"continuation_mode_unknown",json!({"id":id,"session_id":target})),
                }
            }
            Err(e)=>{
                ctx.db().execute("UPDATE resume_queue SET state='needs_user',detail=? WHERE id=?", &[e.message.as_str().into(),id.into()])?;
                notify(ctx,"continuation_not_started",json!({"id":id,"error":e}));
            }
        }
    }
    Ok(())
}
fn rows(ctx: &ModuleCtx) -> Result<Value, ApiError> {
    let rows=ctx.db().query("SELECT id,agent,session,incident,created_at,expires_at,attempts,state,detail,process,context FROM resume_queue ORDER BY id DESC LIMIT 100",&[])?;
    let entries=rows.iter().map(|r|Ok(json!({"id":r.int(0)?,"agent":r.text(1)?,"session_id":r.text(2)?,"incident":r.text(3)?,"created_at":r.int(4)?,"expires_at":r.int(5)?,"attempts":r.int(6)?,"state":r.text(7)?,"detail":r.opt_text(8)?,"process":r.opt_text(9)?.and_then(|p|serde_json::from_str::<Value>(&p).ok()),"current_session_id":serde_json::from_str::<Value>(&r.text(10)?).ok().and_then(|v|v["session_id"].as_str().map(str::to_owned))}))).collect::<Result<Vec<_>,crate::sqlite::Error>>().map_err(sql)?;
    Ok(json!({"entries":entries}))
}
impl Resume {
    fn stop_worker(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
impl Drop for Resume {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
    }
}
impl Module for Resume {
    fn spec(&self) -> &'static ModuleSpec {
        &SPEC
    }
    fn start(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        *self.settings.lock().unwrap() = settings::load(ctx)?;
        let uncertain = ctx
            .db()
            .query("SELECT id,session FROM resume_queue WHERE state='launching'", &[])?;
        ctx.db().execute("UPDATE resume_queue SET state='needs_user',detail='daemon stopped during launch; refusing a duplicate' WHERE state='launching'", &[])?;
        for row in uncertain {
            notify(
                ctx,
                "continuation_launch_uncertain",
                json!({"id":row.int(0).map_err(sql)?,"session_id":row.text(1).map_err(sql)?}),
            );
        }
        let (tx, rx) = mpsc::channel();
        let settings = self.settings.clone();
        let gate = self.gate.clone();
        let announced = self.announced.clone();
        let ctx = ctx.clone();
        self.worker = Some(
            std::thread::Builder::new()
                .name("resume".into())
                .spawn(move || {
                    loop {
                        match rx.recv_timeout(Duration::from_secs(1)) {
                            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                let s = settings.lock().unwrap().clone();
                                let _gate = gate.lock().unwrap_or_else(|p| p.into_inner());
                                if let Err(e) = tick(&ctx, &s).and_then(|()| announce_active(&ctx, &announced)) {
                                    notify(&ctx, "continuation_queue_failed", json!(e));
                                }
                            }
                        }
                    }
                })
                .map_err(|e| ApiError::internal(e.to_string()))?,
        );
        self.tx = Some(tx);
        Ok(())
    }
    fn stop(&mut self, _: &ModuleCtx) -> Result<(), ApiError> {
        self.stop_worker();
        // Says nothing now: a stopping daemon must not look like finished work to power.
        *self.announced.lock().unwrap_or_else(|p| p.into_inner()) = None;
        Ok(())
    }
    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        match req.cmd {
            "show" => rows(ctx),
            "settings" => Ok(json!(*self.settings.lock().unwrap())),
            "configure" => {
                let s = settings::patch(ctx, &req.args["settings"], &[], ResumeSettings::check)?;
                *self.settings.lock().unwrap() = s.clone();
                Ok(json!({"settings":s}))
            }
            "check" => {
                let s = self.settings.lock().unwrap().clone();
                let _gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
                tick(ctx, &s)?;
                announce_active(ctx, &self.announced)?;
                rows(ctx)
            }
            "cancel" => {
                let _gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
                let n=ctx.db().execute("UPDATE resume_queue SET state='cancelled' WHERE id=? AND state IN ('waiting','launching','running')",&[req.args["id"].as_i64().unwrap().into()])?;
                if n == 0 {
                    return Err(ApiError::new(
                        ErrorCode::NotFound,
                        "no active continuation with that id",
                    ));
                }
                announce_active(ctx, &self.announced)?;
                Ok(json!({"cancelled":true}))
            }
            _ => Err(ApiError::internal("unrouted resume command")),
        }
    }
    fn status(&self, ctx: &ModuleCtx) -> Value {
        json!({"implemented":true,"queue":rows(ctx).unwrap_or(Value::Null),"settings":*self.settings.lock().unwrap()})
    }
    fn on_event(&mut self, e: &Event, ctx: &ModuleCtx) -> Result<(), ApiError> {
        let _gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        let agent = e.data["agent"].as_str().unwrap_or("");
        let session = e.data["session_id"].as_str().unwrap_or("");
        if e.topic == "core.started" {
            *self.announced.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
        if let Some(mode) = e.data["permission_mode"].as_str().filter(|_| e.topic.starts_with("agents.")) {
            let now = ctx.system().now_ms();
            ctx.db().execute(
                "INSERT INTO resume_session_mode(agent,session,permission_mode,updated_at) VALUES(?,?,?,?)
                 ON CONFLICT(agent,session) DO UPDATE SET permission_mode=excluded.permission_mode,updated_at=excluded.updated_at",
                &[agent.into(), session.into(), mode.into(), now.into()],
            )?;
            ctx.db().execute(
                "DELETE FROM resume_session_mode WHERE updated_at<?",
                &[now.saturating_sub(MODE_KEPT_MS).into()],
            )?;
        }
        if e.topic == "agents.turn_end" && e.data["outcome"]["kind"] != "completed" {
            queue(ctx, &self.settings.lock().unwrap(), &e.data)?;
        } else if e.topic == "agents.prompt_submit" || e.topic == "agents.turn_end" {
            for row in ctx.db().query("SELECT id,session,context,state,process FROM resume_queue WHERE agent=? AND state IN ('waiting','running','continued')", &[agent.into()])? {
                let id=row.int(0).map_err(sql)?;
                let root=row.text(1).map_err(sql)?;
                let mut context:Value=serde_json::from_str(&row.text(2).map_err(sql)?).map_err(|e|ApiError::internal(e.to_string()))?;
                let current=context["session_id"].as_str().unwrap_or("");
                if e.topic=="agents.turn_end" {
                    if current==session { ctx.db().execute("UPDATE resume_queue SET state='completed' WHERE id=?", &[id.into()])?; }
                } else {
                    let own_prompt=e.data["automatic_continuation"]==true && row.opt_text(4).map_err(sql)?
                        .and_then(|p|serde_json::from_str::<ProcessInfo>(&p).ok()).is_some_and(|p|Some(p.pid as u64)==e.data["agent_pid"].as_u64());
                    if own_prompt { continue; }
                    if root==session && current!=session && row.text(3).map_err(sql)?=="running" {
                        if context["overlap_warned"]!=true {
                            notify(ctx,"continuation_both_active",json!({"original_session_id":root,"new_session_id":current}));
                            context["overlap_warned"]=json!(true);
                            ctx.db().execute("UPDATE resume_queue SET context=? WHERE id=?", &[context.to_string().into(),id.into()])?;
                        }
                    } else if (root==session||current==session) && row.text(3).map_err(sql)?=="waiting" {
                        ctx.db().execute("UPDATE resume_queue SET state='user_active' WHERE id=?", &[id.into()])?;
                    }
                }
            }
        } else if let Some(adapter) = agents::get(agent).filter(|_| e.topic == "agents.session_end") {
            // A person ended this conversation, so it stays ended. Only the one that would be
            // continued counts: closing the old window leaves a started fork and its retries alone.
            let reason = e.data["detail"].as_str().unwrap_or("");
            for row in ctx.db().query("SELECT id,context FROM resume_queue WHERE agent=? AND state='waiting'", &[agent.into()])? {
                let context:Value=serde_json::from_str(&row.text(1).map_err(sql)?).map_err(|e|ApiError::internal(e.to_string()))?;
                if context["session_id"]==session && adapter.ended_by_the_user(reason,context["in_a_window"]==true) {
                    ctx.db().execute("UPDATE resume_queue SET state='user_ended' WHERE id=?", &[row.int(0).map_err(sql)?.into()])?;
                }
            }
        }

        announce_active(ctx, &self.announced)
    }
    fn recover(&mut self, _: &JournalEntry, _: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        Ok(RecoverOutcome::Restored)
    }
}
