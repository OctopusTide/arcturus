//! Turns request lines into response lines. Per request, in this order:
//! handshake check → route → agent permission check → argument check → module (or the
//! daemon's own `core.*` commands) → operation log. Transport-free, so tests drive it directly.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use proto::catalog::{CallerInfo, DaemonHello, HelloArgs, HelloResult, Section, negotiate};
use proto::protocol::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, parse_request};
use proto::{ApiError, ErrorCode, Request, Response, names, subcode};
use serde::Serialize;
use serde_json::{Value, json};

use crate::access::TrustPolicy;
use crate::args;
use crate::dlog;
use crate::module::{ArgKind, ArgSpec, Caller, CommandSpec, EventSpec, ModuleRequest, ModuleSpec};
use crate::registry::{RecoveryReport, Registry, Resolved};
use crate::store::{OpRecord, Store, now_ms};
use crate::sys::System;

const LOG_DEFAULT_LIMIT: u64 = 50;
const LOG_MAX_LIMIT: u64 = 1000;

/// The daemon's own commands and events, described like any module.
pub static CORE_SPEC: ModuleSpec = ModuleSpec::new(
    proto::commands::module::CORE,
    Section::Common,
    "The daemon itself",
)
.commands(&[
    CommandSpec::new(
        "hello",
        "Handshake: agree on a protocol version and list the commands",
    )
    .agent()
    .read_only()
    .hidden()
    .args(&[
        ArgSpec::new(
            "protocols",
            ArgKind::Json,
            "Protocol versions the client speaks",
        )
        .required(),
        ArgSpec::new("client", ArgKind::Json, "Client name and version"),
    ]),
    CommandSpec::new("status", "Daemon and module status")
        .short("status")
        .agent()
        .read_only(),
    CommandSpec::new("log", "Recent requests: who asked for what")
        .short("log")
        .agent()
        .read_only()
        .args(&[ArgSpec::new(
            "limit",
            ArgKind::Uint,
            "Number of entries, 1-1000",
        )]),
])
.publishes(&[
    EventSpec::new("started", "The daemon finished starting"),
    EventSpec::new("module_faulted", "A module crashed and was switched off"),
]);

#[derive(Debug, Clone, Serialize)]
pub struct DaemonInfo {
    pub product: &'static str,
    pub version: &'static str,
    pub pid: u32,
    pub started_at: i64,
    pub endpoint: String,
    pub data_dir: PathBuf,
}

/// Per-connection state.
#[derive(Debug, Clone)]
pub struct Session {
    pub caller: Caller,
    /// Set by a successful `core.hello`.
    pub protocol: Option<u32>,
}

pub struct Dispatcher {
    registry: Registry,
    store: Arc<Store>,
    info: DaemonInfo,
    recovery: RecoveryReport,
    trust: TrustPolicy,
    system: Arc<dyn System>,
}

impl Dispatcher {
    pub fn new(
        registry: Registry,
        store: Arc<Store>,
        info: DaemonInfo,
        recovery: RecoveryReport,
        trust: TrustPolicy,
        system: Arc<dyn System>,
    ) -> Self {
        Dispatcher {
            registry,
            store,
            info,
            recovery,
            trust,
            system,
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn recovery(&self) -> &RecoveryReport {
        &self.recovery
    }

    /// Stops all modules. Called once on shutdown.
    pub fn shutdown(&self) {
        self.registry.stop_all();
    }

    /// A new connection from process `pid` (as reported by the transport).
    pub fn session(&self, pid: Option<u32>) -> Session {
        let exe = pid.and_then(|p| self.system.process_image_path(p));
        self.session_for(Caller {
            pid,
            exe,
            name: None,
            kind: Default::default(),
        })
    }

    /// A session for an already-identified caller; the kind is decided here.
    pub fn session_for(&self, mut caller: Caller) -> Session {
        caller.kind = self.trust.classify(caller.exe.as_deref());
        Session {
            caller,
            protocol: None,
        }
    }

    /// Handles one request line (without its newline).
    pub fn handle_line(&self, session: &mut Session, line: &[u8]) -> Response {
        let started = Instant::now();
        match parse_request(line) {
            Ok(req) => self.handle(session, &req),
            Err((id, err)) => {
                let cmd = serde_json::from_slice::<Value>(line)
                    .ok()
                    .and_then(|v| v.get("cmd").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_default();
                self.log_op(
                    id.as_deref(),
                    &cmd,
                    None,
                    None,
                    &session.caller,
                    Some(&err),
                    started,
                );
                self.reply(session, Response::failure(id, err))
            }
        }
    }

    fn reply(&self, session: &Session, mut resp: Response) -> Response {
        resp.v = session.protocol.unwrap_or(PROTOCOL_VERSION);
        resp
    }

    /// Handles one parsed request.
    pub fn handle(&self, session: &mut Session, req: &Request) -> Response {
        let started = Instant::now();
        if let Some(c) = &req.client {
            session.caller.name = Some(c.name.clone());
        }
        let resolved = self.registry.resolve(&req.cmd);
        let result = self.execute(session, req, resolved);

        let quiet = resolved.is_some_and(|r| r.command.read_only) && result.is_ok();
        if !quiet {
            let name = resolved
                .map(|r| r.full_name())
                .unwrap_or_else(|| req.cmd.clone());
            let module = resolved.map(|r| r.module.name);
            self.log_op(
                Some(&req.id),
                &name,
                module,
                Some(&req.args),
                &session.caller,
                result.as_ref().err(),
                started,
            );
        }
        let resp = match result {
            Ok(v) => Response::success(req.id.clone(), v),
            Err(e) => Response::failure(Some(req.id.clone()), e),
        };
        self.reply(session, resp)
    }

    fn execute(
        &self,
        session: &mut Session,
        req: &Request,
        resolved: Option<Resolved<'_>>,
    ) -> Result<Value, ApiError> {
        let is_hello = resolved.is_some_and(|r| r.slot.is_none() && r.command.name == "hello");
        match session.protocol {
            None if !is_hello => {
                return Err(ApiError::new(
                    ErrorCode::BadRequest,
                    "send core.hello first (handshake)",
                )
                .with_subcode(subcode::HANDSHAKE_REQUIRED));
            }
            Some(v) if !is_hello && req.v != v => {
                return Err(ApiError::new(
                    ErrorCode::ProtocolVersion,
                    format!(
                        "this connection agreed on protocol {v}, request says {}",
                        req.v
                    ),
                ));
            }
            _ => {}
        }
        let Some(r) = resolved else {
            return Err(ApiError::new(
                ErrorCode::UnknownCommand,
                format!("unknown command '{}'", req.cmd),
            ));
        };
        let full = r.full_name();
        if !r.command.agent && session.caller.kind != crate::module::CallerKind::Trusted {
            return Err(ApiError::new(
                ErrorCode::Denied,
                format!("'{full}' is for the page and tray only, not for agents"),
            )
            .with_subcode(subcode::AGENT_NOT_ALLOWED));
        }
        args::validate(&full, r.command, &req.args)?;
        match r.slot {
            None => self.core(session, r.command.name, &req.args),
            Some(slot) => {
                let mreq = ModuleRequest {
                    id: &req.id,
                    cmd: r.command.name,
                    args: &req.args,
                    caller: &session.caller,
                };
                self.registry.call(slot, &mreq)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn log_op(
        &self,
        request_id: Option<&str>,
        cmd: &str,
        module: Option<&str>,
        args: Option<&Value>,
        caller: &Caller,
        error: Option<&ApiError>,
        started: Instant,
    ) {
        let rec = OpRecord {
            id: 0,
            ts: now_ms(),
            request_id: request_id.map(str::to_owned),
            cmd: cmd.to_owned(),
            module: module.map(str::to_owned),
            args: args.cloned().map(|mut value| {
                if cmd == "agents.hook" {
                    if let Some(event) = value.get_mut("event").and_then(Value::as_object_mut) {
                        event.remove("resume_environment");
                    }
                }
                value
            }),
            client_pid: caller.pid.map(i64::from),
            client_exe: caller.exe.clone(),
            client_name: caller.name.clone(),
            caller_kind: Some(caller.kind.as_str().to_owned()),
            ok: error.is_none(),
            error_code: error.map(|e| e.code.as_str().to_owned()),
            subcode: error.and_then(|e| e.subcode.clone()),
            duration_ms: started.elapsed().as_millis() as i64,
        };
        if let Err(e) = self.store.oplog_append(&rec) {
            dlog!("ERROR", "cannot write operation log: {e}");
        }
    }

    fn core(&self, session: &mut Session, verb: &str, args: &Value) -> Result<Value, ApiError> {
        match verb {
            "hello" => self.hello(session, args),
            "status" => Ok(self.status()),
            "log" => self.log(args),
            other => Err(ApiError::internal(format!(
                "core command '{other}' has no handler"
            ))),
        }
    }

    fn hello(&self, session: &mut Session, args: &Value) -> Result<Value, ApiError> {
        let hello: HelloArgs = serde_json::from_value(args.clone())
            .map_err(|e| ApiError::invalid_args(format!("core.hello arguments: {e}")))?;
        let Some(v) = negotiate(MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, &hello.protocols) else {
            let client_max = hello.protocols.iter().copied().max().unwrap_or(0);
            let (sub, advice) = if client_max < MIN_PROTOCOL_VERSION {
                (subcode::CLIENT_TOO_OLD, "update the client")
            } else {
                (subcode::DAEMON_TOO_OLD, "update the daemon")
            };
            return Err(ApiError::new(
                ErrorCode::ProtocolVersion,
                format!(
                    "no common protocol version: daemon speaks {MIN_PROTOCOL_VERSION}..={PROTOCOL_VERSION}, client offered {:?}; {advice}",
                    hello.protocols
                ),
            )
            .with_subcode(sub)
            .with_details(json!({
                "daemon_min": MIN_PROTOCOL_VERSION,
                "daemon_max": PROTOCOL_VERSION,
                "client": hello.protocols,
            })));
        };
        session.protocol = Some(v);
        if let Some(c) = hello.client {
            session.caller.name = Some(c.name);
        }
        let (modules, commands) = self.registry.catalog(session.caller.kind);
        let result = HelloResult {
            protocol: v,
            daemon: DaemonHello {
                product: self.info.product.into(),
                version: self.info.version.into(),
                pid: self.info.pid,
                min_protocol: MIN_PROTOCOL_VERSION,
                max_protocol: PROTOCOL_VERSION,
            },
            caller: CallerInfo {
                kind: session.caller.kind.as_str().into(),
                pid: session.caller.pid,
                exe: session.caller.exe.clone(),
            },
            modules,
            commands,
        };
        serde_json::to_value(result).map_err(|e| ApiError::internal(e.to_string()))
    }

    fn status(&self) -> Value {
        let open = self.store.journal_open(None).map(|e| e.len()).ok();
        json!({
            "daemon": {
                "product": self.info.product,
                "version": self.info.version,
                "protocol": { "min": MIN_PROTOCOL_VERSION, "max": PROTOCOL_VERSION },
                "pid": self.info.pid,
                "started_at": self.info.started_at,
                "uptime_ms": now_ms() - self.info.started_at,
                "endpoint": self.info.endpoint,
                "data_dir": self.info.data_dir,
                "sqlite": self.store.library(),
                "config_warnings": self.registry.config_warnings(),
                "config_error": self.registry.config_error(),
            },
            "modules": self.registry.status(),
            "journal": {
                "open": open,
                "last_recovery": self.recovery,
            },
        })
    }

    fn log(&self, args: &Value) -> Result<Value, ApiError> {
        let limit = match args.get("limit").and_then(Value::as_u64) {
            None => LOG_DEFAULT_LIMIT,
            Some(n) if (1..=LOG_MAX_LIMIT).contains(&n) => n,
            Some(_) => {
                return Err(ApiError::invalid_args(format!(
                    "\"limit\" must be 1..={LOG_MAX_LIMIT}"
                )));
            }
        };
        let entries = self
            .store
            .oplog_recent(limit as u32)
            .map_err(|e| ApiError::internal(format!("store: {e}")))?;
        Ok(json!({ "entries": entries }))
    }
}

/// Static part of [`DaemonInfo`].
pub fn daemon_info(endpoint: &str, data_dir: PathBuf) -> DaemonInfo {
    DaemonInfo {
        product: names::PRODUCT,
        version: env!("CARGO_PKG_VERSION"),
        pid: std::process::id(),
        started_at: now_ms(),
        endpoint: endpoint.to_owned(),
        data_dir,
    }
}
