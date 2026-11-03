//! 文件归属 (files): which conversation put which files on disk, from the agents' own
//! transcripts (read through `crates/agents`, never written). Each path belongs to the
//! conversation that created or downloaded it; shared caches (models, packages, container
//! and WSL disks) are listed apart, with only which conversations used them.
//!
//! Once a day (and on `files.scan`) only what was added to each transcript since the last
//! scan is read; then every attributed path and cache is measured. The tables keep paths,
//! sizes and redacted one-line command summaries, never conversation text. How ownership
//! and counting work: see `scan`.
//!
//! What it cannot see: files a program made on its own (a training run writing
//! checkpoints), downloads into default caches, and commands run on other machines over
//! `ssh`. Writing-time recording (ETW) is task 15.

mod caches;
mod downloads;
mod measure;
mod paths;
mod redact;
mod scan;
mod shell;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use proto::{ApiError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::dlog;
use crate::module::{
    ArgKind, ArgSpec, CommandSpec, ErrorSpec, EventSpec, Migration, Module, ModuleCtx,
    ModuleRequest, ModuleSpec, RecoverOutcome, Section,
};
use crate::modules::settings;
use crate::sqlite::{Row, Value as Sql};
use crate::store::{JournalEntry, now_ms};

static CONVERSATION: ArgSpec = ArgSpec::new(
    "id",
    ArgKind::Str,
    "Conversation id, or its first characters (at least 6)",
)
.required()
.positional();
static AGENT: ArgSpec = ArgSpec::new(
    "agent",
    ArgKind::Str,
    "Only this agent (claude-code, codex)",
);

static SPEC: ModuleSpec = ModuleSpec::new(
    "files",
    Section::AccountsStorage,
    "Which conversation put which files on disk",
)
.commands(&[
    CommandSpec::new(
        "scan",
        "Read new transcript data and measure sizes now (in the background)",
    )
    .agent()
    .args(&[ArgSpec::new(
        "no_measure",
        ArgKind::Bool,
        "Only read transcripts, keep the last sizes",
    )]),
    CommandSpec::new("scans", "The current scan's progress and the last scans")
        .agent()
        .read_only(),
    CommandSpec::new("top", "Conversations by the size of the data they own")
        .agent()
        .read_only()
        .args(&[
            ArgSpec::new("limit", ArgKind::Uint, "How many (default 10)"),
            AGENT,
            ArgSpec::new(
                "by_folder",
                ArgKind::Bool,
                "Add up by the conversations' working folder instead",
            ),
        ]),
    CommandSpec::new(
        "conversation",
        "What one conversation wrote and downloaded, and the caches it used",
    )
    .agent()
    .read_only()
    .args(&[
        CONVERSATION,
        AGENT,
        ArgSpec::new("limit", ArgKind::Uint, "Paths to list (default 50)"),
    ]),
    CommandSpec::new(
        "downloads",
        "What one conversation downloaded, cloned or unpacked (with the command)",
    )
    .agent()
    .read_only()
    .args(&[CONVERSATION, AGENT]),
    CommandSpec::new(
        "find",
        "Which conversation made the paths matching this text",
    )
    .agent()
    .read_only()
    .args(&[
        ArgSpec::new("query", ArgKind::Str, "Part of a path, e.g. a file name")
            .required()
            .positional(),
        ArgSpec::new("limit", ArgKind::Uint, "How many (default 20)"),
    ]),
    CommandSpec::new(
        "caches",
        "Shared caches: size, and which conversations used them",
    )
    .agent()
    .read_only(),
    CommandSpec::new("settings", "Scan settings")
        .agent()
        .read_only(),
    CommandSpec::new(
        "configure",
        "Change scan settings (daily scan, interval, ...)",
    )
    .args(&[ArgSpec::new(
        "settings",
        ArgKind::Json,
        "Partial settings object, e.g. {\"interval_hours\":12}",
    )
    .required()]),
])
.publishes(&[EventSpec::new(
    "scanned",
    "A scan finished (counts, outcome)",
)])
.migrations(&[Migration::new(
    1,
    "CREATE TABLE files_transcripts (
            path            TEXT PRIMARY KEY,
            agent           TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            subagent        TEXT,
            size            INTEGER NOT NULL,
            modified_ms     INTEGER,
            read_to         INTEGER NOT NULL,
            cwd             TEXT,
            version         TEXT,
            stop            TEXT,
            scanned_at      INTEGER NOT NULL
        );
        CREATE INDEX files_transcripts_conv ON files_transcripts (agent, conversation_id);
        CREATE TABLE files_conversations (
            agent           TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            cwd             TEXT,
            version         TEXT,
            first_ts        INTEGER,
            last_ts         INTEGER,
            writes          INTEGER NOT NULL DEFAULT 0,
            commands        INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (agent, conversation_id)
        );
        CREATE TABLE files_paths (
            id              INTEGER PRIMARY KEY,
            path_key        TEXT NOT NULL UNIQUE,
            path            TEXT NOT NULL,
            agent           TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            how             TEXT NOT NULL,
            ts              INTEGER,
            tool            TEXT,
            command         TEXT,
            source          TEXT,
            deleted_ts      INTEGER,
            kind            TEXT,
            bytes           INTEGER,
            files           INTEGER,
            counted         INTEGER,
            pre_existing    INTEGER NOT NULL DEFAULT 0,
            truncated       INTEGER NOT NULL DEFAULT 0,
            measured_at     INTEGER,
            note            TEXT,
            unattributed    INTEGER
        );
        CREATE INDEX files_paths_owner ON files_paths (agent, conversation_id);
        CREATE TABLE files_touches (
            path_id         INTEGER NOT NULL,
            agent           TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            how             TEXT NOT NULL,
            first_ts        INTEGER,
            last_ts         INTEGER,
            count           INTEGER NOT NULL,
            command         TEXT,
            source          TEXT,
            PRIMARY KEY (path_id, agent, conversation_id, how)
        );
        CREATE INDEX files_touches_conv ON files_touches (agent, conversation_id);
        CREATE TABLE files_cache_uses (
            cache           TEXT NOT NULL,
            agent           TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            first_ts        INTEGER,
            last_ts         INTEGER,
            uses            INTEGER NOT NULL,
            command         TEXT,
            PRIMARY KEY (cache, agent, conversation_id)
        );
        CREATE TABLE files_caches (
            cache           TEXT NOT NULL,
            path            TEXT NOT NULL,
            kind            TEXT,
            bytes           INTEGER,
            files           INTEGER,
            truncated       INTEGER NOT NULL DEFAULT 0,
            measured_at     INTEGER NOT NULL,
            PRIMARY KEY (cache, path)
        );
        CREATE TABLE files_scans (
            id              INTEGER PRIMARY KEY,
            reason          TEXT NOT NULL,
            started_at      INTEGER NOT NULL,
            ended_at        INTEGER,
            outcome         TEXT,
            totals          TEXT
        );",
)])
.error_codes(&[
    ErrorSpec::new(
        "NO_SUCH_CONVERSATION",
        ErrorCode::NotFound,
        "No scanned conversation has this id",
    ),
    ErrorSpec::new(
        "AMBIGUOUS_CONVERSATION",
        ErrorCode::InvalidArgs,
        "Several conversations start with this; give more characters",
    ),
]);

// ---- settings ----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilesSettings {
    /// Scan by itself every `interval_hours`.
    pub auto_scan: bool,
    pub interval_hours: u32,
    /// The first automatic scan after the daemon starts waits this long.
    pub first_scan_delay_minutes: u32,
    /// Measure sizes after reading (automatic scans).
    pub measure: bool,
    /// A folder walk stops after this many entries (its size is then a lower bound).
    pub max_entries_per_path: u64,
    /// Look for transcripts and caches under this home folder instead of the daemon user's
    /// (with none of the user's environment variables). For tests and for scanning a copy.
    pub home: Option<String>,
}

impl Default for FilesSettings {
    fn default() -> Self {
        FilesSettings {
            auto_scan: true,
            interval_hours: 24,
            first_scan_delay_minutes: 10,
            measure: true,
            max_entries_per_path: 2_000_000,
            home: None,
        }
    }
}

impl FilesSettings {
    fn check(&self) -> Result<(), String> {
        if !(1..=168).contains(&self.interval_hours) {
            return Err("interval_hours must be 1..=168".into());
        }
        if self.first_scan_delay_minutes > 1440 {
            return Err("first_scan_delay_minutes must be 0..=1440".into());
        }
        if !(1000..=100_000_000).contains(&self.max_entries_per_path) {
            return Err("max_entries_per_path must be 1000..=100000000".into());
        }
        if self.home.as_deref().is_some_and(|h| !paths::is_absolute(h)) {
            return Err("home must be an absolute path".into());
        }
        Ok(())
    }

    fn options(&self, measure: bool, reason: &str) -> scan::Options {
        let (env, mut paths) = match &self.home {
            Some(h) => {
                let env = agents::Env::with_home(h);
                let paths = paths::PathEnv::from_agents_env(&env);
                (env, paths)
            }
            None => {
                let env = agents::Env::from_process();
                let mut paths = paths::PathEnv::from_agents_env(&env);
                paths.vars.extend(caches::process_vars());
                (env, paths)
            }
        };
        if let Some(t) = std::env::var("TEMP").ok().filter(|_| self.home.is_none()) {
            paths.vars.insert("TEMP".into(), t.clone());
            paths.vars.insert("TMP".into(), t);
        }
        scan::Options {
            env,
            paths,
            measure,
            max_entries: self.max_entries_per_path,
            reason: reason.to_owned(),
        }
    }
}

// ---- module ------------------------------------------------------------------------------

enum Msg {
    Stop,
    Scan {
        reason: String,
        measure: Option<bool>,
    },
    /// Settings changed: work out the next automatic scan again.
    Wake,
}

#[derive(Default)]
struct Shared {
    settings: FilesSettings,
    last: Mutex<Option<Value>>,
}

pub struct FilesModule {
    shared: Arc<Mutex<Shared>>,
    progress: Arc<Mutex<scan::Progress>>,
    stop: Arc<AtomicBool>,
    tx: Option<Sender<Msg>>,
    worker: Option<JoinHandle<()>>,
    /// When the first automatic scan is due (for status).
    next_auto: Arc<Mutex<Option<i64>>>,
}

pub fn create() -> Box<dyn Module> {
    Box::new(FilesModule {
        shared: Arc::default(),
        progress: Arc::default(),
        stop: Arc::new(AtomicBool::new(false)),
        tx: None,
        worker: None,
        next_auto: Arc::default(),
    })
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn last_done(ctx: &ModuleCtx) -> Option<i64> {
    let rows = ctx
        .db()
        .query(
            "SELECT max(ended_at) FROM files_scans WHERE outcome = 'done'",
            &[],
        )
        .ok()?;
    rows.first()?.opt_int(0).ok().flatten()
}

fn worker(
    ctx: ModuleCtx,
    rx: mpsc::Receiver<Msg>,
    shared: Arc<Mutex<Shared>>,
    progress: Arc<Mutex<scan::Progress>>,
    stop: Arc<AtomicBool>,
    next_auto: Arc<Mutex<Option<i64>>>,
) {
    let started = now_ms();
    loop {
        if !ctx.is_active() {
            break;
        }
        let s = lock(&shared).settings.clone();
        let due = s.auto_scan.then(|| {
            let earliest = started + i64::from(s.first_scan_delay_minutes) * 60_000;
            last_done(&ctx)
                .map(|t| t + i64::from(s.interval_hours) * 3_600_000)
                .unwrap_or(earliest)
                .max(earliest)
        });
        *lock(&next_auto) = due;
        let msg = match due {
            Some(d) => rx.recv_timeout(Duration::from_millis((d - now_ms()).max(0) as u64)),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let (reason, measure) = match msg {
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Msg::Wake) => continue,
            Ok(Msg::Scan { reason, measure }) => (reason, measure.unwrap_or(true)),
            Err(RecvTimeoutError::Timeout) => ("daily".to_owned(), s.measure),
        };
        {
            let mut p = lock(&progress);
            *p = scan::Progress {
                running: true,
                reason: Some(reason.clone()),
                started_at: Some(now_ms()),
                ..Default::default()
            };
        }
        if !ctx.is_active() {
            break;
        }
        let opts = s.options(measure, &reason);
        let result = scan::run(&ctx, &opts, &stop, &progress);
        *lock(&progress) = scan::Progress::default();
        match result {
            Ok(summary) => {
                if let Err(e) = ctx.publish("scanned", summary.clone()) {
                    dlog!("WARN", "files: {}", e.message);
                }
                *lock(&lock(&shared).last) = Some(summary);
            }
            Err(e) => dlog!("WARN", "files: scan failed: {}", e.message),
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
}

impl FilesModule {
    fn stop_worker(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Stop);
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }

    fn scan(&mut self, req: &ModuleRequest<'_>) -> Result<Value, ApiError> {
        let mut p = lock(&self.progress);
        if p.running {
            return Ok(json!({ "started": false, "running": true, "progress": *p }));
        }
        let Some(tx) = &self.tx else {
            return Err(ApiError::internal("files: worker not running"));
        };
        p.running = true;
        p.reason = Some("manual".into());
        let measure = Some(req.args["no_measure"].as_bool() != Some(true));
        tx.send(Msg::Scan {
            reason: "manual".into(),
            measure,
        })
        .map_err(|_| ApiError::internal("files: worker stopped"))?;
        Ok(json!({ "started": true, "note": "runs in the background; see `files scans`" }))
    }

    fn scans(&self, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        let rows = ctx.db().query("SELECT id, reason, started_at, ended_at, outcome, totals FROM files_scans ORDER BY id DESC LIMIT 10", &[])?;
        let scans: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "scan_id": r.int(0).ok(), "reason": text(r, 1), "started_at": int(r, 2), "ended_at": int(r, 3),
                    "outcome": text(r, 4), "totals": text(r, 5).and_then(|t| serde_json::from_str::<Value>(&t).ok()),
                })
            })
            .collect();
        Ok(
            json!({ "progress": *lock(&self.progress), "next_auto_scan": *lock(&self.next_auto), "scans": scans }),
        )
    }
}

fn int(r: &Row, i: usize) -> Option<i64> {
    r.opt_int(i).ok().flatten()
}

fn text(r: &Row, i: usize) -> Option<String> {
    r.opt_text(i).ok().flatten()
}

fn limit(args: &Value, default: u64) -> i64 {
    args["limit"].as_u64().unwrap_or(default).clamp(1, 1000) as i64
}

/// `(agent, conversation_id)` for an id or a prefix of one.
fn find_conversation(ctx: &ModuleCtx, args: &Value) -> Result<(String, String), ApiError> {
    let id = args["id"].as_str().unwrap_or_default().trim();
    let agent = args["agent"].as_str();
    let exact = ctx.db().query(
        "SELECT agent, conversation_id FROM files_conversations WHERE conversation_id = ?1 AND (?2 IS NULL OR agent = ?2)",
        &[id.into(), agent.into()],
    )?;
    let rows = if !exact.is_empty() || id.chars().count() < 6 {
        exact
    } else {
        let pattern = format!(
            "{}%",
            id.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        ctx.db().query(
            "SELECT agent, conversation_id FROM files_conversations WHERE conversation_id LIKE ?1 ESCAPE '\\' AND (?2 IS NULL OR agent = ?2) LIMIT 6",
            &[pattern.into(), agent.into()],
        )?
    };
    match rows.as_slice() {
        [] => Err(ctx.error(
            "NO_SUCH_CONVERSATION",
            format!(
                "no scanned conversation '{id}' (ids need at least 6 characters; has a scan run?)"
            ),
        )),
        [r] => Ok((r.text(0).unwrap_or_default(), r.text(1).unwrap_or_default())),
        many => {
            let ids: Vec<String> = many.iter().filter_map(|r| r.text(1).ok()).collect();
            Err(ctx.error(
                "AMBIGUOUS_CONVERSATION",
                format!("'{id}' matches {}", ids.join(", ")),
            ))
        }
    }
}

fn path_json(r: &Row) -> Value {
    json!({
        "path": text(r, 0), "how": text(r, 1), "ts": int(r, 2), "kind": text(r, 3), "bytes": int(r, 4),
        "counted": int(r, 5), "files": int(r, 6), "pre_existing": int(r, 7) == Some(1), "deleted_ts": int(r, 8),
        "command": text(r, 9), "source": text(r, 10), "tool": text(r, 11), "truncated": int(r, 12) == Some(1),
        "note": text(r, 13), "unattributed": int(r, 14),
    })
}

/// Columns `path_json` reads; queries add their own after these (from index 15).
const PATH_COLS: &str = "p.path, p.how, p.ts, p.kind, p.bytes, p.counted, p.files, p.pre_existing, p.deleted_ts, p.command, p.source, p.tool, p.truncated, p.note, p.unattributed";

fn caches_used(ctx: &ModuleCtx, agent: &str, conv: &str) -> Result<Vec<Value>, ApiError> {
    let rows = ctx.db().query(
        "SELECT cache, uses, first_ts, last_ts, command FROM files_cache_uses WHERE agent = ?1 AND conversation_id = ?2 ORDER BY uses DESC",
        &[agent.into(), conv.into()],
    )?;
    Ok(rows.iter().map(|r| json!({"cache": text(r, 0), "uses": int(r, 1), "first_ts": int(r, 2), "last_ts": int(r, 3), "last_command": text(r, 4)})).collect())
}

fn conversation(ctx: &ModuleCtx, args: &Value) -> Result<Value, ApiError> {
    let (agent, conv) = find_conversation(ctx, args)?;
    let p: [Sql; 2] = [agent.as_str().into(), conv.as_str().into()];
    let info = ctx.db().query(
        "SELECT cwd, version, first_ts, last_ts, writes, commands FROM files_conversations WHERE agent = ?1 AND conversation_id = ?2",
        &p,
    )?;
    let t = ctx.db().query(
        "SELECT count(*), sum(size), sum(CASE WHEN subagent IS NOT NULL THEN 1 ELSE 0 END), sum(CASE WHEN stop IS NOT NULL THEN 1 ELSE 0 END)
         FROM files_transcripts WHERE agent = ?1 AND conversation_id = ?2",
        &p,
    )?;
    let totals = ctx.db().query(
        "SELECT count(*), sum(coalesce(counted, 0)), sum(CASE WHEN how = 'modified' THEN coalesce(bytes, 0) ELSE 0 END),
                sum(CASE WHEN how IN ('downloaded', 'cloned', 'extracted') THEN coalesce(counted, 0) ELSE 0 END)
         FROM files_paths WHERE agent = ?1 AND conversation_id = ?2",
        &p,
    )?;
    let owned = ctx.db().query(
        &format!(
            "SELECT {PATH_COLS} FROM files_paths p WHERE p.agent = ?1 AND p.conversation_id = ?2
             ORDER BY coalesce(p.counted, 0) DESC, coalesce(p.bytes, 0) DESC, p.path LIMIT ?3"
        ),
        &[p[0].clone(), p[1].clone(), limit(args, 50).into()],
    )?;
    let others = ctx.db().query(
        "SELECT p.path, p.agent, p.conversation_id, p.how, t.how, t.count FROM files_touches t JOIN files_paths p ON p.id = t.path_id
         WHERE t.agent = ?1 AND t.conversation_id = ?2 AND NOT (p.agent = ?1 AND p.conversation_id = ?2)
         ORDER BY t.last_ts DESC LIMIT 20",
        &p,
    )?;
    let i = info.first();
    let t = t.first();
    let s = totals.first();
    Ok(json!({
        "agent": agent, "conversation_id": conv,
        "cwd": i.and_then(|r| text(r, 0)), "version": i.and_then(|r| text(r, 1)),
        "first_ts": i.and_then(|r| int(r, 2)), "last_ts": i.and_then(|r| int(r, 3)),
        "file_writes": i.and_then(|r| int(r, 4)), "commands": i.and_then(|r| int(r, 5)),
        "transcripts": {"files": t.and_then(|r| int(r, 0)), "bytes": t.and_then(|r| int(r, 1)), "subagents": t.and_then(|r| int(r, 2)), "stopped": t.and_then(|r| int(r, 3))},
        "owned": {"paths": s.and_then(|r| int(r, 0)), "bytes": s.and_then(|r| int(r, 1)).unwrap_or(0), "modified_bytes": s.and_then(|r| int(r, 2)).unwrap_or(0), "downloaded_bytes": s.and_then(|r| int(r, 3)).unwrap_or(0)},
        "paths": owned.iter().map(path_json).collect::<Vec<_>>(),
        "also_wrote": others.iter().map(|r| json!({"path": text(r, 0), "owner": {"agent": text(r, 1), "conversation_id": text(r, 2), "how": text(r, 3)}, "how": text(r, 4), "count": int(r, 5)})).collect::<Vec<_>>(),
        "caches": caches_used(ctx, &agent, &conv)?,
    }))
}

fn downloads(ctx: &ModuleCtx, args: &Value) -> Result<Value, ApiError> {
    let (agent, conv) = find_conversation(ctx, args)?;
    // Every download this conversation made, also those a later conversation made again.
    let rows = ctx.db().query(
        &format!(
            "SELECT {PATH_COLS}, p.agent, p.conversation_id, t.how, t.first_ts, t.count, t.command, t.source FROM files_touches t JOIN files_paths p ON p.id = t.path_id
             WHERE t.agent = ?1 AND t.conversation_id = ?2 AND t.how IN ('downloaded', 'cloned', 'extracted')
             ORDER BY t.first_ts, p.path"
        ),
        &[agent.as_str().into(), conv.as_str().into()],
    )?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut v = path_json(r);
            let owner_is_me = text(r, 15).as_deref() == Some(agent.as_str())
                && text(r, 16).as_deref() == Some(conv.as_str());
            v["how"] = json!(text(r, 17));
            v["ts"] = json!(int(r, 18));
            v["times"] = json!(int(r, 19));
            // This conversation's own command, also when another one owns the path now.
            v["command"] = json!(text(r, 20));
            v["source"] = json!(text(r, 21));
            if !owner_is_me {
                v["now_owned_by"] = json!({"agent": text(r, 15), "conversation_id": text(r, 16)});
            }
            v
        })
        .collect();
    Ok(
        json!({ "agent": agent, "conversation_id": conv, "downloads": items, "caches": caches_used(ctx, &agent, &conv)? }),
    )
}

fn top(ctx: &ModuleCtx, args: &Value) -> Result<Value, ApiError> {
    let agent = args["agent"].as_str();
    let by_folder = args["by_folder"].as_bool() == Some(true);
    let group = if by_folder {
        "c.cwd"
    } else {
        "p.agent, p.conversation_id"
    };
    let rows = ctx.db().query(
        &format!(
            "SELECT p.agent, p.conversation_id, c.cwd, min(c.first_ts), max(c.last_ts), count(*), sum(coalesce(p.counted, 0)) AS counted,
                    sum(CASE WHEN p.how IN ('downloaded', 'cloned', 'extracted') THEN coalesce(p.counted, 0) ELSE 0 END),
                    sum(CASE WHEN p.how = 'modified' THEN coalesce(p.bytes, 0) ELSE 0 END), count(DISTINCT p.conversation_id)
             FROM files_paths p LEFT JOIN files_conversations c ON c.agent = p.agent AND c.conversation_id = p.conversation_id
             WHERE ?1 IS NULL OR p.agent = ?1
             GROUP BY {group} ORDER BY counted DESC LIMIT ?2"
        ),
        &[agent.into(), limit(args, 10).into()],
    )?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut v = json!({
                "cwd": text(r, 2), "first_ts": int(r, 3), "last_ts": int(r, 4), "paths": int(r, 5),
                "bytes": int(r, 6).unwrap_or(0), "downloaded_bytes": int(r, 7).unwrap_or(0), "modified_bytes": int(r, 8).unwrap_or(0),
            });
            if by_folder {
                v["conversations"] = json!(int(r, 9));
            } else {
                v["agent"] = json!(text(r, 0));
                v["conversation_id"] = json!(text(r, 1));
            }
            v
        })
        .collect();
    let all = ctx.db().query(
        "SELECT count(DISTINCT agent || ' ' || conversation_id), count(*), sum(coalesce(counted, 0)), max(measured_at),
                sum(coalesce(unattributed, 0)) FROM files_paths
         WHERE ?1 IS NULL OR agent = ?1",
        &[agent.into()],
    )?;
    let shared = ctx.db().query(
        &format!("SELECT {PATH_COLS} FROM files_paths p WHERE p.note = 'shared' AND (?1 IS NULL OR p.agent = ?1) ORDER BY p.unattributed DESC LIMIT 10"),
        &[agent.into()],
    )?;
    let a = all.first();
    Ok(json!({
        "items": items,
        "total": {
            "conversations": a.and_then(|r| int(r, 0)), "paths": a.and_then(|r| int(r, 1)), "bytes": a.and_then(|r| int(r, 2)).unwrap_or(0),
            "unattributed_bytes": a.and_then(|r| int(r, 4)).unwrap_or(0),
        },
        // Folders several conversations put things in: what is left of them after taking out
        // each conversation's own paths.
        "shared_folders": shared.iter().map(path_json).collect::<Vec<_>>(),
        "measured_at": a.and_then(|r| int(r, 3)),
    }))
}

fn find(ctx: &ModuleCtx, args: &Value) -> Result<Value, ApiError> {
    let q = args["query"].as_str().unwrap_or_default().to_lowercase();
    if q.trim().len() < 3 {
        return Err(ApiError::invalid_args("query needs at least 3 characters"));
    }
    let pattern = format!(
        "%{}%",
        q.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let rows = ctx.db().query(
        &format!(
            "SELECT {PATH_COLS}, p.agent, p.conversation_id, (SELECT count(DISTINCT t.agent || ' ' || t.conversation_id) FROM files_touches t WHERE t.path_id = p.id)
             FROM files_paths p WHERE p.path_key LIKE ?1 ESCAPE '\\' ORDER BY coalesce(p.bytes, 0) DESC LIMIT ?2"
        ),
        &[pattern.into(), limit(args, 20).into()],
    )?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut v = path_json(r);
            v["owner"] = json!({"agent": text(r, 15), "conversation_id": text(r, 16)});
            v["conversations"] = json!(int(r, 17));
            v
        })
        .collect();
    Ok(json!({ "items": items }))
}

fn caches_report(ctx: &ModuleCtx) -> Result<Value, ApiError> {
    let sizes = ctx.db().query("SELECT cache, path, kind, bytes, files, truncated, measured_at FROM files_caches ORDER BY cache, bytes DESC", &[])?;
    let uses = ctx.db().query(
        "SELECT cache, count(*), sum(uses), max(last_ts) FROM files_cache_uses GROUP BY cache",
        &[],
    )?;
    let recent = ctx.db().query(
        "SELECT cache, agent, conversation_id, uses, last_ts FROM files_cache_uses ORDER BY cache, last_ts DESC",
        &[],
    )?;
    let out: Vec<Value> = caches::NAMES
        .iter()
        .map(|name| {
            let locs: Vec<Value> = sizes
                .iter()
                .filter(|r| text(r, 0).as_deref() == Some(*name))
                .map(|r| json!({"path": text(r, 1), "kind": text(r, 2), "bytes": int(r, 3), "files": int(r, 4), "truncated": int(r, 5) == Some(1), "measured_at": int(r, 6)}))
                .collect();
            let u = uses.iter().find(|r| text(r, 0).as_deref() == Some(*name));
            let last: Vec<Value> = recent
                .iter()
                .filter(|r| text(r, 0).as_deref() == Some(*name))
                .take(5)
                .map(|r| json!({"agent": text(r, 1), "conversation_id": text(r, 2), "uses": int(r, 3), "last_ts": int(r, 4)}))
                .collect();
            json!({
                "cache": name,
                "bytes": locs.iter().filter_map(|l| l["bytes"].as_i64()).sum::<i64>(),
                "locations": locs,
                "conversations": u.and_then(|r| int(r, 1)).unwrap_or(0), "uses": u.and_then(|r| int(r, 2)).unwrap_or(0),
                "last_used": u.and_then(|r| int(r, 3)), "recent": last,
            })
        })
        .collect();
    Ok(json!({ "caches": out }))
}

impl Module for FilesModule {
    fn spec(&self) -> &'static ModuleSpec {
        &SPEC
    }

    fn start(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        let s: FilesSettings = settings::load(ctx)?;
        *lock(&self.next_auto) = s.auto_scan.then(|| {
            let earliest = now_ms() + i64::from(s.first_scan_delay_minutes) * 60_000;
            last_done(ctx)
                .map(|t| t + i64::from(s.interval_hours) * 3_600_000)
                .unwrap_or(earliest)
                .max(earliest)
        });
        lock(&self.shared).settings = s;
        self.stop.store(false, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel::<Msg>();
        let (wctx, shared, progress, stop, next) = (
            ctx.clone(),
            self.shared.clone(),
            self.progress.clone(),
            self.stop.clone(),
            self.next_auto.clone(),
        );
        let w = std::thread::Builder::new()
            .name("files-scan".into())
            .spawn(move || worker(wctx, rx, shared, progress, stop, next))
            .map_err(|e| ApiError::internal(format!("cannot start the files thread: {e}")))?;
        self.tx = Some(tx);
        self.worker = Some(w);
        Ok(())
    }

    fn stop(&mut self, _ctx: &ModuleCtx) -> Result<(), ApiError> {
        self.stop_worker();
        Ok(())
    }

    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        match req.cmd {
            "scan" => self.scan(req),
            "scans" => self.scans(ctx),
            "top" => top(ctx, req.args),
            "conversation" => conversation(ctx, req.args),
            "downloads" => downloads(ctx, req.args),
            "find" => find(ctx, req.args),
            "caches" => caches_report(ctx),
            "settings" => Ok(json!({ "settings": lock(&self.shared).settings })),
            "configure" => {
                let next: FilesSettings =
                    settings::patch(ctx, &req.args["settings"], &[], FilesSettings::check)?;
                lock(&self.shared).settings = next.clone();
                if let Some(tx) = &self.tx {
                    let _ = tx.send(Msg::Wake);
                }
                Ok(json!({ "settings": next }))
            }
            other => Err(ApiError::internal(format!(
                "files has no handler for '{other}'"
            ))),
        }
    }

    fn status(&self, _ctx: &ModuleCtx) -> Value {
        let p = lock(&self.progress).clone();
        let shared = lock(&self.shared);
        let last = lock(&shared.last).clone();
        let summary = if p.running {
            format!(
                "scanning ({} {}/{})",
                p.phase.unwrap_or("starting"),
                p.done,
                p.total
            )
        } else {
            match &last {
                Some(l) => format!(
                    "last scan {}: {}",
                    l["outcome"].as_str().unwrap_or("?"),
                    l["totals"]["transcripts"]
                ),
                None => "no scan since start".to_owned(),
            }
        };
        json!({ "implemented": true, "summary": summary, "progress": p, "next_auto_scan": *lock(&self.next_auto), "last_scan": last })
    }

    fn recover(
        &mut self,
        _entry: &JournalEntry,
        _ctx: &ModuleCtx,
    ) -> Result<RecoverOutcome, ApiError> {
        // Only reads the system, so it never writes journal entries.
        Ok(RecoverOutcome::Restored)
    }
}
