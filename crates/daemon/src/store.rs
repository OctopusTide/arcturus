//! SQLite store: versioned migrations per component, the framework's own `core_*` tables,
//! and connection access for modules' `<module>_*` tables.
//!
//! Every component (the framework is component `core`, each module is one too) declares an
//! ordered list of [`Migration`]s. `schema_versions` records how far each component has got;
//! on start the pending steps run in order, each in its own transaction. Adding a module or
//! changing a table never needs a rebuilt database. Only the daemon touches the database;
//! everyone else goes through the protocol.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use proto::commands::module::CORE;
use serde::Serialize;
use serde_json::Value as Json;

use crate::module::Migration;
use crate::sqlite::{Connection, Error, Result, Row, SqliteLibrary, Value};

const BOOTSTRAP: &str = "CREATE TABLE IF NOT EXISTS schema_versions (
    component  TEXT PRIMARY KEY,
    version    INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
)";

/// The framework's own tables. Append only.
pub static CORE_MIGRATIONS: [Migration; 1] = [Migration::new(
    1,
    r#"
-- Key/value settings. scope = 'core' or a module id; value = JSON.
CREATE TABLE core_settings (
    scope      TEXT    NOT NULL,
    key        TEXT    NOT NULL,
    value      TEXT    NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (scope, key)
);

-- Who asked for what. client_pid/client_exe come from the OS, client_name is self-reported.
CREATE TABLE core_op_log (
    id          INTEGER PRIMARY KEY,
    ts          INTEGER NOT NULL,
    request_id  TEXT,
    cmd         TEXT    NOT NULL,
    module      TEXT,
    args        TEXT,
    client_pid  INTEGER,
    client_exe  TEXT,
    client_name TEXT,
    caller_kind TEXT,
    ok          INTEGER NOT NULL,
    error_code  TEXT,
    subcode     TEXT,
    duration_ms INTEGER NOT NULL
);
CREATE INDEX core_op_log_ts ON core_op_log (ts);

-- Recovery journal: the original value is written here before any change is made.
CREATE TABLE core_journal (
    id          INTEGER PRIMARY KEY,
    module      TEXT    NOT NULL,
    key         TEXT    NOT NULL,
    original    TEXT    NOT NULL,
    applied     TEXT,
    note        TEXT,
    status      TEXT    NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'restored')),
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    restored_at INTEGER,
    restored_by TEXT,
    attempts    INTEGER NOT NULL DEFAULT 0,
    last_error  TEXT
);
-- At most one open entry per (module, key): its `original` is the true original.
CREATE UNIQUE INDEX core_journal_one_open ON core_journal (module, key) WHERE status = 'open';
"#,
)];

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// One recovery-journal entry that has not been restored yet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JournalEntry {
    pub id: i64,
    pub module: String,
    /// Module-chosen name of the changed setting, e.g. `"lid_action_ac"`.
    pub key: String,
    /// Value before the module touched it. This is what recovery puts back.
    pub original: Json,
    /// Latest value the module set (informational).
    pub applied: Option<Json>,
    pub note: Option<String>,
    pub created_at: i64,
    /// Failed restore attempts so far.
    pub attempts: i64,
    pub last_error: Option<String>,
}

/// One operation-log row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpRecord {
    pub id: i64,
    pub ts: i64,
    pub request_id: Option<String>,
    pub cmd: String,
    pub module: Option<String>,
    pub args: Option<Json>,
    pub client_pid: Option<i64>,
    pub client_exe: Option<String>,
    pub client_name: Option<String>,
    pub caller_kind: Option<String>,
    pub ok: bool,
    pub error_code: Option<String>,
    pub subcode: Option<String>,
    pub duration_ms: i64,
}

/// Outcome of migrating one component.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MigrationReport {
    pub from: u32,
    pub to: u32,
}

pub struct Store {
    pub(crate) arbitration: Mutex<crate::arbitration::Arbitration>,
    conn: Mutex<Connection>,
    path: PathBuf,
    library: String,
}

fn json_text(v: &Json) -> Value {
    Value::Text(v.to_string())
}

fn parse_json(col: usize, text: Option<String>) -> Result<Option<Json>> {
    text.map(|t| serde_json::from_str(&t).map_err(|e| Error::new(-1, format!("column {col}: bad JSON: {e}"))))
        .transpose()
}

const JOURNAL_COLUMNS: &str = "id, module, key, original, applied, note, created_at, attempts, last_error";

fn journal_entry(row: &Row) -> Result<JournalEntry> {
    Ok(JournalEntry {
        id: row.int(0)?,
        module: row.text(1)?,
        key: row.text(2)?,
        original: parse_json(3, Some(row.text(3)?))?.unwrap_or(Json::Null),
        applied: parse_json(4, row.opt_text(4)?)?,
        note: row.opt_text(5)?,
        created_at: row.int(6)?,
        attempts: row.int(7)?,
        last_error: row.opt_text(8)?,
    })
}

fn schema_objects(c: &Connection) -> Result<Vec<String>> {
    c.query("SELECT name FROM sqlite_master WHERE name NOT LIKE 'sqlite\\_%' ESCAPE '\\'", &[])?
        .iter()
        .map(|r| r.text(0))
        .collect()
}

impl Store {
    /// Opens the database (creating it if needed) and brings the framework's own tables up
    /// to date. Fails if the database was written by a newer build.
    pub fn open(path: &Path, lib: &dyn SqliteLibrary) -> Result<Store> {
        let conn = Connection::open(path, lib)?;
        // FULL sync: a journal entry is on disk before the change it protects is made.
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(BOOTSTRAP)?;
        let library = format!("{} {}", lib.describe(), conn.library_version());
        let store = Store { arbitration: Mutex::new(Default::default()), conn: Mutex::new(conn), path: path.to_owned(), library };
        store.migrate(CORE, &CORE_MIGRATIONS)?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Which SQLite library is in use, for status output.
    pub fn library(&self) -> &str {
        &self.library
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs `f` with the connection held.
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        f(&self.conn())
    }

    /// Schema version of `component` (0 = never migrated).
    pub fn schema_version(&self, component: &str) -> Result<u32> {
        Ok(self
            .conn()
            .query_row("SELECT version FROM schema_versions WHERE component = ?", &[component.into()])?
            .map(|r| r.int(0))
            .transpose()?
            .unwrap_or(0) as u32)
    }

    /// Brings `component` up to the last of `migrations`. Each step runs in its own
    /// transaction and may only create schema objects named `<component>_…`; a step that
    /// breaks the rule is rolled back. Fails if the database is newer than `migrations`.
    pub fn migrate(&self, component: &str, migrations: &[Migration]) -> Result<MigrationReport> {
        for (i, m) in migrations.iter().enumerate() {
            if m.version as usize != i + 1 {
                return Err(Error::new(-1, format!("{component}: migrations must be numbered 1, 2, 3 … in order")));
            }
        }
        let from = self.schema_version(component)?;
        let to = migrations.len() as u32;
        if from > to {
            return Err(Error::new(
                -1,
                format!("{component}: database schema is version {from}, newer than this build ({to})"),
            ));
        }
        let prefix = format!("{component}_");
        let conn = self.conn();
        for m in &migrations[from as usize..] {
            conn.transaction(|c| {
                let before = schema_objects(c)?;
                c.execute_batch(m.sql)?;
                let bad: Vec<String> = schema_objects(c)?
                    .into_iter()
                    .filter(|n| !before.contains(n) && !n.starts_with(&prefix))
                    .collect();
                if !bad.is_empty() {
                    return Err(Error::new(
                        -1,
                        format!("{component} migration {}: objects must be named {prefix}…, got {bad:?}", m.version),
                    ));
                }
                c.execute(
                    "INSERT INTO schema_versions (component, version, updated_at) VALUES (?, ?, ?)
                     ON CONFLICT (component) DO UPDATE SET version = excluded.version, updated_at = excluded.updated_at",
                    &[component.into(), m.version.into(), now_ms().into()],
                )?;
                Ok(())
            })?;
        }
        Ok(MigrationReport { from, to })
    }

    // ---- settings ----

    pub fn setting_get(&self, scope: &str, key: &str) -> Result<Option<Json>> {
        let row = self
            .conn()
            .query_row("SELECT value FROM core_settings WHERE scope = ? AND key = ?", &[scope.into(), key.into()])?;
        match row {
            Some(r) => parse_json(0, Some(r.text(0)?)),
            None => Ok(None),
        }
    }

    pub fn setting_set(&self, scope: &str, key: &str, value: &Json) -> Result<()> {
        self.conn().execute(
            "INSERT INTO core_settings (scope, key, value, updated_at) VALUES (?, ?, ?, ?)
             ON CONFLICT (scope, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            &[scope.into(), key.into(), json_text(value), now_ms().into()],
        )?;
        Ok(())
    }

    // ---- operation log ----

    pub fn oplog_append(&self, rec: &OpRecord) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO core_op_log (ts, request_id, cmd, module, args, client_pid, client_exe, client_name,
                                      caller_kind, ok, error_code, subcode, duration_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                rec.ts.into(),
                rec.request_id.clone().into(),
                rec.cmd.clone().into(),
                rec.module.clone().into(),
                rec.args.as_ref().map(Json::to_string).into(),
                rec.client_pid.into(),
                rec.client_exe.clone().into(),
                rec.client_name.clone().into(),
                rec.caller_kind.clone().into(),
                rec.ok.into(),
                rec.error_code.clone().into(),
                rec.subcode.clone().into(),
                rec.duration_ms.into(),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Newest first.
    pub fn oplog_recent(&self, limit: u32) -> Result<Vec<OpRecord>> {
        let rows = self.conn().query(
            "SELECT id, ts, request_id, cmd, module, args, client_pid, client_exe, client_name,
                    caller_kind, ok, error_code, subcode, duration_ms
             FROM core_op_log ORDER BY id DESC LIMIT ?",
            &[limit.into()],
        )?;
        rows.iter()
            .map(|r| {
                Ok(OpRecord {
                    id: r.int(0)?,
                    ts: r.int(1)?,
                    request_id: r.opt_text(2)?,
                    cmd: r.text(3)?,
                    module: r.opt_text(4)?,
                    args: parse_json(5, r.opt_text(5)?)?,
                    client_pid: r.opt_int(6)?,
                    client_exe: r.opt_text(7)?,
                    client_name: r.opt_text(8)?,
                    caller_kind: r.opt_text(9)?,
                    ok: r.int(10)? != 0,
                    error_code: r.opt_text(11)?,
                    subcode: r.opt_text(12)?,
                    duration_ms: r.int(13)?,
                })
            })
            .collect()
    }

    // ---- recovery journal ----

    /// Records `original` for (`module`, `key`) before a change. If an open entry for the
    /// same key already exists, it is kept (its original is the true one) and only
    /// `applied`/`note` are updated. Returns the open entry.
    pub fn journal_record(
        &self,
        module: &str,
        key: &str,
        original: &Json,
        applied: Option<&Json>,
        note: Option<&str>,
    ) -> Result<JournalEntry> {
        let now = now_ms();
        let conn = self.conn();
        conn.transaction(|c| {
            let existing = c.query_row(
                "SELECT id FROM core_journal WHERE module = ? AND key = ? AND status = 'open'",
                &[module.into(), key.into()],
            )?;
            let id = match existing {
                Some(row) => {
                    let id = row.int(0)?;
                    c.execute(
                        "UPDATE core_journal SET applied = ?, note = coalesce(?, note), updated_at = ? WHERE id = ?",
                        &[applied.map(json_text).into(), note.into(), now.into(), id.into()],
                    )?;
                    id
                }
                None => {
                    c.execute(
                        "INSERT INTO core_journal (module, key, original, applied, note, created_at, updated_at)
                         VALUES (?, ?, ?, ?, ?, ?, ?)",
                        &[
                            module.into(),
                            key.into(),
                            json_text(original),
                            applied.map(json_text).into(),
                            note.into(),
                            now.into(),
                            now.into(),
                        ],
                    )?;
                    c.last_insert_rowid()
                }
            };
            let row = c
                .query_row(&format!("SELECT {JOURNAL_COLUMNS} FROM core_journal WHERE id = ?"), &[id.into()])?
                .ok_or_else(|| Error::new(-1, "journal entry vanished"))?;
            journal_entry(&row)
        })
    }

    /// Marks an open entry of `module` restored. Returns false if it was not open.
    pub fn journal_mark_restored(&self, module: &str, id: i64, restored_by: &str) -> Result<bool> {
        let now = now_ms();
        let n = self.conn().execute(
            "UPDATE core_journal SET status = 'restored', restored_at = ?, restored_by = ?, updated_at = ?
             WHERE id = ? AND module = ? AND status = 'open'",
            &[now.into(), restored_by.into(), now.into(), id.into(), module.into()],
        )?;
        Ok(n == 1)
    }

    /// Counts a failed restore attempt; the entry stays open.
    pub fn journal_record_failure(&self, id: i64, error: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE core_journal SET attempts = attempts + 1, last_error = ?, updated_at = ? WHERE id = ?",
            &[error.into(), now_ms().into(), id.into()],
        )?;
        Ok(())
    }

    /// Open entries, newest first (the order recovery undoes them in).
    pub fn journal_open(&self, module: Option<&str>) -> Result<Vec<JournalEntry>> {
        let conn = self.conn();
        let rows = match module {
            Some(m) => conn.query(
                &format!("SELECT {JOURNAL_COLUMNS} FROM core_journal WHERE status = 'open' AND module = ? ORDER BY id DESC"),
                &[m.into()],
            )?,
            None => conn.query(
                &format!("SELECT {JOURNAL_COLUMNS} FROM core_journal WHERE status = 'open' ORDER BY id DESC"),
                &[],
            )?,
        };
        rows.iter().map(journal_entry).collect()
    }

    /// `(status, restored_by)` of an entry; for tests and diagnostics.
    pub fn journal_status(&self, id: i64) -> Result<Option<(String, Option<String>)>> {
        self.conn()
            .query_row("SELECT status, restored_by FROM core_journal WHERE id = ?", &[id.into()])?
            .map(|r| Ok((r.text(0)?, r.opt_text(1)?)))
            .transpose()
    }
}
