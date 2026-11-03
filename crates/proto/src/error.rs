//! The fixed error-code table.
//!
//! Two levels:
//! - `code`: one of the framework codes below. Each has a stable wire name and a stable CLI
//!   exit code. Clients branch on this.
//! - `subcode` (optional): a namespaced detail, `"<module>.<NAME>"`, e.g.
//!   `"task.ALREADY_PROTECTED"`. Each module declares its own; the framework's are `core.*`.
//!
//! Both are append-only: never rename, renumber or reuse a code or subcode.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// CLI exit code for success.
pub const EXIT_OK: i32 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    // Produced on the client side (CLI, page, tray).
    /// Unexpected failure inside the CLI itself.
    CliFailure,
    /// Bad command-line arguments.
    Usage,
    /// Nobody is listening on the pipe.
    DaemonNotRunning,
    /// The pipe broke, timed out, or the reply was not valid protocol.
    IpcError,
    // Produced by either side.
    /// No protocol version both sides speak, or a request used the wrong one.
    ProtocolVersion,
    // Produced by the daemon.
    /// The request line is not a valid protocol request (or came before the handshake).
    BadRequest,
    /// No module handles this command.
    UnknownCommand,
    /// The command exists but its arguments are wrong.
    InvalidArgs,
    /// The module behind this command is still a stub.
    NotImplemented,
    /// The module behind this command is turned off.
    ModuleDisabled,
    /// Policy refused the request (for example an agent calling a page-only command).
    Denied,
    /// The thing the request refers to does not exist.
    NotFound,
    /// The request conflicts with the current state.
    Conflict,
    /// The daemon failed internally.
    Internal,
    /// The module behind this command crashed earlier and is switched off until restart.
    ModuleFaulted,
    // New codes go above this line (with the next free exit code); `Unknown` stays last
    // because serde requires the catch-all variant to be the final one.
    /// A code this build does not know (for example sent by a newer daemon).
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 16] = [
        ErrorCode::CliFailure,
        ErrorCode::Usage,
        ErrorCode::DaemonNotRunning,
        ErrorCode::IpcError,
        ErrorCode::ProtocolVersion,
        ErrorCode::BadRequest,
        ErrorCode::UnknownCommand,
        ErrorCode::InvalidArgs,
        ErrorCode::NotImplemented,
        ErrorCode::ModuleDisabled,
        ErrorCode::Denied,
        ErrorCode::NotFound,
        ErrorCode::Conflict,
        ErrorCode::Internal,
        ErrorCode::Unknown,
        ErrorCode::ModuleFaulted,
    ];

    /// The CLI exit code for this error.
    pub const fn exit_code(self) -> i32 {
        match self {
            ErrorCode::CliFailure => 1,
            ErrorCode::Usage => 2,
            ErrorCode::DaemonNotRunning => 3,
            ErrorCode::IpcError => 4,
            ErrorCode::ProtocolVersion => 5,
            ErrorCode::BadRequest => 10,
            ErrorCode::UnknownCommand => 11,
            ErrorCode::InvalidArgs => 12,
            ErrorCode::NotImplemented => 13,
            ErrorCode::ModuleDisabled => 14,
            ErrorCode::Denied => 15,
            ErrorCode::NotFound => 16,
            ErrorCode::Conflict => 17,
            ErrorCode::Internal => 18,
            ErrorCode::Unknown => 19,
            ErrorCode::ModuleFaulted => 20,
        }
    }

    /// The wire name, identical to the serde form.
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::CliFailure => "CLI_FAILURE",
            ErrorCode::Usage => "USAGE",
            ErrorCode::DaemonNotRunning => "DAEMON_NOT_RUNNING",
            ErrorCode::IpcError => "IPC_ERROR",
            ErrorCode::ProtocolVersion => "PROTOCOL_VERSION",
            ErrorCode::BadRequest => "BAD_REQUEST",
            ErrorCode::UnknownCommand => "UNKNOWN_COMMAND",
            ErrorCode::InvalidArgs => "INVALID_ARGS",
            ErrorCode::NotImplemented => "NOT_IMPLEMENTED",
            ErrorCode::ModuleDisabled => "MODULE_DISABLED",
            ErrorCode::Denied => "DENIED",
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::Conflict => "CONFLICT",
            ErrorCode::Internal => "INTERNAL",
            ErrorCode::Unknown => "UNKNOWN",
            ErrorCode::ModuleFaulted => "MODULE_FAULTED",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Framework subcodes (namespace `core`).
pub mod subcode {
    /// The first request on a connection must be `core.hello`.
    pub const HANDSHAKE_REQUIRED: &str = "core.HANDSHAKE_REQUIRED";
    /// The command is not open to agents (page/tray only).
    pub const AGENT_NOT_ALLOWED: &str = "core.AGENT_NOT_ALLOWED";
    /// The operation needs administrator rights the daemon does not have.
    pub const NEEDS_ELEVATION: &str = "core.NEEDS_ELEVATION";
    /// With `MODULE_DISABLED`: no module was started because the config file is broken.
    pub const CONFIG_INVALID: &str = "core.CONFIG_INVALID";
    /// Client side: the daemon predates the handshake or speaks only older versions.
    pub const DAEMON_TOO_OLD: &str = "core.DAEMON_TOO_OLD";
    /// The daemon speaks only newer protocol versions than the client.
    pub const CLIENT_TOO_OLD: &str = "core.CLIENT_TOO_OLD";
    /// With `NOT_IMPLEMENTED`: this platform or machine does not offer the system call.
    pub const UNSUPPORTED: &str = "core.UNSUPPORTED";
    /// With `DENIED`: the OS refused the daemon's own (normal user) rights, for example on an
    /// elevated process. The administrator layer may still be able to do it.
    pub const ACCESS_DENIED: &str = "core.ACCESS_DENIED";
}

/// The `error` object of a failed response. Daemon modules return this type directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    /// Human-readable, English. Not stable; match on `code` / `subcode`.
    pub message: String,
    /// Namespaced detail code, `"<module>.<NAME>"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subcode: Option<String>,
    /// Module that produced the error, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// Extra machine-readable data, shape defined by whoever sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ApiError { code, message: message.into(), subcode: None, module: None, details: None }
    }

    pub fn with_subcode(mut self, subcode: impl Into<String>) -> Self {
        self.subcode = Some(subcode.into());
        self
    }

    pub fn with_module(mut self, module: impl Into<String>) -> Self {
        self.module = Some(module.into());
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn not_implemented(module: &str, cmd: &str) -> Self {
        ApiError::new(ErrorCode::NotImplemented, format!("'{cmd}' is not implemented yet")).with_module(module)
    }

    pub fn invalid_args(message: impl Into<String>) -> Self {
        ApiError::new(ErrorCode::InvalidArgs, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        ApiError::new(ErrorCode::Internal, message)
    }

    pub fn exit_code(&self) -> i32 {
        self.code.exit_code()
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}", self.code)?;
        if let Some(s) = &self.subcode {
            write!(f, " {s}")?;
        }
        f.write_str("] ")?;
        if let Some(m) = &self.module {
            write!(f, "{m}: ")?;
        }
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn exit_codes_are_unique_and_never_zero() {
        let mut seen = HashSet::new();
        for code in ErrorCode::ALL {
            assert_ne!(code.exit_code(), EXIT_OK, "{code}");
            assert!(seen.insert(code.exit_code()), "duplicate exit code for {code}");
        }
    }

    #[test]
    fn wire_names_match_serde() {
        for code in ErrorCode::ALL {
            let json = serde_json::to_value(code).unwrap();
            assert_eq!(json, Value::String(code.as_str().into()));
            let back: ErrorCode = serde_json::from_value(json).unwrap();
            assert_eq!(back, code);
        }
    }

    #[test]
    fn unknown_wire_code_maps_to_unknown() {
        let code: ErrorCode = serde_json::from_str("\"SOMETHING_NEWER\"").unwrap();
        assert_eq!(code, ErrorCode::Unknown);
        assert_eq!(code.exit_code(), 19);
    }

    #[test]
    fn api_error_omits_empty_fields_and_ignores_unknown_ones() {
        let e = ApiError::new(ErrorCode::NotFound, "no such task");
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"code":"NOT_FOUND","message":"no such task"}"#);
        let newer: ApiError =
            serde_json::from_str(r#"{"code":"CONFLICT","message":"m","subcode":"task.X","added_later":1}"#).unwrap();
        assert_eq!(newer.subcode.as_deref(), Some("task.X"));
    }
}
