//! Connectivity checks share the agent's proxy route. Only away/custom policy may switch
//! saved profiles or adapter metrics; captive portals and API-only outages never switch.
use crate::module::{
    ArgKind, ArgSpec, CommandSpec, Event, EventSpec, Migration, Module, ModuleCtx, ModuleRequest, ModuleSpec,
    RecoverOutcome, Section,
};
use crate::modules::settings;
use crate::store::JournalEntry;
use crate::sys::network::{LinkReading, ProxyRoute, WifiProfile};
use crate::sys::{Connectivity, Guard};
use proto::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

static SPEC:ModuleSpec=ModuleSpec::new("network",Section::Usage,"Connectivity and saved-network failover")
    .commands(&[
        CommandSpec::new("show","Current probes, route and mobile traffic").agent().read_only(),
        CommandSpec::new("profiles","Saved network profiles, without passwords").agent().read_only(),
        CommandSpec::new("settings","Connectivity settings").agent().read_only(),
        CommandSpec::new("configure","Change routes, hotspot order and probes (page only)").args(&[ArgSpec::new("settings",ArgKind::Json,"Partial settings object").required()]),
        CommandSpec::new("check","Check connectivity now").agent(),
        CommandSpec::new("history","Recent outages and line changes").agent().read_only().args(&[ArgSpec::new("limit",ArgKind::Uint,"1..=100; default 20")]),
    ])
    .publishes(&[EventSpec::new("down","Internet/API unreachable after consecutive failures"),EventSpec::new("switched","The active line changed"),
        EventSpec::new("up","Connectivity restored"),EventSpec::new("reading","Current connectivity"),EventSpec::new("notice","User action needed")])
    .subscribes(&["core.started","mode.changed"])
    .migrations(&[Migration::new(1,"CREATE TABLE network_outages (id INTEGER PRIMARY KEY, started_at INTEGER NOT NULL, ended_at INTEGER, reason TEXT NOT NULL, changes TEXT NOT NULL DEFAULT '[]', mobile_bytes INTEGER NOT NULL DEFAULT 0);")]);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Line {
    pub interface: String,
    pub profile: String,
    #[serde(default)]
    pub hotspot: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkSettings {
    pub poll_millis: u32,
    pub failures: u32,
    pub internet_url: String,
    pub internet_expected: String,
    pub api_urls: Vec<String>,
    pub proxy: ProxyRoute,
    pub lines: Vec<Line>,
    pub backup_interface: Option<String>,
    pub connect_timeout_seconds: u32,
    pub retry_main_seconds: u32,
    pub mobile_limit_mb: Option<u64>,
}
impl Default for NetworkSettings {
    fn default() -> Self {
        Self {
            poll_millis: 1500,
            failures: 3,
            internet_url: "http://www.msftconnecttest.com/connecttest.txt".into(),
            internet_expected: "Microsoft Connect Test".into(),
            api_urls: Vec::new(),
            proxy: ProxyRoute::System,
            lines: Vec::new(),
            backup_interface: None,
            connect_timeout_seconds: 10,
            retry_main_seconds: 600,
            mobile_limit_mb: None,
        }
    }
}
impl NetworkSettings {
    fn check(&self) -> Result<(), String> {
        if !(500..=60_000).contains(&self.poll_millis)
            || !(1..=10).contains(&self.failures)
            || !(1..=30).contains(&self.connect_timeout_seconds)
            || !(30..=86400).contains(&self.retry_main_seconds)
            || self.mobile_limit_mb == Some(0)
        {
            return Err("invalid polling, failure, connection, retry or traffic limit".into());
        }
        if self.api_urls.len() > 8 || self.lines.len() > 128 {
            return Err("too many endpoints or lines".into());
        }
        for url in std::iter::once(&self.internet_url).chain(&self.api_urls) {
            if !(url.starts_with("https://")
                || url == "http://www.msftconnecttest.com/connecttest.txt"
                || url.starts_with("http://127.0.0.1:")
                || url.starts_with("http://[::1]:"))
                || url.contains(['@', '\r', '\n', '\0'])
                || url.len() > 2048
            {
                return Err(
                    "probe URLs must be HTTPS, the fixed NCSI probe, or literal loopback; no credentials"
                        .into(),
                );
            }
        }
        let mut seen = BTreeSet::new();
        for line in &self.lines {
            proto::admin::Operation::InterfaceMetric {
                interface: line.interface.clone(),
                metric: 1,
            }
            .validate(0)
            .map_err(|e| e.message)?;
            if line.profile.is_empty()
                || line.profile.encode_utf16().count() > 256
                || line.profile.contains('\0')
                || !seen.insert((interface_key(&line.interface), line.profile.clone()))
            {
                return Err("invalid or duplicate saved line".into());
            }
        }
        if let Some(id) = &self.backup_interface {
            proto::admin::Operation::InterfaceMetric {
                interface: id.clone(),
                metric: 1,
            }
            .validate(0)
            .map_err(|e| e.message)?;
        }
        Ok(())
    }
}
struct Attempt {
    line: Line,
    since: i64,
    previous: Option<Line>,
    main_trial: bool,
}
struct Inner {
    settings: NetworkSettings,
    failed: u32,
    online: Option<bool>,
    outage: Option<i64>,
    down_since: Option<i64>,
    reason: String,
    changes: Vec<Value>,
    attempt: Option<Attempt>,
    main: Option<Line>,
    main_disappeared: bool,
    last_main_trial: i64,
    last_attempt: BTreeMap<String, i64>,
    counters: BTreeMap<String, u64>,
    mobile_bytes: u64,
    outage_mobile_start: u64,
    last: Value,
    notice: Option<String>,
    backup_since: Option<i64>,
    main_link_was_down: bool,
    published: Value,
    mobile_period: String,
}
enum Msg {
    Wake,
    Stop,
}
struct NetworkModule {
    shared: Arc<Mutex<Inner>>,
    tx: Option<mpsc::Sender<Msg>>,
    worker: Option<JoinHandle<()>>,
    watch: Option<Box<dyn Guard>>,
}
fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
pub fn create() -> Box<dyn Module> {
    Box::new(NetworkModule {
        shared: Arc::new(Mutex::new(Inner {
            settings: NetworkSettings::default(),
            failed: 0,
            online: None,
            outage: None,
            down_since: None,
            reason: String::new(),
            changes: Vec::new(),
            attempt: None,
            main: None,
            main_disappeared: false,
            last_main_trial: 0,
            last_attempt: BTreeMap::new(),
            counters: BTreeMap::new(),
            mobile_bytes: 0,
            outage_mobile_start: 0,
            last: Value::Null,
            notice: None,
            backup_since: None,
            main_link_was_down: false,
            published: Value::Null,
            mobile_period: String::new(),
        })),
        tx: None,
        worker: None,
        watch: None,
    })
}
fn interface_key(id: &str) -> String {
    id.trim_matches(['{', '}']).to_ascii_lowercase()
}
fn normalize_settings(s: &mut NetworkSettings) {
    for line in &mut s.lines {
        line.interface = interface_key(&line.interface);
    }
    s.backup_interface = s.backup_interface.as_deref().map(interface_key);
}
fn key(line: &Line) -> String {
    format!("{}/{}", interface_key(&line.interface), line.profile)
}
fn lines(g: &Inner, profiles: &[WifiProfile]) -> Vec<Line> {
    if !g.settings.lines.is_empty() {
        return g.settings.lines.clone();
    }
    let mut profiles = profiles.to_vec();
    profiles.sort_by_key(|p| p.hotspot_hint);
    profiles
        .into_iter()
        .map(|p| Line {
            interface: p.interface,
            profile: p.name,
            hotspot: p.hotspot_hint,
        })
        .collect()
}
fn current(g: &Inner, profiles: &[WifiProfile]) -> Option<Line> {
    let ls = lines(g, profiles);
    ls.into_iter().find(|l| {
        profiles
            .iter()
            .any(|p| p.connected && p.interface == l.interface && p.name == l.profile)
    })
}
fn notice(g: &mut Inner, ctx: &ModuleCtx, reason: &str, details: Value) {
    if g.notice.as_deref() != Some(reason) {
        g.notice = Some(reason.into());
        let _ = ctx.publish("notice", json!({"reason":reason,"details":details}));
        let _ = ctx.system().notify_user("Arcturus 网络", reason);
    }
}
fn save_outage(g: &Inner, ctx: &ModuleCtx, ended: Option<i64>) -> Result<(), ApiError> {
    if let Some(id) = g.outage {
        ctx.db().execute(
            "UPDATE network_outages SET ended_at=?,changes=?,mobile_bytes=? WHERE id=?",
            &[
                ended.into(),
                json!(g.changes).to_string().into(),
                (g.mobile_bytes.saturating_sub(g.outage_mobile_start) as i64).into(),
                id.into(),
            ],
        )?;
    }
    Ok(())
}
fn publish_reading(g: &mut Inner, ctx: &ModuleCtx) -> Result<(), ApiError> {
    {
        let mut a = ctx.arbitration();
        a.network_online = g.online;
        a.network_outage = g.outage;
    }
    let value =
        json!({"online":g.online,"outage_id":g.outage,"reason":g.reason,"mobile_bytes":g.mobile_bytes});
    if value != g.published {
        ctx.publish("reading", value.clone())?;
        g.published = value;
    }
    Ok(())
}
fn mark_mobile(g: &mut Inner, ctx: &ModuleCtx, line: &Line) -> Result<(), ApiError> {
    if !line.hotspot {
        return Ok(());
    }
    let journal_key = format!("metered:{}", key(line));
    if ctx.journal().open_entry(&journal_key)?.is_some() {
        return Ok(());
    }
    let metered = ctx.system().connectivity()?.metered;
    if metered {
        return Ok(());
    }
    let entry = ctx.journal().record(
        &journal_key,
        &json!({"line":line,"metered":metered}),
        Some(&json!(true)),
    )?;
    match ctx
        .system()
        .network()
        .set_profile_metered(&line.interface, &line.profile, true)
    {
        Ok(()) => Ok(()),
        Err(e) => {
            if crate::sys::is_needs_elevation(&e) {
                ctx.journal().mark_restored(entry.id)?;
            }
            notice(g, ctx, "hotspot_metered_not_applied", json!(e));
            Ok(())
        }
    }
}
fn start_switch(
    g: &mut Inner,
    ctx: &ModuleCtx,
    line: Line,
    current: Option<Line>,
    main_trial: bool,
) -> Result<(), ApiError> {
    let a = ctx.arbitration();
    if !a.hotspot {
        return Ok(());
    }
    if line.hotspot
        && g.settings
            .mobile_limit_mb
            .is_some_and(|limit| g.mobile_bytes >= limit.saturating_mul(1024 * 1024))
    {
        return Err(ApiError::new(
            ErrorCode::Denied,
            "monthly mobile data limit reached",
        ));
    }
    let original = current.clone();
    let journal_key = format!("profile:{}", line.interface);
    let entry=ctx.journal().record(&journal_key,&json!({"interface":line.interface,"profile":original.as_ref().filter(|p|p.interface==line.interface).map(|l|l.profile.clone())}),Some(&json!({"profile":line.profile})))?;
    let result = ctx
        .system()
        .network()
        .connect_profile(&line.interface, &line.profile);
    drop(a);
    g.last_attempt.insert(key(&line), ctx.system().now_ms());
    if let Err(e) = result {
        if crate::sys::is_needs_elevation(&e) {
            ctx.journal().mark_restored(entry.id)?;
        }
        return Err(e);
    }
    g.attempt = Some(Attempt {
        line,
        since: ctx.system().now_ms(),
        previous: current,
        main_trial,
    });
    Ok(())
}
fn restore_entry(entry: &JournalEntry, ctx: &ModuleCtx) -> Result<(), ApiError> {
    if entry.key.starts_with("profile:") {
        let interface = entry.original["interface"]
            .as_str()
            .ok_or_else(|| ApiError::internal("bad saved WLAN journal"))?;
        if let Some(profile) = entry.original["profile"].as_str() {
            let profiles = ctx.system().network().profiles()?;
            let current = profiles.iter().find(|p| p.interface == interface && p.connected);
            if current.is_none_or(|p| {
                Some(p.name.as_str()) == entry.applied.as_ref().and_then(|v| v["profile"].as_str())
                    || p.name == profile
            }) {
                ctx.system().network().connect_profile(interface, profile)?;
            }
        } else {
            let profiles = ctx.system().network().profiles()?;
            if profiles.iter().any(|p| {
                p.interface == interface
                    && p.connected
                    && Some(p.name.as_str()) == entry.applied.as_ref().and_then(|v| v["profile"].as_str())
            }) {
                ctx.system().network().disconnect(interface)?;
            }
        }
    } else if entry.key.starts_with("metered:") {
        let line: Line = serde_json::from_value(entry.original["line"].clone())
            .map_err(|e| ApiError::internal(e.to_string()))?;
        ctx.system().network().set_profile_metered(
            &line.interface,
            &line.profile,
            entry.original["metered"].as_bool().unwrap_or(false),
        )?;
    } else if entry.key.starts_with("metric:") {
        let reading: LinkReading =
            serde_json::from_value(entry.original.clone()).map_err(|e| ApiError::internal(e.to_string()))?;
        ctx.elevated().restore_interface_metric(
            &reading.interface,
            reading.metric,
            reading.automatic_metric,
        )?;
    } else {
        return Err(ApiError::internal("unknown network journal key"));
    }
    Ok(())
}
fn restore(ctx: &ModuleCtx) -> Result<(), ApiError> {
    let mut error = None;
    for e in ctx.journal().open_entries()? {
        match restore_entry(&e, ctx).and_then(|_| ctx.journal().mark_restored(e.id)) {
            Ok(()) => {}
            Err(e) => error = Some(e),
        }
    }
    error.map_or(Ok(()), Err)
}
fn prefer_backup(g: &mut Inner, ctx: &ModuleCtx, readings: &[LinkReading]) -> Result<bool, ApiError> {
    let Some(id) = &g.settings.backup_interface else {
        return Ok(false);
    };
    let Some(backup) = readings.iter().find(|r| &r.interface == id && r.up) else {
        return Ok(false);
    };
    if ctx.journal().open_entry(&format!("metric:{id}"))?.is_some() {
        return Ok(false);
    }
    let a = ctx.arbitration();
    if !a.hotspot {
        return Ok(false);
    }
    for r in readings.iter().filter(|r| {
        r.up && (&r.interface == id || g.main.as_ref().is_some_and(|m| m.interface == r.interface))
    }) {
        let metric = if r.interface == backup.interface { 5 } else { 100 };
        let entry = ctx.journal().record(
            &format!("metric:{}", r.interface),
            &json!(r),
            Some(&json!(metric)),
        )?;
        if let Err(e) = ctx.elevated().set_interface_metric(&r.interface, metric) {
            if crate::sys::is_needs_elevation(&e) {
                ctx.journal().mark_restored(entry.id)?;
            }
            return Err(e);
        }
    }
    drop(a);
    g.backup_since = Some(ctx.system().now_ms());
    let data = json!({"interface":id,"reason":"backup_metric","outage_id":g.outage});
    g.changes.push(data.clone());
    ctx.publish("switched", data)?;
    save_outage(g, ctx, None)?;
    Ok(true)
}
fn cycle(g: &mut Inner, ctx: &ModuleCtx) -> Result<Value, ApiError> {
    if !ctx.is_active() {
        return Ok(g.last.clone());
    }
    let now = ctx.system().now_ms();
    let period = crate::log::utc_time(now)[..7].to_owned();
    if g.mobile_period != period {
        g.mobile_bytes = 0;
        g.outage_mobile_start = 0;
        g.mobile_period = period;
        ctx.settings().set("mobile_period", &json!(g.mobile_period))?;
    }
    let net = ctx.system().network();
    let mut profiles = net.profiles().unwrap_or_default();
    let mut readings = net.link_readings().unwrap_or_default();
    for p in &mut profiles {
        p.interface = interface_key(&p.interface);
    }
    for r in &mut readings {
        r.interface = interface_key(&r.interface);
    }
    let active = current(g, &profiles);
    let previous_outage = g.outage;
    if g.attempt.is_none() && g.backup_since.is_none() {
        if let Some(line) = active.clone().filter(|l| !l.hotspot) {
            if g.main.as_ref().is_none_or(|m| key(m) != key(&line)) {
                g.main = Some(line);
                g.main_disappeared = false;
                g.main_link_was_down = false;
                g.last_main_trial = now;
            }
        }
    }
    if g.main.is_none() {
        g.main = lines(g, &profiles).into_iter().find(|l| {
            !l.hotspot
                && profiles
                    .iter()
                    .any(|p| p.available && p.interface == l.interface && p.name == l.profile)
        });
    }
    // After moving locations, prefer a currently available non-mobile line.
    if g.main.as_ref().is_some_and(|m| {
        !profiles
            .iter()
            .any(|p| p.available && p.interface == m.interface && p.name == m.profile)
    }) {
        if let Some(line) = lines(g, &profiles).into_iter().find(|l| {
            !l.hotspot
                && profiles
                    .iter()
                    .any(|p| p.available && p.interface == l.interface && p.name == l.profile)
        }) {
            g.main = Some(line);
            g.main_disappeared = true;
        }
    }
    if let Some(main) = &g.main {
        if !profiles
            .iter()
            .any(|p| p.interface == main.interface && p.name == main.profile && p.available)
        {
            g.main_disappeared = true;
        }
    }
    let main_up = g
        .main
        .as_ref()
        .is_some_and(|m| readings.iter().any(|r| r.interface == m.interface && r.up));
    if !main_up {
        g.main_link_was_down = true;
    }
    for r in &readings {
        let total = r.received_bytes.saturating_add(r.sent_bytes);
        let before = g.counters.insert(r.interface.clone(), total);
        if active
            .as_ref()
            .is_some_and(|l| l.hotspot && l.interface == r.interface)
        {
            if let Some(before) = before {
                g.mobile_bytes = g.mobile_bytes.saturating_add(total.saturating_sub(before));
            }
        }
    }
    ctx.settings().set("mobile_bytes", &json!(g.mobile_bytes))?;
    save_outage(g, ctx, None)?;
    if g.settings
        .mobile_limit_mb
        .is_some_and(|limit| g.mobile_bytes / 1024 / 1024 >= limit)
    {
        notice(g, ctx, "mobile_data_limit", json!({"used_bytes":g.mobile_bytes}));
    }
    let (internet, apis) = std::thread::scope(|scope| {
        let internet = scope.spawn(|| net.probe(&g.settings.internet_url, &g.settings.proxy));
        let apis: Vec<_> = g
            .settings
            .api_urls
            .iter()
            .map(|url| scope.spawn(|| (url.clone(), net.probe(url, &g.settings.proxy))))
            .collect();
        (
            internet.join().unwrap(),
            apis.into_iter().map(|t| t.join().unwrap()).collect::<Vec<_>>(),
        )
    });
    let captive = internet.captive
        || ctx
            .system()
            .connectivity()
            .is_ok_and(|h| h.level == Connectivity::Constrained)
        || (internet.reachable
            && !g.settings.internet_expected.is_empty()
            && !internet
                .body
                .as_deref()
                .is_some_and(|b| b.trim() == g.settings.internet_expected));
    let internet_ok = internet.reachable && !captive;
    let api_ok = apis.iter().all(|(_, p)| p.reachable && !p.captive);
    let healthy = internet_ok && api_ok;
    g.reason = if captive {
        "captive_portal"
    } else if internet_ok && !api_ok {
        "api_unreachable"
    } else if !internet_ok {
        "internet_unreachable"
    } else {
        "online"
    }
    .into();
    g.last = json!({"internet":internet,"apis":apis,"captive":captive,"active":active,"mobile_bytes":g.mobile_bytes,"reason":g.reason});
    if healthy {
        g.failed = 0;
        if g.online != Some(true) {
            if let Some(since) = g.down_since {
                save_outage(g, ctx, Some(now))?;
                ctx.publish("up",json!({"outage_id":g.outage,"down_ms":now.saturating_sub(since),"mobile_bytes":g.mobile_bytes.saturating_sub(g.outage_mobile_start)}))?;
            }
            g.online = Some(true);
            g.down_since = None;
            g.outage = None;
            g.notice = None;
        }
    } else {
        g.failed = g.failed.saturating_add(1);
        if g.failed >= g.settings.failures && g.online != Some(false) {
            g.online = Some(false);
            g.down_since = Some(now);
            g.changes.clear();
            g.outage_mobile_start = g.mobile_bytes;
            let id = ctx.db().transaction(|c| {
                c.execute(
                    "INSERT INTO network_outages(started_at,reason) VALUES(?,?)",
                    &[now.into(), g.reason.as_str().into()],
                )?;
                c.query("SELECT last_insert_rowid()", &[])?[0].int(0)
            })?;
            g.outage = Some(id);
            ctx.publish("down", json!({"outage_id":id,"reason":g.reason,"at":now}))?;
            notice(g, ctx, &g.reason.clone(), g.last.clone());
        }
    }
    publish_reading(g, ctx)?;
    if !ctx.arbitration().hotspot {
        if g.attempt.is_some() || !ctx.journal().open_entries()?.is_empty() {
            g.attempt = None;
            g.backup_since = None;
            restore(ctx)?;
        }
        return Ok(g.last.clone());
    }
    if let Some(since) = g.backup_since {
        if healthy
            && main_up
            && (g.main_link_was_down
                || now.saturating_sub(since) >= i64::from(g.settings.retry_main_seconds) * 1000)
        {
            for e in ctx
                .journal()
                .open_entries()?
                .into_iter()
                .filter(|e| e.key.starts_with("metric:"))
            {
                restore_entry(&e, ctx)?;
                ctx.journal().mark_restored(e.id)?;
            }
            g.backup_since = None;
            g.main_link_was_down = false;
        } else if !healthy && now.saturating_sub(since) < i64::from(g.settings.connect_timeout_seconds) * 1000
        {
            return Ok(g.last.clone());
        }
    }
    if let Some(attempt) = g.attempt.take() {
        let connected = profiles
            .iter()
            .any(|p| p.interface == attempt.line.interface && p.name == attempt.line.profile && p.connected);
        if connected && healthy {
            if attempt.line.hotspot {
                g.last_main_trial = now;
            }
            mark_mobile(g, ctx, &attempt.line)?;
            let data = json!({"interface":attempt.line.interface,"profile":attempt.line.profile,"hotspot":attempt.line.hotspot,"outage_id":g.outage});
            g.changes.push(data.clone());
            ctx.publish("switched", data)?;
            if let Some(id) = g.outage.or(previous_outage) {
                ctx.db().execute(
                    "UPDATE network_outages SET changes=? WHERE id=?",
                    &[json!(g.changes).to_string().into(), id.into()],
                )?;
            }
        } else if now.saturating_sub(attempt.since) >= i64::from(g.settings.connect_timeout_seconds) * 1000 {
            if attempt.main_trial {
                if let Some(previous) = attempt.previous {
                    start_switch(g, ctx, previous, active.clone(), false)?;
                }
            }
        } else {
            g.attempt = Some(attempt);
            return Ok(g.last.clone());
        }
    }
    if captive {
        notice(g, ctx, "captive_portal", g.last.clone());
        return Ok(g.last.clone());
    }
    if internet_ok && !api_ok {
        return Ok(g.last.clone());
    }
    if g.failed >= g.settings.failures && !healthy && g.attempt.is_none() {
        match prefer_backup(g, ctx, &readings) {
            Ok(true) => return Ok(g.last.clone()),
            Err(e) => notice(g, ctx, "backup_needs_admin", json!(e)),
            _ => {}
        }
        let candidates = lines(g, &profiles);
        for line in candidates {
            if active.as_ref().is_some_and(|a| key(a) == key(&line))
                || g.last_attempt
                    .get(&key(&line))
                    .is_some_and(|at| now.saturating_sub(*at) < 30_000)
            {
                continue;
            }
            if !profiles
                .iter()
                .any(|p| p.interface == line.interface && p.name == line.profile && p.available)
            {
                continue;
            }
            match start_switch(g, ctx, line, active.clone(), false) {
                Ok(()) => break,
                Err(e) => notice(g, ctx, "saved_line_failed", json!(e)),
            }
        }
    } else if healthy && active.as_ref().is_some_and(|l| l.hotspot) {
        if let Some(main) = g.main.clone() {
            let visible = profiles
                .iter()
                .any(|p| p.interface == main.interface && p.name == main.profile && p.available);
            if !visible {
                g.main_disappeared = true;
            }
            if visible
                && (g.main_disappeared
                    || now.saturating_sub(g.last_main_trial)
                        >= i64::from(g.settings.retry_main_seconds) * 1000)
            {
                g.main_disappeared = false;
                g.last_main_trial = now;
                start_switch(g, ctx, main, active, true)?;
            }
        }
    }
    Ok(g.last.clone())
}

impl NetworkModule {
    fn stop_worker(&mut self) {
        self.watch = None;
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Stop);
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
impl Drop for NetworkModule {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Stop);
        }
    }
}
impl Module for NetworkModule {
    fn spec(&self) -> &'static ModuleSpec {
        &SPEC
    }
    fn start(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        {
            let mut g = lock(&self.shared);
            g.settings = settings::load(ctx)?;
            normalize_settings(&mut g.settings);
            g.mobile_period = ctx
                .settings()
                .get("mobile_period")?
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            g.mobile_bytes = ctx
                .settings()
                .get("mobile_bytes")?
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let rows=ctx.db().query("SELECT id,started_at,reason,changes,mobile_bytes FROM network_outages WHERE ended_at IS NULL ORDER BY id DESC LIMIT 1",&[])?;
            if let Some(r) = rows.first() {
                let parse = |e: crate::sqlite::Error| ApiError::internal(e.to_string());
                g.outage = Some(r.int(0).map_err(parse)?);
                g.down_since = Some(r.int(1).map_err(parse)?);
                g.reason = r.text(2).map_err(parse)?;
                g.changes = serde_json::from_str(&r.text(3).map_err(parse)?)
                    .map_err(|e| ApiError::internal(e.to_string()))?;
                g.outage_mobile_start = g
                    .mobile_bytes
                    .saturating_sub(r.int(4).map_err(parse)?.max(0) as u64);
                g.online = Some(false);
            }
        }
        let (tx, rx) = mpsc::channel();
        let notify = tx.clone();
        match ctx.system().network().watch(Arc::new(move |_| {
            let _ = notify.send(Msg::Wake);
        })) {
            Ok(w) => self.watch = Some(w),
            Err(e) => notice(
                &mut lock(&self.shared),
                ctx,
                "network_watch_unavailable",
                json!(e),
            ),
        }
        let shared = self.shared.clone();
        let ctx = ctx.clone();
        self.worker = Some(
            std::thread::Builder::new()
                .name("network".into())
                .spawn(move || {
                    let mut elapsed = Duration::ZERO;
                    loop {
                        let wait = Duration::from_millis(u64::from(lock(&shared).settings.poll_millis))
                            .saturating_sub(elapsed)
                            .max(Duration::from_millis(500));
                        match rx.recv_timeout(wait) {
                            Ok(Msg::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            _ => {
                                let started = std::time::Instant::now();
                                let mut g = lock(&shared);
                                if let Err(e) = cycle(&mut g, &ctx) {
                                    notice(&mut g, &ctx, "network_check_failed", json!(e));
                                }
                                elapsed = started.elapsed();
                            }
                        }
                    }
                })
                .map_err(|e| ApiError::internal(e.to_string()))?,
        );
        self.tx = Some(tx);
        Ok(())
    }
    fn stop(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        self.stop_worker();
        restore(ctx)
    }
    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        match req.cmd {
            "show" => Ok(self.status(ctx)),
            "profiles" => Ok(json!({"profiles":ctx.system().network().profiles()?})),
            "settings" => Ok(json!(lock(&self.shared).settings)),
            "configure" => {
                let mut settings = settings::patch(ctx, &req.args["settings"], &[], NetworkSettings::check)?;
                normalize_settings(&mut settings);
                lock(&self.shared).settings = settings.clone();
                if let Some(tx) = &self.tx {
                    let _ = tx.send(Msg::Wake);
                }
                Ok(json!({"settings":settings}))
            }
            "check" => cycle(&mut lock(&self.shared), ctx),
            "history" => {
                let limit = req.args["limit"].as_u64().unwrap_or(20);
                if !(1..=100).contains(&limit) {
                    return Err(ApiError::invalid_args("limit must be 1..=100"));
                }
                let entries=ctx.db().query("SELECT id,started_at,ended_at,reason,changes,mobile_bytes FROM network_outages ORDER BY id DESC LIMIT ?",&[(limit as i64).into()])?.iter().map(|r|Ok(json!({"outage_id":r.int(0)?,"started_at":r.int(1)?,"ended_at":r.opt_int(2)?,"reason":r.text(3)?,"changes":serde_json::from_str::<Value>(&r.text(4)?).unwrap_or(Value::Null),"mobile_bytes":r.int(5)?}))).collect::<Result<Vec<_>,crate::sqlite::Error>>().map_err(|e|ApiError::internal(e.to_string()))?;
                Ok(json!({"entries":entries}))
            }
            _ => Err(ApiError::new(
                ErrorCode::UnknownCommand,
                "unrouted network command",
            )),
        }
    }
    fn status(&self, _: &ModuleCtx) -> Value {
        let g = lock(&self.shared);
        json!({"implemented":true,"online":g.online,"failed_probes":g.failed,"outage_id":g.outage,"reading":g.last,"notice":g.notice,"settings":g.settings,
        "switching":g.attempt.as_ref().map(|a|json!({"interface":a.line.interface,"profile":a.line.profile,"since":a.since})),"mobile_bytes":g.mobile_bytes,"mobile_period":g.mobile_period,"main":g.main,"api_probes_configured":!g.settings.api_urls.is_empty()})
    }
    fn on_event(&mut self, _: &Event, _: &ModuleCtx) -> Result<(), ApiError> {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Wake);
        }
        Ok(())
    }
    fn recover(&mut self, e: &JournalEntry, ctx: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        restore_entry(e, ctx)?;
        Ok(RecoverOutcome::Restored)
    }
}
