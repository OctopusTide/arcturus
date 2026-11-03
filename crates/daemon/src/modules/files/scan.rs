//! One scan: read what was added to every transcript since the last scan (through the agent
//! adapters, without message text), attribute the paths it wrote or downloaded to its
//! conversation, then measure every attributed path and every shared cache.
//!
//! Who owns a path: the conversation with the latest event that made it (created, written,
//! downloaded, cloned, unpacked, moved there). A path that conversations only edited is
//! owned by the first editor, as `modified`, and is not counted as that conversation's
//! data. Every conversation that wrote a path is kept as a touch. Reading order does not
//! matter: the same transcripts give the same owners.
//!
//! Folders are judged by when they were created: the owner is the conversation with the last
//! event that made the folder at or before its creation time (a later `mkdir -Force` of an
//! existing folder does not take it over); a folder created before every such event existed
//! already and is marked `pre_existing`. A folder that appeared within a few minutes of a
//! write inside it (the write made it, like `Write` or `New-Item -Force` do for missing
//! parents) is `created_folder` by that conversation.
//!
//! What a conversation's data is: the bytes of the paths it owns, minus what lies inside
//! them that another counted path owns, so nothing is counted twice. Pre-existing folders
//! are not counted. A folder where other conversations made at least as many paths as its
//! owner is `shared`: what is left of it after taking their paths out (build outputs, files
//! programs made) cannot be pinned on one conversation and is `unattributed` instead of
//! counted (not for clones, downloads and unpacked archives: their command explains what is
//! in them). Folders holding the
//! agents' own transcripts, and the folders above them, are `agent_data`, never attributed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use agents::model::{Encoding, FileOp, ReadOptions, ReadOutcome, Record, Shell, Stop, TranscriptFile};
use proto::ApiError;
use serde::Serialize;
use serde_json::{Value, json};

use super::downloads::{self, Found};
use super::measure::{self, Kind, Size};
use super::paths::{PathEnv, is_within, key, resolve};
use super::{caches, shell};
use crate::dlog;
use crate::module::ModuleCtx;
use crate::sqlite::{self, Connection, Value as Sql};
use crate::store::now_ms;

/// How far a folder's creation time and the event that made it may be apart (clocks, the
/// time a record is written vs. when the tool ran).
const SLACK_MS: i64 = 10 * 60_000;
/// Rows written per transaction while measuring.
const MEASURE_BATCH: usize = 500;
/// Parent folders looked at above one path.
const MAX_PARENTS: usize = 12;

const FOLDER: &str = "created_folder";
pub const CREATING: &[&str] = &["created", "written", "moved", "downloaded", "cloned", "extracted", FOLDER];

#[derive(Debug, Clone, Default, Serialize)]
pub struct Progress {
    pub running: bool,
    pub reason: Option<String>,
    /// `listing`, `reading`, `measuring`.
    pub phase: Option<&'static str>,
    pub done: u64,
    pub total: u64,
    pub started_at: Option<i64>,
}

pub struct Options {
    pub env: agents::Env,
    pub paths: PathEnv,
    pub measure: bool,
    pub max_entries: u64,
    pub reason: String,
}

#[derive(Debug, Default, Serialize)]
struct Totals {
    transcripts: u64,
    read_files: u64,
    read_bytes: u64,
    records: u64,
    stopped: u64,
    compressed: u64,
    unresolved: u64,
    measured: u64,
    /// `created_folder` rows found this time.
    folders: u64,
    /// Folders whose owner was set by their creation time.
    reowned: u64,
    errors: u64,
}

fn set_progress(p: &Mutex<Progress>, f: impl FnOnce(&mut Progress)) {
    f(&mut p.lock().unwrap_or_else(|e| e.into_inner()));
}

/// Runs a whole scan and records it in `files_scans`. `stop` aborts between files and
/// during folder walks; what was read so far stays.
pub fn run(ctx: &ModuleCtx, opts: &Options, stop: &AtomicBool, progress: &Mutex<Progress>) -> Result<Value, ApiError> {
    let started = now_ms();
    ctx.db().execute("INSERT INTO files_scans (reason, started_at) VALUES (?1, ?2)", &[opts.reason.as_str().into(), started.into()])?;
    let scan_id = ctx.db().query("SELECT max(id) FROM files_scans", &[])?.first().and_then(|r| r.opt_int(0).ok().flatten()).unwrap_or(0);
    let mut t = Totals::default();
    let outcome = read_all(ctx, opts, stop, progress, &mut t).and_then(|agent_dirs| {
        if opts.measure && !stop.load(Ordering::Relaxed) { measure_all(ctx, opts, &agent_dirs, stop, progress, &mut t) } else { Ok(()) }
    });
    let outcome = match outcome {
        Ok(()) if stop.load(Ordering::Relaxed) => "stopped".to_owned(),
        Ok(()) => "done".to_owned(),
        Err(e) => format!("error: {}", e.message),
    };
    let ended = now_ms();
    let summary = json!({ "scan_id": scan_id, "reason": opts.reason, "started_at": started, "ended_at": ended, "outcome": outcome, "totals": t });
    ctx.db().execute(
        "UPDATE files_scans SET ended_at = ?2, outcome = ?3, totals = ?4 WHERE id = ?1",
        &[scan_id.into(), ended.into(), outcome.as_str().into(), json!(t).to_string().into()],
    )?;
    Ok(summary)
}

// ---- reading -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Made {
    how: &'static str,
    ts: Option<i64>,
    tool: String,
    command: Option<String>,
    source: Option<String>,
}

#[derive(Debug, Default)]
struct PathAgg {
    path: String,
    /// The latest event that made the path.
    made: Option<Made>,
    /// The first edit (time, tool).
    modified: Option<(Option<i64>, String)>,
    deleted: Option<Option<i64>>,
    /// Per kind of touch.
    touches: BTreeMap<&'static str, Touch>,
}

#[derive(Debug, Default)]
struct Touch {
    first: Option<i64>,
    last: Option<i64>,
    count: u64,
    /// The latest event's command summary and source, for events that made the path.
    command: Option<String>,
    source: Option<String>,
}

#[derive(Debug, Default)]
struct CacheAgg {
    first: Option<i64>,
    last: Option<i64>,
    uses: u64,
    command: String,
}

/// What one pass over one transcript found.
#[derive(Default)]
struct Agg {
    paths: HashMap<String, PathAgg>,
    caches: BTreeMap<&'static str, CacheAgg>,
    cwd: Option<String>,
    first_cwd: Option<String>,
    version: Option<String>,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
    writes: u64,
    commands: u64,
    records: u64,
    unresolved: u64,
}

fn min_ts(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

impl Agg {
    fn seen(&mut self, ts: Option<i64>) {
        self.first_ts = min_ts(self.first_ts, ts);
        self.last_ts = self.last_ts.max(ts);
    }

    fn entry(&mut self, path: &str) -> &mut PathAgg {
        self.paths.entry(key(path)).or_insert_with(|| PathAgg { path: path.to_owned(), ..Default::default() })
    }

    fn touch(&mut self, path: &str, how: &'static str, ts: Option<i64>) -> &mut PathAgg {
        let p = self.entry(path);
        let t = p.touches.entry(how).or_insert_with(|| Touch { first: ts, last: ts, ..Default::default() });
        t.first = min_ts(t.first, ts);
        t.last = t.last.max(ts);
        t.count += 1;
        p
    }

    fn made(&mut self, path: &str, m: Made) {
        let p = self.touch(path, m.how, m.ts);
        let t = p.touches.get_mut(m.how).expect("just touched");
        if t.last == m.ts {
            t.command.clone_from(&m.command);
            t.source.clone_from(&m.source);
        }
        if p.made.as_ref().is_none_or(|old| m.ts >= old.ts) {
            p.made = Some(m);
        }
    }

    fn modified(&mut self, path: &str, ts: Option<i64>, tool: &str) {
        let p = self.touch(path, "modified", ts);
        if p.modified.as_ref().is_none_or(|(old, _)| min_ts(ts, *old) == ts && ts != *old) {
            p.modified = Some((ts, tool.to_owned()));
        }
    }

    fn deleted(&mut self, path: &str, ts: Option<i64>) {
        let p = self.touch(path, "deleted", ts);
        p.deleted = Some(p.deleted.flatten().max(ts));
    }

    fn cache(&mut self, cache: &'static str, ts: Option<i64>, command: &str) {
        let c = self.caches.entry(cache).or_default();
        c.first = min_ts(c.first, ts);
        c.last = c.last.max(ts);
        c.uses += 1;
        command.clone_into(&mut c.command);
    }
}

/// Folds one record into `agg`.
fn record(agg: &mut Agg, rec: Record, env: &PathEnv, cache_dirs: &[(&'static str, PathBuf)]) {
    agg.records += 1;
    match rec {
        Record::Session { ts, version, cwd, .. } => {
            agg.seen(ts);
            if cwd.is_some() {
                agg.cwd = cwd.clone();
                if agg.first_cwd.is_none() {
                    agg.first_cwd = cwd;
                }
            }
            if version.is_some() {
                agg.version = version;
            }
        }
        Record::Message { ts, .. } | Record::ToolCall { ts, .. } | Record::ToolResult { ts, .. } | Record::TurnFailed { ts, .. } => agg.seen(ts),
        Record::FileWrite { ts, tool, path, op, .. } => {
            agg.seen(ts);
            agg.writes += 1;
            let cwd = agg.cwd.clone();
            let Some(p) = resolve(&path, cwd.as_deref(), env) else {
                agg.unresolved += 1;
                return;
            };
            let made = |how| Made { how, ts, tool: tool.clone(), command: None, source: None };
            match op {
                FileOp::Create => agg.made(&p, made("created")),
                FileOp::Write => agg.made(&p, made("written")),
                FileOp::Overwrite | FileOp::Modify => agg.modified(&p, ts, &tool),
                FileOp::Delete => agg.deleted(&p, ts),
                FileOp::Move { from } => {
                    agg.made(&p, made("moved"));
                    if let Some(f) = resolve(&from, cwd.as_deref(), env) {
                        agg.deleted(&f, ts);
                    }
                }
            }
        }
        Record::Command { ts, tool, shell: sh, text, cwd, .. } => {
            agg.seen(ts);
            agg.commands += 1;
            let cwd = cwd.or_else(|| agg.cwd.clone());
            // cmd.exe has no backslash escapes, like PowerShell.
            let sh = if sh == Shell::Cmd { Shell::PowerShell } else { sh };
            let is_dir = |p: &str| Path::new(p).is_dir();
            let dctx = downloads::Ctx { env, is_dir: &is_dir };
            let simple = shell::commands(&text, &sh, cwd.as_deref(), env);
            let mut found: Vec<Found> = simple.iter().flat_map(|c| downloads::find(c, &dctx)).collect();
            found.extend(downloads::find_in_text(&text, cwd.as_deref(), &dctx, &simple));
            for f in found {
                match f {
                    Found::Path { path, how, source, summary } => match caches::containing(&path, cache_dirs) {
                        Some(cache) => agg.cache(cache, ts, &summary),
                        None => agg.made(&path, Made { how: how.as_str(), ts, tool: tool.clone(), command: Some(summary), source }),
                    },
                    Found::Cache { cache, summary } => agg.cache(cache, ts, &summary),
                }
            }
        }
    }
}

#[derive(Debug)]
struct Stored {
    size: i64,
    modified_ms: Option<i64>,
    read_to: i64,
    cwd: Option<String>,
    stopped: bool,
}

fn stored(ctx: &ModuleCtx, path: &str) -> Result<Option<Stored>, ApiError> {
    let rows = ctx.db().query("SELECT size, modified_ms, read_to, cwd, stop FROM files_transcripts WHERE path = ?1", &[path.into()])?;
    Ok(rows.first().map(|r| Stored {
        size: r.int(0).unwrap_or(0),
        modified_ms: r.opt_int(1).ok().flatten(),
        read_to: r.int(2).unwrap_or(0),
        cwd: r.opt_text(3).ok().flatten(),
        stopped: r.opt_text(4).ok().flatten().is_some(),
    }))
}

/// Keys of every folder that holds transcripts and of all folders above them.
type AgentDirs = HashSet<String>;

fn read_all(ctx: &ModuleCtx, opts: &Options, stop: &AtomicBool, progress: &Mutex<Progress>, t: &mut Totals) -> Result<AgentDirs, ApiError> {
    set_progress(progress, |p| p.phase = Some("listing"));
    let mut files: Vec<(&'static dyn agents::Adapter, TranscriptFile)> = Vec::new();
    for a in agents::all() {
        match a.transcripts(&opts.env) {
            Ok(list) => files.extend(list.into_iter().map(|f| (*a, f))),
            Err(e) => dlog!("WARN", "files: cannot list {} transcripts: {e}", a.info().id),
        }
    }
    let mut agent_dirs = AgentDirs::new();
    for (_, f) in &files {
        let dir = f.path.parent().map(|p| p.to_string_lossy().into_owned());
        let mut at = dir.and_then(|d| resolve(&d, None, &opts.paths));
        while let Some(p) = at {
            if !agent_dirs.insert(key(&p)) {
                break;
            }
            at = parent(&p).map(str::to_owned);
        }
    }
    // Main transcripts before subagents', so a conversation's folder is its own.
    files.sort_by(|x, y| (x.1.subagent.is_some(), &x.1.path).cmp(&(y.1.subagent.is_some(), &y.1.path)));
    t.transcripts = files.len() as u64;
    set_progress(progress, |p| {
        p.phase = Some("reading");
        p.total = files.len() as u64;
        p.done = 0;
    });
    let cache_dirs = caches::locations(&opts.paths);
    for (adapter, file) in &files {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Err(e) = read_one(ctx, opts, *adapter, file, &cache_dirs, t) {
            t.errors += 1;
            dlog!("WARN", "files: {}: {}", file.path.display(), e.message);
        }
        set_progress(progress, |p| p.done += 1);
    }
    Ok(agent_dirs)
}

fn read_one(
    ctx: &ModuleCtx,
    opts: &Options,
    adapter: &dyn agents::Adapter,
    file: &TranscriptFile,
    cache_dirs: &[(&'static str, PathBuf)],
    t: &mut Totals,
) -> Result<(), ApiError> {
    let path = file.path.to_string_lossy().into_owned();
    let before = stored(ctx, &path)?;
    let size = file.size as i64;
    if let Some(s) = &before
        && s.size == size
        && s.modified_ms == file.modified_ms
        && (s.read_to == size || file.encoding == Encoding::Zstd)
        && (!s.stopped || file.encoding == Encoding::Zstd)
    {
        if file.encoding == Encoding::Zstd {
            t.compressed += 1;
        }
        return Ok(());
    }
    // A file that got shorter was replaced: read it again from the start.
    let from = before.as_ref().map(|s| s.read_to).filter(|&r| r <= size).unwrap_or(0);
    let mut agg = Agg { cwd: before.and_then(|s| s.cwd).filter(|_| from > 0), ..Default::default() };
    let outcome: ReadOutcome = adapter
        .read(file, from as u64, ReadOptions { text: false }, &mut |r| record(&mut agg, r, &opts.paths, cache_dirs))
        .map_err(|e| ApiError::internal(format!("read: {e}")))?;
    match &outcome.stop {
        Some(Stop::Compressed) => t.compressed += 1,
        Some(_) => t.stopped += 1,
        None => {}
    }
    t.read_files += 1;
    t.read_bytes += outcome.offset.saturating_sub(from as u64);
    t.records += agg.records;
    t.unresolved += agg.unresolved;
    ctx.db().transaction(|c| apply(c, file, &path, &agg, &outcome))
}

fn sql_opt(v: Option<i64>) -> Sql {
    v.map(Sql::Int).unwrap_or(Sql::Null)
}

fn sql_str(v: Option<&str>) -> Sql {
    v.map(|s| Sql::Text(s.to_owned())).unwrap_or(Sql::Null)
}

/// Writes one transcript's findings (one transaction).
fn apply(c: &Connection, file: &TranscriptFile, path: &str, agg: &Agg, outcome: &ReadOutcome) -> sqlite::Result<()> {
    let agent = file.agent;
    let conv = file.conversation_id.as_str();
    for (k, p) in &agg.paths {
        let id = own(c, agent, conv, k, p)?;
        if let Some(id) = id {
            for (how, t) in &p.touches {
                touch_row(c, id, agent, conv, how, t)?;
            }
        }
    }
    for (cache, u) in &agg.caches {
        c.execute(
            "INSERT INTO files_cache_uses (cache, agent, conversation_id, first_ts, last_ts, uses, command)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (cache, agent, conversation_id) DO UPDATE SET
               first_ts = coalesce(min(first_ts, excluded.first_ts), first_ts, excluded.first_ts),
               last_ts = coalesce(max(last_ts, excluded.last_ts), last_ts, excluded.last_ts),
               uses = uses + excluded.uses,
               command = CASE WHEN excluded.last_ts >= coalesce(last_ts, excluded.last_ts) THEN excluded.command ELSE command END",
            &[(*cache).into(), agent.into(), conv.into(), sql_opt(u.first), sql_opt(u.last), (u.uses as i64).into(), u.command.as_str().into()],
        )?;
    }
    if agg.records > 0 {
        c.execute(
            "INSERT INTO files_conversations (agent, conversation_id, cwd, version, first_ts, last_ts, writes, commands)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (agent, conversation_id) DO UPDATE SET
               cwd = coalesce(cwd, excluded.cwd),
               version = coalesce(excluded.version, version),
               first_ts = coalesce(min(first_ts, excluded.first_ts), first_ts, excluded.first_ts),
               last_ts = coalesce(max(last_ts, excluded.last_ts), last_ts, excluded.last_ts),
               writes = writes + excluded.writes,
               commands = commands + excluded.commands",
            &[
                agent.into(),
                conv.into(),
                sql_str(agg.first_cwd.as_deref()),
                sql_str(agg.version.as_deref()),
                sql_opt(agg.first_ts),
                sql_opt(agg.last_ts),
                (agg.writes as i64).into(),
                (agg.commands as i64).into(),
            ],
        )?;
    }
    let stop = outcome.stop.as_ref().map(|s| serde_json::to_string(s).unwrap_or_default());
    c.execute(
        "INSERT INTO files_transcripts (path, agent, conversation_id, subagent, size, modified_ms, read_to, cwd, version, stop, scanned_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT (path) DO UPDATE SET
           size = excluded.size, modified_ms = excluded.modified_ms, read_to = excluded.read_to,
           cwd = coalesce(excluded.cwd, cwd), version = coalesce(excluded.version, version),
           stop = excluded.stop, scanned_at = excluded.scanned_at",
        &[
            path.into(),
            agent.into(),
            conv.into(),
            sql_str(file.subagent.as_deref()),
            (file.size as i64).into(),
            sql_opt(file.modified_ms),
            (outcome.offset as i64).into(),
            sql_str(agg.cwd.as_deref()),
            sql_str(outcome.version.as_deref().or(agg.version.as_deref())),
            sql_str(stop.as_deref()),
            now_ms().into(),
        ],
    )?;
    Ok(())
}

fn touch_row(c: &Connection, id: i64, agent: &str, conv: &str, how: &str, t: &Touch) -> sqlite::Result<usize> {
    c.execute(
        "INSERT INTO files_touches (path_id, agent, conversation_id, how, first_ts, last_ts, count, command, source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (path_id, agent, conversation_id, how) DO UPDATE SET
           first_ts = coalesce(min(first_ts, excluded.first_ts), first_ts, excluded.first_ts),
           last_ts = coalesce(max(last_ts, excluded.last_ts), last_ts, excluded.last_ts),
           count = count + excluded.count,
           command = CASE WHEN excluded.last_ts >= coalesce(last_ts, excluded.last_ts) THEN coalesce(excluded.command, command) ELSE command END,
           source = CASE WHEN excluded.last_ts >= coalesce(last_ts, excluded.last_ts) THEN coalesce(excluded.source, source) ELSE source END",
        &[
            id.into(),
            agent.into(),
            conv.into(),
            how.into(),
            sql_opt(t.first),
            sql_opt(t.last),
            (t.count as i64).into(),
            sql_str(t.command.as_deref()),
            sql_str(t.source.as_deref()),
        ],
    )
}

/// Applies the ownership rule for one path; its row id if the path has a row.
fn own(c: &Connection, agent: &str, conv: &str, k: &str, p: &PathAgg) -> sqlite::Result<Option<i64>> {
    let row = c.query_row("SELECT id, how, ts, deleted_ts FROM files_paths WHERE path_key = ?1", &[k.into()])?;
    let mut current: Option<(i64, String, Option<i64>)> = match &row {
        Some(r) => Some((r.int(0)?, r.text(1)?, r.opt_int(2)?)),
        None => None,
    };
    let take = |c: &Connection, id: Option<i64>, how: &str, ts: Option<i64>, m: Option<&Made>, tool: &str| -> sqlite::Result<i64> {
        let params = [
            k.into(),
            p.path.as_str().into(),
            agent.into(),
            conv.into(),
            how.into(),
            sql_opt(ts),
            tool.into(),
            sql_str(m.and_then(|m| m.command.as_deref())),
            sql_str(m.and_then(|m| m.source.as_deref())),
        ];
        match id {
            None => {
                c.execute(
                    "INSERT INTO files_paths (path_key, path, agent, conversation_id, how, ts, tool, command, source)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    &params,
                )?;
                Ok(c.last_insert_rowid())
            }
            Some(id) => {
                c.execute(
                    "UPDATE files_paths SET path = ?2, agent = ?3, conversation_id = ?4, how = ?5, ts = ?6, tool = ?7,
                       command = ?8, source = ?9,
                       deleted_ts = CASE WHEN deleted_ts > coalesce(?6, 0) THEN deleted_ts ELSE NULL END
                     WHERE path_key = ?1",
                    &params,
                )?;
                Ok(id)
            }
        }
    };
    if let Some(m) = &p.made {
        let wins = match &current {
            None => true,
            Some((_, how, ts)) => how == "modified" || m.ts >= *ts,
        };
        if wins {
            let id = take(c, current.as_ref().map(|x| x.0), m.how, m.ts, Some(m), &m.tool)?;
            current = Some((id, m.how.to_owned(), m.ts));
        }
    } else if let Some((ts, tool)) = &p.modified {
        let wins = match &current {
            None => true,
            Some((_, how, old)) => how == "modified" && min_ts(*ts, *old) == *ts && ts != old,
        };
        if wins {
            let id = take(c, current.as_ref().map(|x| x.0), "modified", *ts, None, tool)?;
            current = Some((id, "modified".to_owned(), *ts));
        }
    }
    if let (Some(del), Some((id, _, ts))) = (p.deleted, &current)
        && del >= *ts
    {
        c.execute(
            "UPDATE files_paths SET deleted_ts = CASE WHEN coalesce(deleted_ts, 0) > coalesce(?2, 0) THEN deleted_ts ELSE ?2 END WHERE id = ?1",
            &[(*id).into(), sql_opt(del)],
        )?;
    }
    Ok(current.map(|x| x.0))
}

// ---- measuring ---------------------------------------------------------------------------

struct Measured {
    id: i64,
    key: String,
    agent: String,
    conv: String,
    how: String,
    ts: Option<i64>,
    size: Size,
    pre_existing: bool,
    agent_data: bool,
    shared: bool,
    counted: u64,
    unattributed: u64,
    /// The owner changed (folder judged by its creation time).
    reowned: Option<(Option<String>, Option<String>)>,
}

impl Measured {
    fn counts(&self) -> bool {
        CREATING.contains(&self.how.as_str()) && !self.pre_existing && !self.agent_data
    }

    fn note(&self) -> Option<&'static str> {
        if self.agent_data {
            Some("agent_data")
        } else if self.shared {
            Some("shared")
        } else {
            None
        }
    }
}

struct Candidate {
    agent: String,
    conv: String,
    how: String,
    first: Option<i64>,
    last: Option<i64>,
    command: Option<String>,
    source: Option<String>,
}

/// The parent folder of a normalised path; `None` at a drive root or a share.
fn parent(path: &str) -> Option<&str> {
    let p = &path[..path.rfind('\\')?];
    let top = if path.starts_with(r"\\") { p.matches('\\').count() <= 3 } else { p.len() <= 2 };
    (!top).then_some(p)
}

/// Creation time of a real folder (not a link), Unix ms.
fn folder_created(path: &str) -> Option<i64> {
    let m = std::fs::symlink_metadata(path).ok()?;
    if !m.file_type().is_dir() {
        return None;
    }
    m.created().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_millis() as i64)
}

/// Adds `created_folder` rows: folders above an attributed path that appeared within
/// [`SLACK_MS`] of the event that made the path. The closest event wins.
fn implicit_folders(ctx: &ModuleCtx, agent_dirs: &AgentDirs, stop: &AtomicBool) -> Result<u64, ApiError> {
    let rows = ctx.db().query("SELECT path_key, path, how, ts, agent, conversation_id FROM files_paths", &[])?;
    let existing: std::collections::HashSet<String> = rows.iter().filter_map(|r| r.text(0).ok()).collect();
    let mut created: HashMap<String, Option<i64>> = HashMap::new();
    // folder key → (distance, path, agent, conversation, event time)
    let mut best: HashMap<String, (i64, String, String, String, i64)> = HashMap::new();
    for r in &rows {
        if stop.load(Ordering::Relaxed) {
            return Ok(0);
        }
        let (Ok(path), Ok(how), Ok(agent), Ok(conv)) = (r.text(1), r.text(2), r.text(4), r.text(5)) else { continue };
        let Some(ts) = r.opt_int(3).ok().flatten().filter(|_| CREATING.contains(&how.as_str())) else { continue };
        let mut at = parent(&path);
        for _ in 0..MAX_PARENTS {
            let Some(p) = at else { break };
            let k = key(p);
            if existing.contains(&k) || agent_dirs.contains(&k) {
                break;
            }
            let c = *created.entry(k.clone()).or_insert_with(|| folder_created(p));
            let Some(d) = c.map(|c| (c - ts).abs()).filter(|d| *d <= SLACK_MS) else { break };
            let e = best.entry(k).or_insert_with(|| (i64::MAX, String::new(), String::new(), String::new(), 0));
            if d < e.0 {
                *e = (d, p.to_owned(), agent.clone(), conv.clone(), ts);
            }
            at = parent(p);
        }
    }
    let n = best.len() as u64;
    let best: Vec<(String, (i64, String, String, String, i64))> = best.into_iter().collect();
    for chunk in best.chunks(MEASURE_BATCH) {
        ctx.db().transaction(|c| {
            for (k, (_, path, agent, conv, ts)) in chunk {
                c.execute(
                    "INSERT OR IGNORE INTO files_paths (path_key, path, agent, conversation_id, how, ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    &[k.as_str().into(), path.as_str().into(), agent.as_str().into(), conv.as_str().into(), FOLDER.into(), (*ts).into()],
                )?;
                let id = c.query_row("SELECT id FROM files_paths WHERE path_key = ?1", &[k.as_str().into()])?.map(|r| r.int(0)).transpose()?;
                if let Some(id) = id {
                    let t = Touch { first: Some(*ts), last: Some(*ts), count: 1, command: None, source: None };
                    c.execute("DELETE FROM files_touches WHERE path_id = ?1 AND how = ?2", &[id.into(), FOLDER.into()])?;
                    touch_row(c, id, agent, conv, FOLDER, &t)?;
                }
            }
            Ok::<(), sqlite::Error>(())
        })?;
    }
    Ok(n)
}

/// Folder owners by creation time (see the module comment).
fn judge_folders(ctx: &ModuleCtx, all: &mut [Measured]) -> Result<(), ApiError> {
    let creating = CREATING.iter().map(|h| format!("'{h}'")).collect::<Vec<_>>().join(", ");
    let rows = ctx.db().query(
        &format!("SELECT path_id, agent, conversation_id, how, first_ts, last_ts, command, source FROM files_touches WHERE how IN ({creating})"),
        &[],
    )?;
    let mut by_path: HashMap<i64, Vec<Candidate>> = HashMap::new();
    for r in &rows {
        let (Ok(id), Ok(agent), Ok(conv), Ok(how)) = (r.int(0), r.text(1), r.text(2), r.text(3)) else { continue };
        let c = Candidate {
            agent,
            conv,
            how,
            first: r.opt_int(4).ok().flatten(),
            last: r.opt_int(5).ok().flatten(),
            command: r.opt_text(6).ok().flatten(),
            source: r.opt_text(7).ok().flatten(),
        };
        by_path.entry(id).or_default().push(c);
    }
    for m in all.iter_mut().filter(|m| m.size.kind == Kind::Dir && CREATING.contains(&m.how.as_str())) {
        let Some(created) = m.size.created_ms else { continue };
        let latest_allowed = created + SLACK_MS;
        let cands = by_path.get(&m.id).map(Vec::as_slice).unwrap_or(&[]);
        let pick = cands
            .iter()
            .filter_map(|c| {
                let ts = [c.last, c.first].into_iter().flatten().find(|t| *t <= latest_allowed)?;
                Some((ts, c))
            })
            .max_by_key(|(ts, _)| *ts);
        match pick {
            Some((ts, c)) => {
                if c.agent != m.agent || c.conv != m.conv || c.how != m.how {
                    (m.agent, m.conv, m.how) = (c.agent.clone(), c.conv.clone(), c.how.clone());
                    m.reowned = Some((c.command.clone(), c.source.clone()));
                }
                m.ts = Some(ts);
            }
            // Every event that made it came after the folder existed.
            None => m.pre_existing = cands.iter().any(|c| c.first.is_some()) || m.ts.is_some_and(|t| created < t - SLACK_MS),
        }
    }
    Ok(())
}

fn measure_all(
    ctx: &ModuleCtx,
    opts: &Options,
    agent_dirs: &AgentDirs,
    stop: &AtomicBool,
    progress: &Mutex<Progress>,
    t: &mut Totals,
) -> Result<(), ApiError> {
    set_progress(progress, |p| p.phase = Some("measuring"));
    t.folders = implicit_folders(ctx, agent_dirs, stop)?;
    let rows = ctx.db().query("SELECT id, path_key, path, how, ts, agent, conversation_id FROM files_paths", &[])?;
    let cache_dirs = caches::locations(&opts.paths);
    set_progress(progress, |p| {
        p.total = (rows.len() + cache_dirs.len()) as u64;
        p.done = 0;
    });
    let mut all: Vec<Measured> = Vec::with_capacity(rows.len());
    for r in &rows {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let (Ok(id), Ok(k), Ok(path), Ok(how), Ok(agent), Ok(conv)) = (r.int(0), r.text(1), r.text(2), r.text(3), r.text(5), r.text(6)) else {
            continue;
        };
        let ts = r.opt_int(4).ok().flatten();
        let size = measure::measure(Path::new(&path), opts.max_entries, stop);
        let agent_data = agent_dirs.contains(&k);
        all.push(Measured {
            id,
            key: k,
            agent,
            conv,
            how,
            ts,
            size,
            pre_existing: false,
            agent_data,
            shared: false,
            counted: 0,
            unattributed: 0,
            reowned: None,
        });
        set_progress(progress, |p| p.done += 1);
    }
    judge_folders(ctx, &mut all)?;
    subtract_nested(&mut all);
    t.measured = all.len() as u64;
    t.reowned = all.iter().filter(|m| m.reowned.is_some()).count() as u64;
    let now = now_ms();
    for chunk in all.chunks(MEASURE_BATCH) {
        ctx.db().transaction(|c| {
            for m in chunk {
                c.execute(
                    "UPDATE files_paths SET kind = ?2, bytes = ?3, files = ?4, counted = ?5, pre_existing = ?6, truncated = ?7,
                       measured_at = ?8, note = ?9, unattributed = ?10 WHERE id = ?1",
                    &[
                        m.id.into(),
                        m.size.kind.as_str().into(),
                        (m.size.bytes as i64).into(),
                        (m.size.files as i64).into(),
                        (m.counted as i64).into(),
                        m.pre_existing.into(),
                        m.size.truncated.into(),
                        now.into(),
                        sql_str(m.note()),
                        (m.unattributed as i64).into(),
                    ],
                )?;
                if let Some((command, source)) = &m.reowned {
                    c.execute(
                        "UPDATE files_paths SET agent = ?2, conversation_id = ?3, how = ?4, ts = ?5, command = ?6, source = ?7, tool = NULL WHERE id = ?1",
                        &[
                            m.id.into(),
                            m.agent.as_str().into(),
                            m.conv.as_str().into(),
                            m.how.as_str().into(),
                            sql_opt(m.ts),
                            sql_str(command.as_deref()),
                            sql_str(source.as_deref()),
                        ],
                    )?;
                }
            }
            Ok::<(), sqlite::Error>(())
        })?;
    }
    for (cache, path) in &cache_dirs {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let size = measure::measure(path, opts.max_entries, stop);
        ctx.db().execute(
            "INSERT INTO files_caches (cache, path, kind, bytes, files, truncated, measured_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (cache, path) DO UPDATE SET kind = excluded.kind, bytes = excluded.bytes, files = excluded.files,
               truncated = excluded.truncated, measured_at = excluded.measured_at",
            &[
                (*cache).into(),
                caches::display(path).into(),
                size.kind.as_str().into(),
                (size.bytes as i64).into(),
                (size.files as i64).into(),
                size.truncated.into(),
                now.into(),
            ],
        )?;
        set_progress(progress, |p| p.done += 1);
    }
    // Caches no longer there.
    ctx.db().execute("DELETE FROM files_caches WHERE measured_at < ?1", &[now.into()])?;
    Ok(())
}

/// Takes each counted path's bytes out of the nearest counted folder that contains it, so
/// the sum over all paths is the size of their union; then moves what is left of shared
/// folders to `unattributed`.
fn subtract_nested(all: &mut [Measured]) {
    for m in all.iter_mut() {
        m.counted = if m.counts() { m.size.bytes } else { 0 };
    }
    // Sorting with `\` as the smallest character keeps every folder's contents right after it.
    let sort_key = |k: &str| k.replace('\\', "\u{1}");
    let mut order: Vec<usize> = (0..all.len()).collect();
    order.sort_by_cached_key(|&i| sort_key(&all[i].key));
    let mut stack: Vec<usize> = Vec::new();
    // Per folder: counted paths inside it, and how many of them its own owner made.
    let (mut inside, mut own) = (vec![0u32; all.len()], vec![0u32; all.len()]);
    for i in order {
        while let Some(&top) = stack.last() {
            if is_within(&all[i].key, &all[top].key) && all[i].key != all[top].key {
                break;
            }
            stack.pop();
        }
        if !all[i].counts() {
            continue;
        }
        if let Some(&parent) = stack.last() {
            let b = all[i].size.bytes;
            all[parent].counted = all[parent].counted.saturating_sub(b);
        }
        for &a in &stack {
            inside[a] += 1;
            if (&all[a].agent, &all[a].conv) == (&all[i].agent, &all[i].conv) {
                own[a] += 1;
            }
        }
        if all[i].size.kind == Kind::Dir {
            stack.push(i);
        }
    }
    for (i, m) in all.iter_mut().enumerate() {
        // A clone, download or unpacked archive explains its own contents. Otherwise a folder
        // where other conversations made at least as much as its owner (the folder itself
        // counting for the owner) is shared.
        let explained = matches!(m.how.as_str(), "downloaded" | "cloned" | "extracted" | "moved");
        m.shared = !explained && inside[i] > own[i] && (1 + own[i]) * 2 <= 1 + inside[i];
        if m.shared {
            m.unattributed = std::mem::take(&mut m.counted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(conv: &str, key: &str, how: &str, kind: Kind, bytes: u64) -> Measured {
        let size = Size { kind, bytes, files: 1, created_ms: None, truncated: false };
        Measured {
            id: 0,
            key: key.into(),
            agent: "a".into(),
            conv: conv.into(),
            how: how.into(),
            ts: None,
            size,
            pre_existing: false,
            agent_data: false,
            shared: false,
            counted: 0,
            unattributed: 0,
            reowned: None,
        }
    }

    fn m(key: &str, how: &str, kind: Kind, bytes: u64) -> Measured {
        owned("x", key, how, kind, bytes)
    }

    #[test]
    fn leftovers_of_shared_folders_are_unattributed() {
        let mut all = vec![
            owned("x", r"d:\proj", "created_folder", Kind::Dir, 100),
            owned("x", r"d:\proj\a", "created", Kind::File, 10),
            owned("y", r"d:\proj\b", "created", Kind::File, 20),
            owned("y", r"d:\proj\run", "written", Kind::Dir, 40),
            // One stray file of another conversation does not make a folder shared.
            owned("x", r"d:\solo", "written", Kind::Dir, 50),
            owned("x", r"d:\solo\a", "created", Kind::File, 1),
            owned("y", r"d:\solo\b", "created", Kind::File, 2),
        ];
        subtract_nested(&mut all);
        let got: Vec<(u64, u64, bool)> = all.iter().map(|m| (m.counted, m.unattributed, m.shared)).collect();
        assert_eq!(got, [(0, 30, true), (10, 0, false), (20, 0, false), (40, 0, false), (47, 0, false), (1, 0, false), (2, 0, false)]);
    }

    #[test]
    fn parents_stop_at_roots() {
        assert_eq!(parent(r"D:\a\b"), Some(r"D:\a"));
        assert_eq!(parent(r"D:\a"), None);
        assert_eq!(parent(r"\\srv\share\a\b"), Some(r"\\srv\share\a"));
        assert_eq!(parent(r"\\srv\share\a"), None);
    }

    #[test]
    fn nested_paths_are_counted_once() {
        let mut all = vec![
            m(r"d:\w\repo", "cloned", Kind::Dir, 100),
            m(r"d:\w\repo\out", "written", Kind::Dir, 30),
            m(r"d:\w\repo\out\a.bin", "created", Kind::File, 10),
            m(r"d:\w\repo\src\x.py", "modified", Kind::File, 5),
            m(r"d:\w\repo b", "downloaded", Kind::File, 7),
            m(r"d:\w\repo-2\z", "created", Kind::File, 1),
        ];
        subtract_nested(&mut all);
        let counted: Vec<u64> = all.iter().map(|x| x.counted).collect();
        assert_eq!(counted, [70, 20, 10, 0, 7, 1]);
        assert_eq!(counted.iter().sum::<u64>(), 108);
    }
}
