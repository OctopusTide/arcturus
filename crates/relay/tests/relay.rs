use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
struct Mock {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn mock(responses: Vec<(Vec<u8>, Duration)>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let record = requests.clone();
    let mut responses: VecDeque<_> = responses.into();
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") && request.len() < 65536 {
                if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                    break;
                }
                request.push(byte[0]);
            }
            let text = String::from_utf8_lossy(&request).to_string();
            let length = text
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            let _ = socket.read_exact(&mut body).await;
            record.lock().unwrap().push(text);
            if let Some((response, delay)) = responses.pop_front() {
                let _ = socket.write_all(&response).await;
                let _ = socket.flush().await;
                tokio::time::sleep(delay).await;
            }
        }
    });
    Mock { url, requests, task }
}
fn wire(status: &str, kind: &str, body: &str, complete: bool) -> (Vec<u8>, Duration) {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n{body}\r\n",
        body.len()
    );
    if complete {
        response.push_str("0\r\n\r\n");
    }
    (response.into_bytes(), Duration::from_millis(20))
}
struct Running {
    url: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Running {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}
async fn relay(upstream: &str, away: bool, limit: usize) -> Running {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = relay::Config::new(address, upstream).unwrap();
    config.proxy = Some("direct".into());
    config.heartbeat = Duration::from_millis(5);
    config.retry_delay = Duration::from_millis(5);
    config.read_timeout = Duration::from_secs(2);
    config.max_buffer_bytes = limit;
    let relay = relay::Relay::new(config, Arc::new(AtomicBool::new(away))).unwrap();
    let (stop, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        relay
            .serve(listener, async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    Running {
        url: format!("http://{address}"),
        stop: Some(stop),
        task,
    }
}
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .read_timeout(Duration::from_secs(3))
        .build()
        .unwrap()
}

#[tokio::test]
async fn away_discards_partial_attempts_and_only_delivers_complete_sse() {
    let upstream = mock(vec![
        wire("200 OK", "text/event-stream", "data: discarded-draft\n\n", false),
        wire(
            "200 OK",
            "text/event-stream",
            "data: complete-answer\n\ndata: [DONE]\n\n",
            true,
        ),
    ])
    .await;
    let relay = relay(&upstream.url, true, 65536).await;
    let body = client()
        .post(format!("{}/v1/messages", relay.url))
        .body("{}")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("complete-answer"), "{body}");
    assert!(!body.contains("discarded-draft"));
    assert!(body.contains(": arcturus buffering"));
    assert_eq!(upstream.requests.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn normal_mode_streams_and_never_replays_after_first_model_bytes() {
    let upstream = mock(vec![
        wire("200 OK", "text/event-stream", "data: partial\n\n", false),
        wire("200 OK", "text/event-stream", "data: must-not-replay\n\n", true),
    ])
    .await;
    let relay = relay(&upstream.url, false, 65536).await;
    let mut response = client().post(&relay.url).body("{}").send().await.unwrap();
    let mut body = Vec::new();
    let mut failed = false;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    assert!(failed);
    assert!(String::from_utf8_lossy(&body).contains("partial"));
    assert_eq!(upstream.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn normal_mode_can_retry_before_response_headers_or_bytes() {
    let upstream = mock(vec![
        (Vec::new(), Duration::ZERO),
        wire("200 OK", "application/json", "{\"ok\":true}", true),
    ])
    .await;
    let relay = relay(&upstream.url, false, 65536).await;
    let response = client().post(&relay.url).body("{}").send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "{\"ok\":true}");
    assert_eq!(upstream.requests.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn authentication_errors_are_forwarded_without_retry_and_headers_are_preserved() {
    let upstream = mock(vec![wire(
        "401 Unauthorized",
        "application/json",
        "{\"error\":\"auth\"}",
        true,
    )])
    .await;
    let relay = relay(&upstream.url, true, 65536).await;
    let response = client()
        .post(format!("{}/v1/responses", relay.url))
        .header("authorization", "Bearer test-only-placeholder")
        .header("x-api-key", "test-only-key")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(response.text().await.unwrap().contains("auth"));
    let requests = upstream.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("Bearer test-only-placeholder"));
    assert!(requests[0].contains("x-api-key: test-only-key"));
}
#[tokio::test]
async fn finite_retry_and_buffer_limits_fail_without_leaking_an_incomplete_answer() {
    let upstream = mock(vec![wire(
        "200 OK",
        "application/json",
        "123456789012345678901234567890",
        true,
    )])
    .await;
    let relay = relay(&upstream.url, true, 16).await;
    let response = client().post(&relay.url).body("{}").send().await.unwrap();
    assert_eq!(response.status(), 502);
    assert!(!response.text().await.unwrap().contains("1234567890"));
    assert_eq!(upstream.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn relay_is_not_an_open_proxy_and_never_follows_redirects() {
    let upstream = mock(vec![(
        b"HTTP/1.1 302 Found\r\nLocation: https://example.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec(),
        Duration::ZERO,
    )])
    .await;
    let relay = relay(&upstream.url, false, 65536).await;
    let response = client().post(&relay.url).body("{}").send().await.unwrap();
    assert_eq!(response.status(), 302);
    let mut socket = tokio::net::TcpStream::connect(relay.url.trim_start_matches("http://"))
        .await
        .unwrap();
    socket
        .write_all(
            b"GET http://example.invalid/ HTTP/1.1\r\nHost: example.invalid\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).await.unwrap();
    assert!(response.contains("400 Bad Request"));
    assert_eq!(upstream.requests.lock().unwrap().len(), 1);
}
#[test]
fn unsafe_listeners_upstreams_and_retry_counts_are_rejected() {
    let address = "127.0.0.1:0".parse().unwrap();
    for url in [
        "http://example.com/",
        "https://user:password@example.com/",
        "https://example.com/?api_key=secret",
    ] {
        assert!(relay::Config::new(address, url).unwrap().validate().is_err());
    }
    let mut config = relay::Config::new("0.0.0.0:14599".parse().unwrap(), "https://api.example.com").unwrap();
    assert!(config.validate().is_err());
    config.listen = address;
    config.attempts = 4;
    assert!(config.validate().is_err());
}

#[tokio::test]
async fn repeated_disconnects_stop_at_three_and_never_deliver_drafts() {
    let upstream = mock(
        (0..4)
            .map(|_| wire("200 OK", "text/event-stream", "data: discarded\n\n", false))
            .collect(),
    )
    .await;
    let relay = relay(&upstream.url, true, 65536).await;
    let body = client()
        .post(&relay.url)
        .body("{}")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(upstream.requests.lock().unwrap().len(), 3);
    assert!(body.contains("Network connection error"));
    assert!(!body.contains("data: discarded"));
}

#[tokio::test]
async fn client_disconnect_cancels_buffering_instead_of_starting_another_attempt() {
    let upstream = mock(vec![
        wire("200 OK", "text/event-stream", "data: pending\n\n", false),
        wire("200 OK", "text/event-stream", "data: unused\n\n", true),
    ])
    .await;
    let relay = relay(&upstream.url, true, 65536).await;
    let response = client().post(&relay.url).body("{}").send().await.unwrap();
    drop(response);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(upstream.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn graceful_http_eof_without_an_sse_terminal_event_is_retried() {
    let upstream = mock(vec![
        wire("200 OK", "text/event-stream", "data: silent-draft\n\n", true),
        wire(
            "200 OK",
            "text/event-stream",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            true,
        ),
    ])
    .await;
    let relay = relay(&upstream.url, true, 65536).await;
    let body = client()
        .post(&relay.url)
        .body("{}")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!body.contains("silent-draft"));
    assert!(body.contains("message_stop"));
    assert_eq!(upstream.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn semantic_api_errors_are_terminal_and_are_not_replayed() {
    let upstream = mock(vec![wire(
        "200 OK",
        "text/event-stream",
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}\n\n",
        true,
    )])
    .await;
    let relay = relay(&upstream.url, true, 65536).await;
    let body = client()
        .post(&relay.url)
        .body("{}")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("rate_limit_error"));
    assert_eq!(upstream.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn post_is_replayed_before_any_reply_reaches_the_agent() {
    let upstream = mock(vec![
        (Vec::new(), Duration::ZERO),
        wire("200 OK", "application/json", "{\"duplicated\":true}", true),
    ])
    .await;
    let relay = relay(&upstream.url, false, 65536).await;
    let response = client().post(&relay.url).body("{}").send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(upstream.requests.lock().unwrap().len(), 2);
}
