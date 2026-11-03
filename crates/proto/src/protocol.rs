//! Wire protocol: one JSON object per line (UTF-8, `\n`-terminated) in each direction over
//! any byte stream. The client sends a request line, the daemon answers with exactly one
//! response line carrying the same `id`. The first request on a connection must be the
//! `core.hello` handshake (see [`crate::catalog`]).
//!
//! Compatibility rules (see docs/INTERFACES.md):
//! - fields are only ever added, never removed or repurposed;
//! - unknown fields are ignored by both sides (no `deny_unknown_fields` anywhere);
//! - error codes and subcodes are only ever added;
//! - incompatible changes bump [`PROTOCOL_VERSION`]; the handshake picks the highest version
//!   both sides speak.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ApiError, ErrorCode};

/// Newest protocol version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;
/// Oldest protocol version this build still speaks.
pub const MIN_PROTOCOL_VERSION: u32 = 1;
/// Longest accepted line, excluding the newline. Longer requests get `BAD_REQUEST` and the
/// connection is closed.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
/// Longest accepted request id, in characters.
pub const MAX_ID_CHARS: usize = 128;

/// Every protocol version this build speaks, oldest first.
pub fn supported_versions() -> Vec<u32> {
    (MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Protocol version negotiated by the handshake (ignored on `core.hello` itself).
    pub v: u32,
    /// Chosen by the client, echoed in the response. Non-empty, at most 128 characters.
    pub id: String,
    /// `<module>.<verb>` or a declared short name.
    pub cmd: String,
    /// Always a JSON object; may be omitted.
    #[serde(default = "empty_args")]
    pub args: Value,
    /// Self-description of the caller. Untrusted: the daemon also records the caller's
    /// process id and executable from the OS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    /// The request id; `null` only when the request was too broken to read one.
    pub id: Option<String>,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

fn empty_args() -> Value {
    Value::Object(Default::default())
}

impl Request {
    pub fn new(id: impl Into<String>, cmd: impl Into<String>, args: Value) -> Self {
        Request { v: PROTOCOL_VERSION, id: id.into(), cmd: cmd.into(), args, client: None }
    }
}

impl Response {
    pub fn success(id: impl Into<String>, result: Value) -> Self {
        Response { v: PROTOCOL_VERSION, id: Some(id.into()), ok: true, result: Some(result), error: None }
    }

    pub fn failure(id: Option<String>, error: ApiError) -> Self {
        Response { v: PROTOCOL_VERSION, id, ok: false, result: None, error: Some(error) }
    }

    /// CLI exit code for this response (0 when ok).
    pub fn exit_code(&self) -> i32 {
        match (&self.ok, &self.error) {
            (true, _) => crate::EXIT_OK,
            (false, Some(e)) => e.exit_code(),
            (false, None) => ErrorCode::Unknown.exit_code(),
        }
    }

    pub fn into_result(self) -> Result<Value, ApiError> {
        if self.ok {
            Ok(self.result.unwrap_or(Value::Null))
        } else {
            Err(self
                .error
                .unwrap_or_else(|| ApiError::new(ErrorCode::Unknown, "failed response without an error object")))
        }
    }
}

/// Serializes one message as a protocol line: compact JSON plus `\n`.
pub fn to_line<T: Serialize>(msg: &T) -> String {
    let mut line = serde_json::to_string(msg).expect("protocol messages always serialize");
    line.push('\n');
    line
}

/// Parses one request line (without the newline). Checks the envelope only; the version is
/// checked against the connection's handshake by the daemon. On failure returns the request
/// id, when one could be read, together with the error to send back.
pub fn parse_request(line: &[u8]) -> Result<Request, (Option<String>, ApiError)> {
    let bad = |id: Option<String>, msg: String| (id, ApiError::new(ErrorCode::BadRequest, msg));

    let value: Value =
        serde_json::from_slice(line).map_err(|e| bad(None, format!("request is not valid JSON: {e}")))?;
    let Some(obj) = value.as_object() else {
        return Err(bad(None, "request must be a JSON object".into()));
    };
    let id = match obj.get("id") {
        Some(Value::String(s)) if !s.is_empty() && s.chars().count() <= MAX_ID_CHARS => s.clone(),
        _ => {
            return Err(bad(
                None,
                format!("\"id\" must be a non-empty string of at most {MAX_ID_CHARS} characters"),
            ));
        }
    };
    if obj.get("v").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()).is_none() {
        return Err(bad(Some(id), "\"v\" (protocol version) must be a number".into()));
    }
    let req: Request =
        serde_json::from_value(value).map_err(|e| bad(Some(id.clone()), format!("malformed request: {e}")))?;
    if req.cmd.is_empty() {
        return Err(bad(Some(id), "\"cmd\" must not be empty".into()));
    }
    if !req.args.is_object() {
        return Err(bad(Some(id), "\"args\" must be a JSON object when present".into()));
    }
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn code_of(line: &str) -> (Option<String>, ErrorCode) {
        let (id, e) = parse_request(line.as_bytes()).unwrap_err();
        (id, e.code)
    }

    #[test]
    fn parses_minimal_request() {
        let req = parse_request(br#"{"v":1,"id":"a","cmd":"core.status"}"#).unwrap();
        assert_eq!(req, Request::new("a", "core.status", json!({})));
    }

    #[test]
    fn ignores_unknown_fields() {
        let req = parse_request(
            br#"{"v":1,"id":"a","cmd":"x","args":{"k":1},"client":{"name":"t","extra":2},"future":true}"#,
        )
        .unwrap();
        assert_eq!(req.args, json!({"k": 1}));
        assert_eq!(req.client.unwrap().name, "t");
        let resp: Response = serde_json::from_str(r#"{"v":1,"id":"a","ok":true,"result":1,"added_later":[1]}"#).unwrap();
        assert!(resp.ok);
    }

    #[test]
    fn rejects_broken_requests_with_fixed_codes() {
        assert_eq!(code_of("not json"), (None, ErrorCode::BadRequest));
        assert_eq!(code_of("[1]"), (None, ErrorCode::BadRequest));
        assert_eq!(code_of(r#"{"v":1,"cmd":"x"}"#), (None, ErrorCode::BadRequest));
        assert_eq!(code_of(r#"{"v":1,"id":"","cmd":"x"}"#), (None, ErrorCode::BadRequest));
        assert_eq!(code_of(r#"{"id":"a","cmd":"x"}"#), (Some("a".into()), ErrorCode::BadRequest));
        assert_eq!(code_of(r#"{"v":1,"id":"a"}"#), (Some("a".into()), ErrorCode::BadRequest));
        assert_eq!(code_of(r#"{"v":1,"id":"a","cmd":""}"#), (Some("a".into()), ErrorCode::BadRequest));
        assert_eq!(code_of(r#"{"v":1,"id":"a","cmd":"x","args":[1]}"#), (Some("a".into()), ErrorCode::BadRequest));
    }

    #[test]
    fn response_shape_is_stable() {
        let ok = Response::success("a", json!({"x": 1}));
        assert_eq!(to_line(&ok), "{\"v\":1,\"id\":\"a\",\"ok\":true,\"result\":{\"x\":1}}\n");
        let err = Response::failure(None, ApiError::new(ErrorCode::BadRequest, "nope"));
        assert_eq!(
            to_line(&err),
            "{\"v\":1,\"id\":null,\"ok\":false,\"error\":{\"code\":\"BAD_REQUEST\",\"message\":\"nope\"}}\n"
        );
        assert_eq!(err.exit_code(), 10);
        assert_eq!(ok.exit_code(), 0);
    }
}
