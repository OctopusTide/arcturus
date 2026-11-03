//! The connection layer, behind two traits. The platform crate implements them (named pipe
//! on Windows; a Unix socket would implement the same traits). Everything above the byte
//! stream (framing, sessions, dispatch) is here and shared.

use std::future::Future;
use std::io;
use std::sync::Arc;

use proto::protocol::{MAX_LINE_BYTES, to_line};
use proto::{ApiError, ErrorCode, Response};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::task::JoinSet;

use crate::dispatch::Dispatcher;
use crate::dlog;

/// What the transport knows about the other end, from the OS (not from the client).
#[derive(Debug, Clone, Copy, Default)]
pub struct Peer {
    pub pid: Option<u32>,
}

/// Accepts connections on a claimed endpoint.
pub trait Listener: Send + 'static {
    type Conn: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    /// Waits for the next client. Must be cancel-safe (it is raced against shutdown).
    fn accept(&mut self) -> impl Future<Output = io::Result<(Self::Conn, Peer)>> + Send;
}

#[derive(Debug)]
pub enum BindError {
    /// Another daemon (or someone else) already owns the endpoint.
    AlreadyRunning,
    Io(io::Error),
}

/// Claims endpoints. Implementations must restrict access to the current OS user, refuse
/// remote clients, and refuse to bind an endpoint that is already taken.
pub trait Transport: Send + 'static {
    type Listener: Listener;

    fn bind(&self, endpoint: &str) -> Result<Self::Listener, BindError>;
}

/// Accepts connections until `shutdown` resolves, then closes every connection.
pub async fn serve<L: Listener>(mut listener: L, dispatcher: Arc<Dispatcher>, shutdown: impl Future<Output = ()>) {
    let mut connections = JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => match accepted {
                Ok((conn, peer)) => { connections.spawn(serve_connection(conn, peer, dispatcher.clone())); }
                Err(e) => {
                    dlog!("WARN", "accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            },
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

/// Runs one connection: read a line, dispatch it on a blocking thread, write the reply.
pub async fn serve_connection<C: AsyncRead + AsyncWrite + Unpin>(conn: C, peer: Peer, dispatcher: Arc<Dispatcher>) {
    let mut session = dispatcher.session(peer.pid);
    let mut conn = BufReader::new(conn);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match (&mut conn).take(MAX_LINE_BYTES as u64 + 1).read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if !buf.ends_with(b"\n") && buf.len() > MAX_LINE_BYTES {
            let err = ApiError::new(ErrorCode::BadRequest, format!("request line longer than {MAX_LINE_BYTES} bytes"));
            let _ = write_response(&mut conn, &Response::failure(None, err)).await;
            return;
        }
        let line = buf.trim_ascii().to_vec();
        if line.is_empty() {
            continue;
        }
        let d = dispatcher.clone();
        let mut s = session.clone();
        let (resp, s) = match tokio::task::spawn_blocking(move || {
            let resp = d.handle_line(&mut s, &line);
            (resp, s)
        })
        .await
        {
            Ok(done) => done,
            Err(e) => (Response::failure(None, ApiError::internal(format!("request handler failed: {e}"))), session.clone()),
        };
        session = s;
        if write_response(&mut conn, &resp).await.is_err() {
            return;
        }
    }
}

async fn write_response<C: AsyncRead + AsyncWrite + Unpin>(conn: &mut BufReader<C>, resp: &Response) -> io::Result<()> {
    let stream = conn.get_mut();
    stream.write_all(to_line(resp).as_bytes()).await?;
    stream.flush().await
}
