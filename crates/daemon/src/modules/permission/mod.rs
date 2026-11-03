//! The cannot-close allowlist. Only trusted UI callers may edit or approve requests.
use crate::arbitration::ProgramRule;
use crate::module::{
    ArgKind, ArgSpec, CommandSpec, Event, EventSpec, Migration, Module, ModuleCtx, ModuleRequest,
    ModuleSpec, RecoverOutcome, Section,
};
use crate::store::JournalEntry;
use proto::{ApiError, ErrorCode};
use serde_json::{Value, json};

const PROGRAM: ArgSpec = ArgSpec::new("program", ArgKind::Str, "Full executable path").required();
const CLASS: ArgSpec = ArgSpec::new(
    "class",
    ArgKind::Str,
    "never (cannot close) | may_end (remove protection)",
)
.required();
static SPEC: ModuleSpec = ModuleSpec::new("permission", Section::Protection, "Programs that cannot be closed")
    .commands(&[
        CommandSpec::new("list", "Show immutable and user allowlist entries").agent().read_only(),
        CommandSpec::new("set", "Add or remove protection (page only)").args(&[PROGRAM, CLASS]),
        CommandSpec::new("request", "Ask the user to change protection; has no effect until approved").agent().args(&[
            PROGRAM, CLASS, ArgSpec::new("reason", ArgKind::Str, "Why the change is requested").required(),
        ]),
        CommandSpec::new("requests", "List pending change requests").agent().read_only(),
        CommandSpec::new("review", "Approve or reject a pending request (page only)").args(&[
            ArgSpec::new("request_id", ArgKind::Int, "Request id").required(),
            ArgSpec::new("approve", ArgKind::Bool, "Approve; false rejects").required(),
        ]),
    ])
    .publishes(&[EventSpec::new("changed", "The allowlist changed"), EventSpec::new("requested", "User approval requested")])
    .subscribes(&["core.started", "task.snapshot"])
    .migrations(&[Migration::new(1, "
        CREATE TABLE permission_rules (path TEXT PRIMARY KEY, signer TEXT, source TEXT NOT NULL, enabled INTEGER NOT NULL);
        CREATE TABLE permission_requests (id INTEGER PRIMARY KEY, path TEXT NOT NULL, class TEXT NOT NULL,
            reason TEXT NOT NULL, requested_by TEXT NOT NULL, created_at INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending', reviewed_by TEXT, reviewed_at INTEGER);
    ")]);

pub fn create() -> Box<dyn Module> {
    Box::new(Permission)
}
struct Permission;

pub use crate::sys::programs::{blocked, normalize};

fn class(value: &str) -> Result<bool, ApiError> {
    match value {
        "never" => Ok(true),
        "may_end" => Ok(false),
        _ => Err(ApiError::invalid_args(
            "class must be never or may_end; the list only records programs that cannot be closed",
        )),
    }
}

impl Permission {
    fn refresh(&self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        let discovered = ctx.system().program_rules()?;
        let mut a = ctx.arbitration();
        a.permission_ready = false;
        ctx.db()
            .execute("DELETE FROM permission_rules WHERE source='builtin'", &[])?;
        for rule in discovered {
            let Ok(path) = normalize(&rule.path) else {
                continue;
            };
            if rule.source == "builtin" {
                ctx.db().execute(
                    "INSERT INTO permission_rules(path,signer,source,enabled) VALUES(?,?,?,1)
                    ON CONFLICT(path) DO UPDATE SET source='builtin', signer=excluded.signer, enabled=1",
                    &[path.into(), rule.signer.into(), "builtin".into()],
                )?;
            } else {
                ctx.db().execute(
                    "INSERT OR IGNORE INTO permission_rules(path,signer,source,enabled) VALUES(?,?,?,1)",
                    &[path.into(), rule.signer.into(), "default".into()],
                )?;
            }
        }
        a.rules = self.rules(ctx)?;
        a.excluded_defaults = ctx
            .db()
            .query("SELECT path FROM permission_rules WHERE enabled=0", &[])?
            .iter()
            .map(|r| r.text(0).map_err(db_err))
            .collect::<Result<_, _>>()?;
        a.permission_ready = true;
        Ok(())
    }
    fn rules(&self, ctx: &ModuleCtx) -> Result<Vec<ProgramRule>, ApiError> {
        ctx.db()
            .query(
                "SELECT path,signer,source FROM permission_rules WHERE enabled=1 ORDER BY path",
                &[],
            )?
            .into_iter()
            .map(|r| {
                Ok(ProgramRule {
                    path: r.text(0).map_err(db_err)?,
                    signer: r.opt_text(1).map_err(db_err)?,
                    source: r.text(2).map_err(db_err)?,
                })
            })
            .collect()
    }
    fn change(&self, ctx: &ModuleCtx, program: &str, enabled: bool) -> Result<(), ApiError> {
        self.refresh(ctx)?;
        let path = normalize(program)?;
        let mut a = ctx.arbitration();
        if !enabled
            && a.rules
                .iter()
                .any(|r| r.path == path && r.source == "builtin")
        {
            return Err(ApiError::new(
                ErrorCode::Denied,
                "built-in system, security and own-program protection cannot be removed",
            ));
        }
        let signer = if enabled {
            ctx.system().file_signer(program)?
        } else {
            None
        };
        ctx.db().execute(
            "INSERT INTO permission_rules(path,signer,source,enabled) VALUES(?,?,'user',?)
            ON CONFLICT(path) DO UPDATE SET enabled=excluded.enabled, signer=CASE WHEN excluded.enabled=1 THEN excluded.signer ELSE permission_rules.signer END",
            &[path.into(), signer.into(), i64::from(enabled).into()],
        )?;
        a.rules = self.rules(ctx)?;
        if enabled {
            a.excluded_defaults.remove(&normalize(program)?);
        } else {
            a.excluded_defaults.insert(normalize(program)?);
        }
        drop(a);
        ctx.publish("changed", json!({"program": program, "protected": enabled}))
    }
    fn list(&self, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        let entries: Vec<Value> = ctx
            .db()
            .query("SELECT path,signer,source,enabled FROM permission_rules ORDER BY path", &[])?
            .iter()
            .map(|r| {
                Ok(json!({"program":r.text(0).map_err(db_err)?, "signer":r.opt_text(1).map_err(db_err)?,
                "source":r.text(2).map_err(db_err)?, "protected":r.int(3).map_err(db_err)? != 0,
                "immutable":r.text(2).map_err(db_err)? == "builtin"}))
            })
            .collect::<Result<_, ApiError>>()?;
        let a = ctx.arbitration();
        Ok(json!({"entries":entries, "protected_pids":a.protected, "tasks":a.tasks}))
    }
}
fn db_err(e: crate::sqlite::Error) -> ApiError {
    ApiError::internal(format!("permission store: {e}"))
}

impl Module for Permission {
    fn spec(&self) -> &'static ModuleSpec {
        &SPEC
    }
    fn start(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        self.refresh(ctx)
    }
    fn stop(&mut self, ctx: &ModuleCtx) -> Result<(), ApiError> {
        ctx.arbitration().permission_ready = false;
        Ok(())
    }
    fn handle(&mut self, req: &ModuleRequest<'_>, ctx: &ModuleCtx) -> Result<Value, ApiError> {
        match req.cmd {
            "list" => {
                self.refresh(ctx)?;
                self.list(ctx)
            }
            "set" => {
                self.change(
                    ctx,
                    req.args["program"].as_str().unwrap(),
                    class(req.args["class"].as_str().unwrap())?,
                )?;
                self.list(ctx)
            }
            "request" => {
                let path = normalize(req.args["program"].as_str().unwrap())?;
                class(req.args["class"].as_str().unwrap())?;
                let reason = req.args["reason"].as_str().unwrap();
                if reason.trim().is_empty() || reason.len() > 4096 {
                    return Err(ApiError::invalid_args("reason must contain 1..=4096 bytes"));
                }
                let id = ctx.db().transaction(|c| {
                    c.execute(
                        "INSERT INTO permission_requests(path,class,reason,requested_by,created_at) VALUES(?,?,?,?,?)",
                        &[
                            path.clone().into(),
                            req.args["class"].as_str().unwrap().into(),
                            reason.into(),
                            serde_json::to_string(req.caller).unwrap().into(),
                            ctx.system().now_ms().into(),
                        ],
                    )?;
                    c.query("SELECT last_insert_rowid()", &[])?[0].int(0)
                })?;
                ctx.publish("requested", json!({"request_id":id,"program":path}))?;
                Ok(json!({"request_id":id,"status":"pending","effective":false}))
            }
            "requests" => {
                let requests = ctx.db().query("SELECT id,path,class,reason,requested_by,created_at FROM permission_requests WHERE status='pending' ORDER BY id", &[])?
                    .iter().map(|r| Ok(json!({"request_id":r.int(0).map_err(db_err)?,"program":r.text(1).map_err(db_err)?,
                        "class":r.text(2).map_err(db_err)?,"reason":r.text(3).map_err(db_err)?,
                        "requested_by":serde_json::from_str::<Value>(&r.text(4).map_err(db_err)?).unwrap_or(Value::Null),"created_at":r.int(5).map_err(db_err)?})))
                    .collect::<Result<Vec<_>,ApiError>>()?;
                Ok(json!({"requests":requests}))
            }
            "review" => {
                let id = req.args["request_id"].as_i64().unwrap();
                let rows = ctx.db().query(
                    "SELECT path,class FROM permission_requests WHERE id=? AND status='pending'",
                    &[id.into()],
                )?;
                let r = rows.first().ok_or_else(|| {
                    ApiError::new(ErrorCode::NotFound, "no pending request with that id")
                })?;
                let approve = req.args["approve"].as_bool().unwrap();
                if approve {
                    self.change(
                        ctx,
                        &r.text(0).map_err(db_err)?,
                        class(&r.text(1).map_err(db_err)?)?,
                    )?;
                }
                let status = if approve { "approved" } else { "rejected" };
                ctx.db().execute(
                    "UPDATE permission_requests SET status=?,reviewed_by=?,reviewed_at=? WHERE id=?",
                    &[
                        status.into(),
                        serde_json::to_string(req.caller).unwrap().into(),
                        ctx.system().now_ms().into(),
                        id.into(),
                    ],
                )?;
                Ok(json!({"request_id":id,"status":status}))
            }
            _ => Err(ApiError::internal("unrouted permission command")),
        }
    }
    fn status(&self, ctx: &ModuleCtx) -> Value {
        json!({"implemented":true,"allowlist":self.list(ctx).unwrap_or(Value::Null)})
    }
    fn on_event(&mut self, e: &Event, ctx: &ModuleCtx) -> Result<(), ApiError> {
        if e.topic == "core.started" {
            self.refresh(ctx)?;
        }
        Ok(())
    }
    fn recover(&mut self, _: &JournalEntry, _: &ModuleCtx) -> Result<RecoverOutcome, ApiError> {
        Ok(RecoverOutcome::Restored)
    }
}
