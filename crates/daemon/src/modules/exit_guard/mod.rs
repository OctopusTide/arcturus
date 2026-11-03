//! Event-driven egress protection and persistent, service-backed route restrictions.
pub mod actions;
pub mod config;
mod firewall;
pub mod probe;

use crate::module::{
    ArgKind, ArgSpec, CommandSpec, Event, EventSpec, Migration, Module, ModuleCtx, ModuleRequest, ModuleSpec,
    RecoverOutcome, Section,
};
use crate::store::JournalEntry;
use crate::sys::network::ProxyRoute;
use crate::sys::{Guard, is_access_denied, is_needs_elevation};
use actions::{Actions, SystemActions};
use config::Settings;
use proto::{ApiError, ErrorCode};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

static SPEC: ModuleSpec = ModuleSpec::new("exit_guard", Section::Protection, "Egress region protection and firewall guard")
    .commands(&[
        CommandSpec::new("settings", "Protected full paths and egress settings").agent().read_only(),
        CommandSpec::new("routes", "Read stable network interface ids for selecting a VPN").agent().read_only(),
        CommandSpec::new("configure", "Change egress protection settings (page only)").args(&[ArgSpec::new("settings", ArgKind::Json, "Partial settings object").required()]),
        CommandSpec::new("request", "Submit settings for user review; never applies them").agent().args(&[ArgSpec::new("settings", ArgKind::Json, "Proposed partial settings").required()]),
        CommandSpec::new("requests", "Pending user approvals").read_only(),
        CommandSpec::new("review", "Approve or reject a pending proposal (page only)").args(&[ArgSpec::new("request_id", ArgKind::Int, "Proposal id").required(), ArgSpec::new("approve", ArgKind::Bool, "Approve; omitted means reject")]),
        CommandSpec::new("check", "Queue an immediate check").agent(),
        CommandSpec::new("incidents", "Persistent egress events, newest first").agent().read_only(),
        CommandSpec::new("reports", "Egress appendices for away reports").agent().read_only(),
    ])
    .publishes(&[
        EventSpec::new("checked", "One egress check completed"),
        EventSpec::new("violation", "Egress wrong or unavailable; includes action result"),
        EventSpec::new("acted", "The requested egress action finished or was refused"),
        EventSpec::new("notice", "User notification and DNS/proxy warning"),
        EventSpec::new("requested", "Settings proposal awaits approval"),
        EventSpec::new("report", "An egress appendix for an away report is available"),
    ])
    .subscribes(&["network.switched", "network.up", "network.down", "mode.report", "permission.changed", "task.snapshot"])
    .migrations(&[Migration::new(1, "
        CREATE TABLE exit_guard_incidents (id INTEGER PRIMARY KEY, at INTEGER NOT NULL, kind TEXT NOT NULL, data TEXT NOT NULL);
        CREATE TABLE exit_guard_requests (id INTEGER PRIMARY KEY, at INTEGER NOT NULL, caller TEXT NOT NULL, patch TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'pending');
        CREATE TABLE exit_guard_reports (id INTEGER PRIMARY KEY, started_at INTEGER NOT NULL, ended_at INTEGER NOT NULL, data TEXT NOT NULL);
    ")]);

enum Msg {
    Configure(Settings, u64),
    Check(String),
    Safety,
    Poll,
    Stop,
}

#[derive(Default)]
struct State {
    firewall: Value,
    checks: u64,
    last: Value,
    notices: Vec<String>,
    watch_error: Option<String>,
}
struct ExitGuard {
    settings: Settings,
    state: Arc<Mutex<State>>,
    revision: Arc<AtomicU64>,
    tx: Option<mpsc::Sender<Msg>>,
    worker: Option<JoinHandle<()>>,
    watch: Option<Box<dyn Guard>>,
    actions: Arc<dyn Actions>,
    stopped: Arc<AtomicBool>,
}

pub fn create() -> Box<dyn Module> {
    create_with_actions(Arc::new(SystemActions))
}
/// Injection is for fake implementations in automated tests, never a protocol option.
pub fn create_with_actions(actions: Arc<dyn Actions>) -> Box<dyn Module> {
    Box::new(ExitGuard {
        settings: Settings::default(),
        state: Arc::new(Mutex::new(State::default())),
        revision: Arc::new(AtomicU64::new(0)),
        tx: None,
        worker: None,
        watch: None,
        actions,
        stopped: Arc::new(AtomicBool::new(false)),
    })
}
impl ExitGuard {
    fn send(&self, msg: Msg) -> Result<(), ApiError> {
        self.tx
            .as_ref()
            .ok_or_else(|| ApiError::internal("egress worker is stopped"))?
            .send(msg)
            .map_err(|_| ApiError::internal("egress worker is stopped"))
    }
    fn configure(&mut self, patch: &Value, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        let next = config::patch(&self.settings, patch)?;
        ctx.settings().set("settings", &json!(next))?;
        self.settings = next;
        let revision = self.revision.fetch_add(1, Ordering::SeqCst) + 1;
        self.send(Msg::Configure(self.settings.clone(), revision))?;
        Ok(json!({"settings": self.settings, "queued": true, "revision": revision}))
    }
    fn stop_worker(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.watch.take();
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Stop);
        }
        if let Some(t) = self.worker.take() {
            let _ = t.join();
        }
    }
}
impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

impl Module for ExitGuard {
    fn spec(&self) -> &'static ModuleSpec {
        &SPEC
    }
    fn start(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        self.settings = config::load(ctx)?;
        config::validate(&self.settings).map_err(ApiError::invalid_args)?;
        let (tx, rx) = mpsc::channel();
        let notify = tx.clone();
        match ctx.system().network().watch(Arc::new(move |change| {
            let _ = notify.send(Msg::Check(format!("system:{change:?}")));
        })) {
            Ok(w) => self.watch = Some(w),
            Err(e) => self.state.lock().unwrap().watch_error = Some(e.message),
        }
        let mut worker = Worker {
            settings: self.settings.clone(),
            revision: 0,
            node: None,
            mode: None,
            control_error: false,
            last_control: None,
            last_public: None,
            retry_at: None,
            retry_delay_ms: 1000,
            ctx: ctx.clone(),
            state: self.state.clone(),
            current_revision: self.revision.clone(),
            actions: self.actions.clone(),
            stopped: self.stopped.clone(),
        };
        self.worker = Some(
            std::thread::Builder::new()
                .name("egress-check".into())
                .spawn(move || worker.run(rx))
                .map_err(|e| ApiError::internal(format!("egress worker: {e}")))?,
        );
        self.tx = Some(tx);
        self.send(Msg::Check("startup".into()))
    }
    fn stop(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        self.stop_worker();
        let lock = firewall::lock_on_stop(ctx);
        let release = actions::release(ctx, None);
        lock.and(release)
    }
    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        match req.cmd {
            "settings" => Ok(json!(self.settings)),
            "routes" => Ok(json!({"interfaces":ctx.system().network().interfaces()?})),
            "configure" => self.configure(&req.args["settings"], ctx),
            "check" => {
                self.send(Msg::Check("manual".into()))?;
                Ok(json!({"queued": true}))
            }
            "request" => {
                let patch = &req.args["settings"];
                config::patch(&self.settings, patch)?;
                let id = ctx.db().transaction(|db| {
                    db.execute(
                        "INSERT INTO exit_guard_requests (at, caller, patch) VALUES (?, ?, ?)",
                        &[
                            ctx.system().now_ms().into(),
                            json!(req.caller).to_string().into(),
                            patch.to_string().into(),
                        ],
                    )?;
                    db.query("SELECT last_insert_rowid()", &[])?.remove(0).int(0)
                })?;
                ctx.publish("requested", json!({"request_id": id, "caller": req.caller}))?;
                Ok(json!({"request_id": id, "state": "pending", "applied": false}))
            }
            "requests" => {
                let rows = ctx.db().query("SELECT id, at, caller, patch FROM exit_guard_requests WHERE state = 'pending' ORDER BY id DESC LIMIT 100", &[])?;
                let items: Vec<Value> = rows.iter().map(|r| json!({"id": r.int(0).ok(), "at": r.int(1).ok(), "caller": decode(r.text(2).ok()), "settings": decode(r.text(3).ok())})).collect();
                Ok(json!({"requests": items}))
            }
            "review" => {
                let id = req.args["request_id"].as_i64().unwrap();
                let rows = ctx.db().query(
                    "SELECT patch FROM exit_guard_requests WHERE id = ? AND state = 'pending'",
                    &[id.into()],
                )?;
                let row = rows
                    .first()
                    .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "pending proposal not found"))?;
                let approve = req.args["approve"].as_bool() == Some(true);
                if approve {
                    self.configure(&decode(row.text(0).ok()), ctx)?;
                }
                ctx.db().execute(
                    "UPDATE exit_guard_requests SET state = ? WHERE id = ?",
                    &[if approve { "approved" } else { "rejected" }.into(), id.into()],
                )?;
                Ok(json!({"request_id": id, "applied": approve}))
            }
            "incidents" => Ok(json!({"incidents": incidents(ctx, None)?})),
            "reports" => {
                let rows = ctx.db().query(
                    "SELECT data FROM exit_guard_reports ORDER BY id DESC LIMIT 20",
                    &[],
                )?;
                Ok(json!({"reports": rows.iter().map(|r| decode(r.text(0).ok())).collect::<Vec<_>>()}))
            }
            _ => Err(ApiError::internal("unrouted egress command")),
        }
    }
    fn status(&self, _ctx: &ModuleCtx) -> Value {
        let s = self.state.lock().unwrap();
        let mut notices = s.notices.clone();
        if !self.settings.programs.is_empty() {
            notices.push("Rules cover listed executable paths only. Add child runtimes/tools explicitly; this is not a process-tree sandbox.".into());
        }
        let fw = if s.firewall.is_null() {
            json!({"state":"pending","applied":false})
        } else {
            s.firewall.clone()
        };
        if fw["state"] == "needs_admin" {
            notices.push("Administrator service unavailable: firewall protection is not applied.".into());
        }
        if self.settings.programs.iter().any(|p| {
            p.firewall_route
                .as_ref()
                .or(self.settings.firewall_route.as_ref())
                .is_none()
        }) {
            notices.push(
                "Choose a VPN interface or local proxy firewall route; the safe default blocks all traffic."
                    .into(),
            );
        }
        if let Some(e) = &s.watch_error {
            notices.push(format!("Network change listener unavailable: {e}"));
        }
        if !self.settings.programs.is_empty()
            && self
                .settings
                .programs
                .iter()
                .any(|p| !p.region.as_ref().unwrap_or(&self.settings.region).configured())
        {
            notices.push("Select a country, IP or node for every protected program.".into());
        }
        json!({"implemented": true, "phase": 2, "firewall": fw, "programs": self.settings.programs, "checks": s.checks, "last_check": s.last, "periodic_enabled": self.settings.interval_seconds.is_some(), "summary": format!("{} protected program(s); firewall {}", self.settings.programs.len(),fw["state"].as_str().unwrap_or("pending")), "notices": notices})
    }
    fn recover(&mut self, entry: &JournalEntry, ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        if let Some(path) = entry.key.strip_prefix("firewall:") {
            if !ctx.configured_enabled() {
                firewall::restore(ctx, entry)?;
                return Ok(RecoverOutcome::Restored);
            }
            let configured = config::load(ctx)?;
            if configured
                .programs
                .iter()
                .any(|p| config::path_key(&p.path).ok().as_deref() == Some(path))
            {
                ctx.elevated().set_exit_blocked(path, true)?;
                return Ok(RecoverOutcome::Adopted);
            }
            firewall::restore(ctx, entry)?;
        } else {
            actions::recover(ctx, entry)?;
        }
        Ok(RecoverOutcome::Restored)
    }
    fn on_event(&mut self, event: &Event, ctx: &ModuleCtx) -> Result<(), ApiError> {
        if event.topic == "mode.report" {
            let start = event.data["started_at"].as_i64().unwrap_or(event.ts);
            let end = event.data["ended_at"].as_i64().unwrap_or(event.ts);
            let report = json!({"started_at": start, "ended_at": end, "incidents": incidents(ctx, Some((start,end)))?, "firewall": self.state.lock().unwrap().firewall});
            let updated = ctx.db().execute(
                "UPDATE exit_guard_reports SET data = ? WHERE started_at = ? AND ended_at = ?",
                &[report.to_string().into(), start.into(), end.into()],
            )?;
            if updated == 0 {
                ctx.db().execute(
                    "INSERT INTO exit_guard_reports (started_at, ended_at, data) VALUES (?, ?, ?)",
                    &[start.into(), end.into(), report.to_string().into()],
                )?;
            }
            ctx.publish("report", report)
        } else if event.topic == "permission.changed" || event.topic == "task.snapshot" {
            self.send(Msg::Safety)
        } else {
            self.send(Msg::Check(event.topic.clone()))
        }
    }
}
fn decode(text: Option<String>) -> Value {
    text.and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}
fn incidents(ctx: &ModuleCtx, window: Option<(i64, i64)>) -> Result<Vec<Value>, ApiError> {
    let rows = if let Some((a, b)) = window {
        ctx.db().query(
            "SELECT data FROM exit_guard_incidents WHERE at >= ? AND at <= ? ORDER BY id",
            &[a.into(), b.into()],
        )?
    } else {
        ctx.db().query(
            "SELECT data FROM exit_guard_incidents ORDER BY id DESC LIMIT 100",
            &[],
        )?
    };
    Ok(rows.iter().map(|r| decode(r.text(0).ok())).collect())
}

struct Worker {
    settings: Settings,
    revision: u64,
    node: Option<String>,
    mode: Option<String>,
    control_error: bool,
    last_control: Option<i64>,
    last_public: Option<i64>,
    retry_at: Option<i64>,
    retry_delay_ms: i64,
    ctx: ModuleCtx,
    state: Arc<Mutex<State>>,
    current_revision: Arc<AtomicU64>,
    actions: Arc<dyn Actions>,
    stopped: Arc<AtomicBool>,
}
impl Worker {
    fn run(&mut self, rx: mpsc::Receiver<Msg>) {
        self.reconcile_firewall();
        loop {
            if self.stopped.load(Ordering::SeqCst) || !self.ctx.is_active() {
                return;
            }
            let first = match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(m) => m,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => Msg::Poll,
            };
            let mut reason = None;
            let mut messages = vec![first];
            messages.extend(rx.try_iter()); // coalesce bursts without losing configuration updates
            for msg in messages {
                match msg {
                    Msg::Stop => return,
                    Msg::Check(r) => reason = Some(r),
                    Msg::Configure(settings, revision) => {
                        for previous in &self.settings.programs {
                            if !settings.programs.iter().any(|p| {
                                config::path_key(&p.path).ok() == config::path_key(&previous.path).ok()
                            }) {
                                if let Err(e) = actions::release(&self.ctx, Some(&previous.path)) {
                                    self.state
                                        .lock()
                                        .unwrap()
                                        .notices
                                        .push(format!("Cannot resume removed program: {}", e.message));
                                }
                            }
                        }
                        if self.settings.control != settings.control {
                            self.node = None;
                            self.mode = None;
                            self.last_control = None;
                        }
                        self.settings = settings;
                        self.revision = revision;
                        reason = Some("settings_changed".into());
                        self.reconcile_firewall();
                    }
                    Msg::Poll => {}
                    Msg::Safety => {
                        if let Err(e) = actions::release_protected(&self.ctx) {
                            self.state
                                .lock()
                                .unwrap()
                                .notices
                                .push(format!("Cannot reconcile process safety: {}", e.message));
                        }
                    }
                }
            }
            if self.settings.programs.is_empty() {
                continue;
            }
            let now = self.ctx.system().now_ms();
            if self.retry_at.is_some_and(|at| now >= at) {
                self.retry_enforcement();
            }
            let control_due = self.settings.control.as_ref().is_some_and(|c| {
                self.last_control
                    .is_none_or(|t| now.saturating_sub(t) >= c.poll_seconds as i64 * 1000)
            });
            if control_due && self.read_control() {
                reason = Some("proxy_node_changed".into());
            }
            if self.settings.interval_seconds.is_some_and(|n| {
                self.last_public
                    .is_some_and(|t| now.saturating_sub(t) >= n as i64 * 1000)
            }) {
                reason = Some("periodic".into());
            }
            if let Some(reason) = reason {
                if let Err(e) = self.check(&reason) {
                    self.state.lock().unwrap().notices = vec![format!("Egress check failed: {}", e.message)];
                }
            }
        }
    }
    fn retry_enforcement(&mut self) {
        self.reconcile_firewall();
        let mut last = self.state.lock().unwrap().last.clone();
        let mut failed = false;
        if let Some(items) = last["programs"].as_array_mut() {
            for item in items {
                let Some(path) = item["path"].as_str().map(str::to_owned) else {
                    continue;
                };
                let valid = item["matches"] == true;
                item["firewall"] = firewall::finish(&self.ctx, &path, valid)
                    .unwrap_or_else(|e| json!({"applied":false,"state":"failed","message":e.message}));
                failed |= item["firewall"]["applied"] != true;
                if !valid && item["action"] == "block" && item["firewall"]["applied"] == true {
                    item["action_result"] = json!({"state":"applied","blocked":true});
                }
            }
        }
        self.state.lock().unwrap().last = last;
        self.retry_at = failed.then(|| self.ctx.system().now_ms().saturating_add(self.retry_delay_ms));
        self.retry_delay_ms = if failed {
            (self.retry_delay_ms * 2).min(60_000)
        } else {
            1000
        };
    }
    fn read_control(&mut self) -> bool {
        self.last_control = Some(self.ctx.system().now_ms());
        let c = self.settings.control.as_ref().unwrap();
        let result = self
            .ctx
            .system()
            .network()
            .get(&c.url, &ProxyRoute::Direct)
            .and_then(|s| {
                serde_json::from_str::<Value>(&s)
                    .map_err(|_| ApiError::internal("local control API returned invalid JSON"))
            });
        let (node, mode, error) = match result {
            Ok(v) => {
                let node = v
                    .pointer(&c.node_pointer)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let missing = node.is_none();
                (
                    node,
                    v.pointer(&c.mode_pointer)
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    missing,
                )
            }
            Err(_) => (None, None, true),
        };
        let changed = self.node != node || self.mode != mode || self.control_error != error;
        self.node = node;
        self.mode = mode;
        self.control_error = error;
        changed
    }
    fn check(&mut self, reason: &str) -> Result<(), ApiError> {
        // Keep established route restrictions while rechecking; unchanged valid routes stay usable.
        self.reconcile_firewall();
        let net = self.ctx.system().network();
        let mut routes = BTreeMap::new();
        let mut programs = Vec::new();
        // Lookup once per distinct route, but evaluate the target for each full path.
        for p in &self.settings.programs {
            if self.stopped.load(Ordering::SeqCst)
                || !self.ctx.is_active()
                || self.current_revision.load(Ordering::SeqCst) != self.revision
            {
                return Ok(());
            }
            let route = p.proxy.as_ref().unwrap_or(&self.settings.proxy);
            let guard = p
                .firewall_route
                .as_ref()
                .or(self.settings.firewall_route.as_ref())
                .cloned()
                .unwrap_or_default();
            let key = serde_json::to_string(&(route, &guard)).unwrap();
            if !routes.contains_key(&key) {
                routes.insert(key.clone(), probe::lookup_guarded(net, route, &guard));
            }
        }
        let dns = net.dns().unwrap_or_else(|e| crate::sys::network::DnsAssessment {
            state: "unknown".into(),
            details: vec![e.message],
        });
        if self.stopped.load(Ordering::SeqCst)
            || !self.ctx.is_active()
            || self.current_revision.load(Ordering::SeqCst) != self.revision
        {
            return Ok(());
        }
        let mut notices = Vec::new();
        if dns.state != "vpn_route" {
            notices.push(if dns.state == "bypass" { "DNS resolver traffic has a route outside the VPN; DNS leakage is possible." } else { "Cannot confirm that DNS follows the VPN (local stub, DoH or route information unavailable)." }.to_owned());
        }
        let proxy_active = self.settings.programs.iter().any(|p| {
            !matches!(
                p.proxy.as_ref().unwrap_or(&self.settings.proxy),
                ProxyRoute::Direct
            )
        });
        if self
            .mode
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case("rule"))
        {
            notices.push("Proxy rule mode: direct traffic sent by the proxy is outside per-application firewall control. Use global mode.".into());
        } else if proxy_active
            && self
                .mode
                .as_deref()
                .is_none_or(|m| !m.eq_ignore_ascii_case("global"))
        {
            notices.push("Proxy mode is unverified: rule-mode direct traffic cannot be covered by per-application firewall rules.".into());
        }
        if self.control_error {
            notices.push("Local proxy status API is unavailable; node identity cannot be verified.".into());
        }
        let at = self.ctx.system().now_ms();
        let mut notified = false;
        let mut enforcement_failed = false;
        for p in &self.settings.programs {
            if self.stopped.load(Ordering::SeqCst)
                || !self.ctx.is_active()
                || self.current_revision.load(Ordering::SeqCst) != self.revision
            {
                return Ok(());
            }
            let target = p.region.as_ref().unwrap_or(&self.settings.region);
            let route = p.proxy.as_ref().unwrap_or(&self.settings.proxy);
            let selected = p
                .firewall_route
                .as_ref()
                .or(self.settings.firewall_route.as_ref())
                .cloned()
                .unwrap_or_default();
            let result = &routes[&serde_json::to_string(&(route, &selected)).unwrap()];
            let unavailable = result.is_err() || (target.node.is_some() && self.node.is_none());
            let valid = !unavailable && probe::matches(target, result.as_ref().ok(), self.node.as_deref());
            let action = if unavailable || !target.configured() {
                config::Action::Block
            } else {
                p.action
            };
            let guard=firewall::finish(&self.ctx,&p.path,valid).unwrap_or_else(|e|json!({"state":if is_needs_elevation(&e){"needs_admin"}else{"failed"},"applied":false,"message":e.message}));
            {
                let mut state = self.state.lock().unwrap();
                if let Some(items) = state.firewall["programs"].as_array_mut() {
                    if let Some(f) = items.iter_mut().find(|f| f["path"] == p.path) {
                        let mut update = guard.clone();
                        update["path"] = json!(p.path);
                        *f = update;
                    }
                }
            }
            let mut item = json!({"path": p.path, "target": target, "route": route, "exit": result.as_ref().ok(), "errors": result.as_ref().err(), "node": self.node, "matches": valid, "action": action, "configured_action": p.action, "unavailable": unavailable, "firewall":guard,"at": at, "reason": reason});
            if !valid {
                // Persist and announce the problem before any process action. The same
                // row is completed afterwards, so a crash cannot erase the intent.
                item["action_result"] = json!({"state":"pending"});
                let id = self.record_intent("violation", &item, at)?;
                item["incident_id"] = json!(id);
                self.ctx.publish("violation", item.clone())?;
                // Never claim enforcement succeeded merely because it was requested.
                match self.actions.apply(&self.ctx, &p.path, action) {
                    Ok(result) => item["action_result"] = result,
                    Err(e) => {
                        item["action_result"] = json!({"state": if is_needs_elevation(&e) || is_access_denied(&e) {"needs_admin"} else if e.code == ErrorCode::NotImplemented {"unsupported"} else if e.code == ErrorCode::Denied {"denied"} else {"failed"}, "message": e.message})
                    }
                }
                let failed = item["firewall"]["applied"] != true
                    || matches!(
                        item["action_result"]["state"].as_str(),
                        Some("failed" | "needs_admin" | "denied" | "unsupported")
                    );
                enforcement_failed |= failed;
                if !notified {
                    let message = if failed {
                        "出口异常，但保护操作失败，当前无法确认已封网；将自动重试。请查看出口保护状态。"
                    } else {
                        "出口异常，已执行保护；详情请查看出口保护状态。"
                    };
                    if let Err(e) = self.ctx.system().notify_user("Arcturus 出口保护", message) {
                        notices.push(format!("Desktop notification unavailable: {}", e.message));
                    }
                    notified = true;
                }
                self.ctx.db().execute(
                    "UPDATE exit_guard_incidents SET data = ? WHERE id = ?",
                    &[item.to_string().into(), id.into()],
                )?;
                self.ctx.publish("acted", item.clone())?;
            } else {
                item["action_result"] = match actions::release(&self.ctx, Some(&p.path)) {
                    Ok(()) => json!({"state":"not_needed"}),
                    Err(e) => json!({"state":"recovery_pending","message":e.message}),
                };
            }
            enforcement_failed |= item["firewall"]["applied"] != true;
            programs.push(item);
        }
        if enforcement_failed {
            self.retry_at = Some(at.saturating_add(self.retry_delay_ms));
            self.retry_delay_ms = (self.retry_delay_ms * 2).min(60_000);
        } else {
            self.retry_at = None;
            self.retry_delay_ms = 1000;
        }
        let previous = self.state.lock().unwrap().notices.clone();
        for text in &notices {
            if !previous.contains(text) {
                let data = json!({"message": text, "at": at});
                self.record("notice", &data, at)?;
                self.ctx.publish("notice", data)?;
            }
        }
        if !notified && !notices.is_empty() && notices != previous {
            let message = notices.join(" ");
            let delivery = self
                .ctx
                .system()
                .notify_user("Arcturus egress protection", &message);
            if let Err(e) = delivery {
                notices.push(format!(
                    "Desktop notification unavailable; see exit_guard.notice/status: {}",
                    e.message
                ));
            }
        }
        let result = json!({"at": at, "reason": reason, "programs": programs, "dns": dns, "node": self.node, "proxy_mode": self.mode});
        self.ctx.publish("checked", result.clone())?;
        let mut state = self.state.lock().unwrap();
        if let Some(guards) = state.firewall["programs"].as_array() {
            let all = guards.iter().all(|g| g["applied"] == true);
            let admin = guards.iter().all(|g| g["state"] == "needs_admin");
            state.firewall["applied"] = json!(all && !guards.is_empty());
            state.firewall["state"] = json!(if admin {
                "needs_admin"
            } else if all {
                "applied"
            } else {
                "partial"
            });
        }
        state.checks += 1;
        state.last = result;
        state.notices = notices;
        self.last_public = Some(self.ctx.system().now_ms());
        Ok(())
    }
    fn record(&self, kind: &str, data: &Value, at: i64) -> Result<(), ApiError> {
        self.record_intent(kind, data, at).map(|_| ())
    }
    fn reconcile_firewall(&self) {
        let state=firewall::reconcile(&self.ctx,&self.settings).unwrap_or_else(|e|json!({"state":if is_needs_elevation(&e){"needs_admin"}else{"failed"},"applied":false,"message":e.message}));
        self.state.lock().unwrap().firewall = state;
    }
    fn record_intent(&self, kind: &str, data: &Value, at: i64) -> Result<i64, ApiError> {
        self.ctx.db().transaction(|db| {
            db.execute(
                "INSERT INTO exit_guard_incidents (at, kind, data) VALUES (?, ?, ?)",
                &[at.into(), kind.into(), data.to_string().into()],
            )?;
            db.query("SELECT last_insert_rowid()", &[])?.remove(0).int(0)
        })
    }
}
