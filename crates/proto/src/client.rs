//! Protocol client over any byte stream. The platform layer opens the stream (named pipe on
//! Windows) and hands it here; everything protocol-related happens in this file.

use std::io::{BufRead, BufReader, Read, Write};

use serde_json::{Value, json};

use crate::catalog::HelloResult;
use crate::commands;
use crate::error::{ApiError, ErrorCode, subcode};
use crate::protocol::{self, ClientInfo, MAX_LINE_BYTES, Request, Response};

/// Longest reply the client accepts.
const MAX_REPLY_BYTES: usize = 16 * MAX_LINE_BYTES;

/// A connected byte stream to the daemon.
pub trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

pub struct Client {
    wire: Wire,
    hello: HelloResult,
}

/// The line-level exchange, shared by the handshake and later calls.
struct Wire {
    conn: BufReader<Box<dyn Stream>>,
    info: ClientInfo,
    id_prefix: String,
    next_id: u64,
}

fn ipc(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::IpcError, message)
}

impl Client {
    /// Runs the handshake on a fresh stream, offering every version this build speaks.
    pub fn handshake(stream: Box<dyn Stream>, info: ClientInfo) -> Result<Client, ApiError> {
        Client::handshake_with(stream, info, &protocol::supported_versions())
    }

    /// Runs the handshake offering exactly `protocols` (tests use this to play old/new clients).
    pub fn handshake_with(stream: Box<dyn Stream>, info: ClientInfo, protocols: &[u32]) -> Result<Client, ApiError> {
        let id_prefix = format!("{}-{}", info.name, std::process::id());
        let mut wire = Wire { conn: BufReader::new(stream), info, id_prefix, next_id: 1 };
        let offered_max = protocols.iter().copied().max().unwrap_or(protocol::PROTOCOL_VERSION);
        let args = json!({ "protocols": protocols, "client": wire.info });
        let resp = wire.exchange(offered_max, commands::HELLO, args, true)?;
        let result = match resp.into_result() {
            Ok(v) => v,
            Err(e) if e.code == ErrorCode::UnknownCommand => {
                return Err(ApiError::new(
                    ErrorCode::ProtocolVersion,
                    "the daemon does not know the handshake (core.hello): it is older than this client; update the daemon",
                )
                .with_subcode(subcode::DAEMON_TOO_OLD));
            }
            Err(e) => return Err(e),
        };
        let hello: HelloResult =
            serde_json::from_value(result).map_err(|e| ipc(format!("handshake reply is malformed: {e}")))?;
        if !protocols.contains(&hello.protocol) {
            return Err(ApiError::new(
                ErrorCode::ProtocolVersion,
                format!("daemon chose protocol {} which this client did not offer ({protocols:?})", hello.protocol),
            ));
        }
        Ok(Client { wire, hello })
    }

    /// What the daemon said in the handshake: version, caller kind, modules, commands.
    pub fn hello(&self) -> &HelloResult {
        &self.hello
    }

    /// Sends one request and waits for its response. `Err` means the exchange itself failed
    /// (`IPC_ERROR`, `PROTOCOL_VERSION`); errors reported by the daemon come back as a
    /// response with `ok == false`.
    pub fn call(&mut self, cmd: &str, args: Value) -> Result<Response, ApiError> {
        self.wire.exchange(self.hello.protocol, cmd, args, false)
    }
}

impl Wire {
    fn exchange(&mut self, v: u32, cmd: &str, args: Value, handshake: bool) -> Result<Response, ApiError> {
        let id = format!("{}-{}", self.id_prefix, self.next_id);
        self.next_id += 1;
        let mut req = Request::new(id.clone(), cmd, args);
        req.v = v;
        req.client = Some(self.info.clone());

        let line = protocol::to_line(&req);
        let stream = self.conn.get_mut();
        stream
            .write_all(line.as_bytes())
            .and_then(|()| stream.flush())
            .map_err(|e| ipc(format!("cannot send request: {e}")))?;

        let mut buf = Vec::new();
        let n = (&mut self.conn)
            .take(MAX_REPLY_BYTES as u64 + 1)
            .read_until(b'\n', &mut buf)
            .map_err(|e| ipc(format!("cannot read reply: {e}")))?;
        if n == 0 {
            return Err(ipc("daemon closed the connection without replying"));
        }
        if !buf.ends_with(b"\n") && buf.len() > MAX_REPLY_BYTES {
            return Err(ipc("reply is too long"));
        }
        let resp: Response = serde_json::from_slice(buf.trim_ascii_end())
            .map_err(|e| ipc(format!("reply is not a protocol response: {e}")))?;
        // After the handshake every reply must use the negotiated version.
        if !handshake && resp.v != v {
            return Err(ApiError::new(
                ErrorCode::ProtocolVersion,
                format!("daemon replied with protocol {} on a connection that negotiated {v}", resp.v),
            ));
        }
        match resp.id.as_deref() {
            Some(got) if got == id => Ok(resp),
            // The daemon could not read our id; only possible for a failed response.
            None if !resp.ok => Ok(resp),
            other => Err(ipc(format!("reply id {other:?} does not match request id {id:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A stream that records what the client writes and replays canned reply lines.
    struct Scripted {
        replies: Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.replies.read(buf)
        }
    }

    impl Write for Scripted {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn scripted(replies: &[&str]) -> Box<dyn Stream> {
        let mut bytes = Vec::new();
        for r in replies {
            bytes.extend_from_slice(r.as_bytes());
            bytes.push(b'\n');
        }
        Box::new(Scripted { replies: Cursor::new(bytes), written: Vec::new() })
    }

    fn info() -> ClientInfo {
        ClientInfo { name: "t".into(), version: None }
    }

    fn hello_reply(id: &str, protocol: u32) -> String {
        json!({"v": protocol, "id": id, "ok": true, "result": {
            "protocol": protocol,
            "daemon": {"product": "p", "version": "9", "pid": 1, "min_protocol": 1, "max_protocol": 1, "new_field": true},
            "caller": {"kind": "agent"},
            "commands": [{"name": "x.y", "module": "x", "verb": "y", "agent": true, "callable": true}],
            "something_added_later": {}
        }})
        .to_string()
    }

    #[test]
    fn handshake_reads_catalog_and_ignores_unknown_fields() {
        let id = format!("t-{}-1", std::process::id());
        let c = Client::handshake(scripted(&[&hello_reply(&id, 1)]), info()).unwrap();
        assert_eq!(c.hello().protocol, 1);
        assert!(c.hello().command("x.y").is_some());
    }

    #[test]
    fn daemon_without_handshake_is_reported_as_too_old() {
        let id = format!("t-{}-1", std::process::id());
        let old = json!({"v": 1, "id": id, "ok": false, "error": {"code": "UNKNOWN_COMMAND", "message": "?"}});
        let err = Client::handshake(scripted(&[&old.to_string()]), info()).err().unwrap();
        assert_eq!(err.code, ErrorCode::ProtocolVersion);
        assert_eq!(err.subcode.as_deref(), Some(subcode::DAEMON_TOO_OLD));
    }

    #[test]
    fn daemon_version_errors_pass_through() {
        let id = format!("t-{}-1", std::process::id());
        let newer = json!({"v": 3, "id": id, "ok": false, "error": {
            "code": "PROTOCOL_VERSION", "subcode": "core.CLIENT_TOO_OLD", "message": "daemon speaks 3..3"}});
        let err = Client::handshake(scripted(&[&newer.to_string()]), info()).err().unwrap();
        assert_eq!(err.code, ErrorCode::ProtocolVersion);
        assert_eq!(err.subcode.as_deref(), Some(subcode::CLIENT_TOO_OLD));
    }

    #[test]
    fn closed_stream_is_an_ipc_error() {
        let err = Client::handshake(scripted(&[]), info()).err().unwrap();
        assert_eq!(err.code, ErrorCode::IpcError);
    }
}
