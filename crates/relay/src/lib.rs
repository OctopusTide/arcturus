//! No request/response bodies, cookies or credentials are written to disk or logs.
//! Each listener has one fixed upstream and accepts only loopback connections.
use bytes::Bytes;
use http_body::{Body, Frame};
use http_body_util::{BodyExt, Full, Limited, combinators::UnsyncBoxBody};
use hyper::service::service_fn;
use hyper::{HeaderMap, Method, Request, Response, StatusCode, body::Incoming};
use hyper_util::rt::TokioIo;
use reqwest::{Client, Url};
use std::collections::BTreeSet;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

type ResponseBody = UnsyncBoxBody<Bytes, io::Error>;
#[derive(Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub upstream: Url,
    pub attempts: u32,
    pub max_buffer_bytes: usize,
    pub max_request_bytes: usize,
    pub heartbeat: Duration,
    pub retry_delay: Duration,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub proxy: Option<String>,
}
impl Config {
    pub fn new(listen: SocketAddr, upstream: &str) -> Result<Self, String> {
        Ok(Self {
            listen,
            upstream: Url::parse(upstream).map_err(|_| "invalid upstream URL")?,
            attempts: 3,
            max_buffer_bytes: 64 * 1024 * 1024,
            max_request_bytes: 64 * 1024 * 1024,
            heartbeat: Duration::from_secs(5),
            retry_delay: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(120),
            proxy: None,
        })
    }
    pub fn validate(&self) -> Result<(), String> {
        if !self.listen.ip().is_loopback() {
            return Err("relay must bind a literal loopback address".into());
        }
        let local = self
            .upstream
            .host_str()
            .and_then(|h| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        if (self.upstream.scheme() != "https" && !(self.upstream.scheme() == "http" && local))
            || !self.upstream.username().is_empty()
            || self.upstream.password().is_some()
            || self.upstream.query().is_some()
            || self.upstream.fragment().is_some()
        {
            return Err(
                "upstream must be HTTPS (HTTP only for literal loopback tests), without credentials/query/fragment"
                    .into(),
            );
        }
        if !(1..=3).contains(&self.attempts)
            || self.max_buffer_bytes == 0
            || self.max_buffer_bytes > 256 * 1024 * 1024
            || self.max_request_bytes == 0
            || self.max_request_bytes > 256 * 1024 * 1024
            || self.heartbeat.is_zero()
        {
            return Err("invalid retry, memory or heartbeat limits".into());
        }
        if let Some(proxy) = self.proxy.as_deref().filter(|p| *p != "direct") {
            let proxy = Url::parse(proxy).map_err(|_| "invalid proxy URL")?;
            if !matches!(proxy.scheme(), "http" | "https")
                || proxy.host_str().is_none()
                || !proxy.username().is_empty()
                || proxy.password().is_some()
            {
                return Err(
                    "explicit proxy must be HTTP(S) without credentials; authenticated proxies may use the environment"
                        .into(),
                );
            }
        }
        Ok(())
    }
}
struct State {
    config: Config,
    client: Client,
    buffer: Arc<AtomicBool>,
    requests: AtomicU64,
    retries: AtomicU64,
    slots: Arc<Semaphore>,
}
#[derive(Clone)]
pub struct Relay(Arc<State>);
#[derive(Clone)]
struct Prepared {
    method: Method,
    url: Url,
    headers: HeaderMap,
    body: Bytes,
}
struct Buffered {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}
#[derive(Debug)]
enum Failure {
    Transport,
    Limit,
    ClientClosed,
}
struct ChannelBody(mpsc::Receiver<Result<Bytes, io::Error>>);
impl Body for ChannelBody {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.0.poll_recv(cx).map(|item| item.map(|r| r.map(Frame::data)))
    }
}
fn full(body: impl Into<Bytes>) -> ResponseBody {
    Full::new(body.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}
fn error(status: StatusCode, message: &str) -> Response<ResponseBody> {
    let transport = message.starts_with("Network connection error:");
    let builder = Response::builder()
        .status(status)
        .header("content-type", "application/json");
    // The relay has already consumed its bounded attempts. Hand off to the failure hook.
    let builder = if transport {
        builder.header("x-should-retry", "false")
    } else {
        builder
    };
    builder
        .body(full(
            serde_json::json!({"type":"error","error":{
                "type":if transport {"api_error"} else {"relay_error"},"message":message
            }})
            .to_string(),
        ))
        .unwrap()
}
fn headers(input: &HeaderMap) -> HeaderMap {
    let mut hop: BTreeSet<String> = [
        "connection",
        "proxy-connection",
        "keep-alive",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
        "proxy-authorization",
        "host",
        "content-length",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for value in input.get_all("connection") {
        if let Ok(value) = value.to_str() {
            hop.extend(value.split(',').map(|v| v.trim().to_ascii_lowercase()));
        }
    }
    input
        .iter()
        .filter(|(name, _)| !hop.contains(name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}
fn response(status: StatusCode, headers: HeaderMap, body: ResponseBody) -> Response<ResponseBody> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}
fn sse(headers: &HeaderMap) -> bool {
    headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("text/event-stream"))
        && headers.get("content-encoding").is_none_or(|v| v == "identity")
}
fn sse_finished(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let terminal = |kind: &str| {
        matches!(
            kind,
            "message_stop" | "response.completed" | "response.failed" | "response.incomplete" | "error"
        )
    };
    let text = text.replace("\r\n", "\n");
    for event in text.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data == "[DONE]" {
            return true;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) {
            if value["type"].as_str().is_some_and(terminal)
                || event
                    .lines()
                    .filter_map(|l| l.strip_prefix("event:"))
                    .any(|kind| terminal(kind.trim()))
            {
                return true;
            }
        }
    }
    false
}
impl Relay {
    pub fn new(config: Config, buffer: Arc<AtomicBool>) -> Result<Self, String> {
        config.validate()?;
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout);
        if let Some(proxy) = &config.proxy {
            builder = if proxy == "direct" {
                builder.no_proxy()
            } else {
                builder.proxy(reqwest::Proxy::all(proxy).map_err(|_| "invalid proxy")?)
            };
        }
        let client = builder
            .build()
            .map_err(|_| "cannot initialize TLS/proxy client")?;
        Ok(Self(Arc::new(State {
            config,
            client,
            buffer,
            requests: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            slots: Arc::new(Semaphore::new(32)),
        })))
    }
    pub async fn serve(
        &self,
        listener: TcpListener,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> io::Result<()> {
        if !listener.local_addr()?.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "non-loopback relay listener",
            ));
        }
        tokio::pin!(shutdown);
        loop {
            tokio::select! {_=&mut shutdown=>return Ok(()),accepted=listener.accept()=>{
                let (stream,peer)=accepted?;if !peer.ip().is_loopback(){continue;}
                let relay=self.clone();tokio::spawn(async move {let service=service_fn(move |request|{let relay=relay.clone();async move {Ok::<_,Infallible>(relay.handle(request).await)}});
                    let _=hyper::server::conn::http1::Builder::new().max_buf_size(64*1024).serve_connection(TokioIo::new(stream),service).await;});
            }}
        }
    }
    async fn send(&self, p: &Prepared) -> Result<reqwest::Response, Failure> {
        self.0
            .client
            .request(p.method.clone(), p.url.clone())
            .headers(p.headers.clone())
            .body(p.body.clone())
            .send()
            .await
            .map_err(|_| Failure::Transport)
    }
    async fn retry_pause(&self, attempt: u32) {
        self.0.retries.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(self.0.config.retry_delay.saturating_mul(attempt)).await;
    }
    async fn first_response(&self, p: &Prepared) -> Result<(reqwest::Response, u32), Failure> {
        for attempt in 1..=self.0.config.attempts {
            match self.send(p).await {
                Ok(response) => return Ok((response, attempt)),
                Err(_) if retryable(p) && attempt < self.0.config.attempts => self.retry_pause(attempt).await,
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }
    async fn buffer_all(
        &self,
        p: &Prepared,
        mut upstream: reqwest::Response,
        mut attempt: u32,
        client: Option<&mpsc::Sender<Result<Bytes, io::Error>>>,
    ) -> Result<Buffered, Failure> {
        loop {
            let status = upstream.status();
            let headers = headers(upstream.headers());
            let mut bytes = Vec::new();
            let complete = loop {
                let next = if let Some(client) = client {
                    tokio::select! {_ = client.closed()=>return Err(Failure::ClientClosed),chunk=upstream.chunk()=>chunk}
                } else {
                    upstream.chunk().await
                };
                match next {
                    Ok(Some(chunk)) => {
                        if bytes.len().saturating_add(chunk.len()) > self.0.config.max_buffer_bytes {
                            return Err(Failure::Limit);
                        }
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok(None) => break true,
                    Err(_) => break false,
                }
            };
            if complete && (!status.is_success() || !sse(&headers) || sse_finished(&bytes)) {
                return Ok(Buffered {
                    status,
                    headers,
                    body: bytes.into(),
                });
            }
            if !retryable(p) || attempt >= self.0.config.attempts || !status.is_success() {
                return Err(Failure::Transport);
            }
            if let Some(client) = client {
                tokio::select! {_=client.closed()=>return Err(Failure::ClientClosed),_=self.retry_pause(attempt)=>{}}
            } else {
                self.retry_pause(attempt).await;
            }
            attempt += 1;
            loop {
                let sent = if let Some(client) = client {
                    tokio::select! {_=client.closed()=>return Err(Failure::ClientClosed),sent=self.send(p)=>sent}
                } else {
                    self.send(p).await
                };
                match sent {
                    Ok(response) => {
                        upstream = response;
                        break;
                    }
                    Err(_) if retryable(p) && attempt < self.0.config.attempts => {
                        if let Some(client) = client {
                            tokio::select! {_=client.closed()=>return Err(Failure::ClientClosed),_=self.retry_pause(attempt)=>{}}
                        } else {
                            self.retry_pause(attempt).await;
                        }
                        attempt += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }
    async fn handle(&self, request: Request<Incoming>) -> Response<ResponseBody> {
        if request.uri().path() == "/_arcturus/health" && request.method() == Method::GET {
            return response(StatusCode::OK,HeaderMap::new(),full(serde_json::json!({"ok":true,"buffering":self.0.buffer.load(Ordering::SeqCst),"requests":self.0.requests.load(Ordering::Relaxed),"retries":self.0.retries.load(Ordering::Relaxed),"worker_pid":std::process::id()}).to_string()));
        }
        if request.uri().authority().is_some()
            || request.uri().scheme().is_some()
            || request.method() == Method::CONNECT
        {
            return error(
                StatusCode::BAD_REQUEST,
                "absolute targets and CONNECT are not supported",
            );
        }
        let permit = match self.0.slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return error(StatusCode::SERVICE_UNAVAILABLE, "relay concurrency limit reached");
            }
        };
        let target = format!(
            "{}{}",
            self.0.config.upstream.as_str().trim_end_matches('/'),
            request.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/")
        );
        let url = match Url::parse(&target) {
            Ok(url) if url.origin() == self.0.config.upstream.origin() => url,
            _ => return error(StatusCode::BAD_REQUEST, "invalid target"),
        };
        let method = request.method().clone();
        let mut request_headers = headers(request.headers());
        // SSE keepalives cannot be mixed into compressed bytes; request identity encoding.
        request_headers.remove("accept-encoding");
        let body = match Limited::new(request.into_body(), self.0.config.max_request_bytes)
            .collect()
            .await
        {
            Ok(body) => body.to_bytes(),
            Err(_) => {
                return error(StatusCode::PAYLOAD_TOO_LARGE, "request exceeds memory limit");
            }
        };
        let prepared = Prepared {
            method,
            url,
            headers: request_headers,
            body,
        };
        self.0.requests.fetch_add(1, Ordering::Relaxed);
        let (upstream, attempt) = match self.first_response(&prepared).await {
            Ok(response) => response,
            Err(_) => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Network connection error: upstream unavailable after bounded retries",
                );
            }
        };
        if self.0.buffer.load(Ordering::SeqCst) && upstream.status().is_success() {
            if sse(upstream.headers()) {
                return self.buffered_sse(prepared, upstream, attempt, permit);
            }
            match self.buffer_all(&prepared, upstream, attempt, None).await {
                Ok(body) => response(body.status, body.headers, full(body.body)),
                Err(Failure::Limit) => error(StatusCode::BAD_GATEWAY, "buffered reply exceeds memory limit"),
                Err(_) => error(
                    StatusCode::BAD_GATEWAY,
                    "Network connection error: upstream reply incomplete after bounded retries",
                ),
            }
        } else {
            self.streaming(prepared, upstream, attempt, permit).await
        }
    }
    async fn streaming(
        &self,
        p: Prepared,
        mut upstream: reqwest::Response,
        mut attempt: u32,
        permit: OwnedSemaphorePermit,
    ) -> Response<ResponseBody> {
        // No model bytes are exposed until the first read succeeds. After that, failures
        // terminate the downstream body and are never replayed in normal mode.
        let first = loop {
            match upstream.chunk().await {
                Ok(chunk) => break chunk,
                Err(_)
                    if retryable(&p)
                        && attempt < self.0.config.attempts
                        && upstream.status().is_success() =>
                {
                    self.retry_pause(attempt).await;
                    attempt += 1;
                    loop {
                        match self.send(&p).await {
                            Ok(response) => {
                                upstream = response;
                                break;
                            }
                            Err(_) if attempt < self.0.config.attempts => {
                                self.retry_pause(attempt).await;
                                attempt += 1;
                            }
                            Err(_) => {
                                return error(
                                    StatusCode::BAD_GATEWAY,
                                    "Network connection error: upstream unavailable before first byte",
                                );
                            }
                        }
                    }
                }
                Err(_) => {
                    return error(
                        StatusCode::BAD_GATEWAY,
                        "Network connection error: upstream failed before first byte",
                    );
                }
            }
        };
        let status = upstream.status();
        let headers = headers(upstream.headers());
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            let _permit = permit;
            if let Some(chunk) = first {
                if tx.send(Ok(chunk)).await.is_err() {
                    return;
                }
            }
            loop {
                let next = tokio::select! {_ = tx.closed()=>return,chunk=upstream.chunk()=>chunk};
                match next {
                    Ok(Some(chunk)) => {
                        if tx.send(Ok(chunk)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(_) => {
                        let _ = tx
                            .send(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "upstream stream interrupted",
                            )))
                            .await;
                        return;
                    }
                }
            }
        });
        response(status, headers, ChannelBody(rx).boxed_unsync())
    }
    fn buffered_sse(
        &self,
        p: Prepared,
        upstream: reqwest::Response,
        attempt: u32,
        permit: OwnedSemaphorePermit,
    ) -> Response<ResponseBody> {
        let status = upstream.status();
        let mut reply_headers = headers(upstream.headers());
        reply_headers.insert("cache-control", "no-cache".parse().unwrap());
        reply_headers.insert("x-accel-buffering", "no".parse().unwrap());
        let (tx, rx) = mpsc::channel(8);
        let relay = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
            let ping = tx.clone();
            let heartbeat = relay.0.config.heartbeat;
            let pinger = tokio::spawn(async move {
                loop {
                    tokio::select! {_=&mut stopped=>break,_=tokio::time::sleep(heartbeat)=>{
                        match ping.try_send(Ok(Bytes::from_static(b": arcturus buffering\n\n"))){Ok(())|Err(mpsc::error::TrySendError::Full(_))=>{},Err(mpsc::error::TrySendError::Closed(_))=>break}
                    }}
                }
            });
            let result = relay.buffer_all(&p, upstream, attempt, Some(&tx)).await;
            let _ = stop.send(());
            let _ = pinger.await;
            match result {
                Ok(buffer) if buffer.status.is_success() && sse(&buffer.headers) => {
                    for chunk in buffer.body.chunks(64 * 1024) {
                        if tx.send(Ok(Bytes::copy_from_slice(chunk))).await.is_err() {
                            return;
                        }
                    }
                }
                Err(Failure::ClientClosed) => {}
                _ => {
                    let _=tx.send(Ok(Bytes::from_static(b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"Network connection error: upstream reply could not be completed\"}}\n\n"))).await;
                }
            }
        });
        response(status, reply_headers, ChannelBody(rx).boxed_unsync())
    }
}

fn retryable(p: &Prepared) -> bool {
    matches!(
        p.method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::POST
    )
}
