//! A scripted HTTP server for transport tests.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot, watch};
use tokio::task::JoinHandle;

/// One scripted HTTP response.
#[derive(Debug)]
pub struct Turn {
    status: u16,
    headers: Vec<(String, String)>,
    body: Body,
}

#[derive(Debug)]
enum Body {
    Bytes(Vec<u8>),
    Sse(Vec<Chunk>),
    Stalled,
}

impl Turn {
    /// A streaming response. Each chunk is written as-is; the connection closes
    /// after the last one.
    pub fn sse(chunks: impl IntoIterator<Item = Chunk>) -> Self {
        Self {
            status: 200,
            headers: vec![
                ("content-type".to_owned(), "text/event-stream".to_owned()),
                ("cache-control".to_owned(), "no-cache".to_owned()),
            ],
            body: Body::Sse(chunks.into_iter().collect()),
        }
    }

    /// A JSON response with a `content-length`.
    pub fn json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: Body::Bytes(body.into().into_bytes()),
        }
    }

    /// A plain-text response with a status code.
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
            body: Body::Bytes(body.into().into_bytes()),
        }
    }

    /// A complete response body: status, headers and bytes as they were
    /// received. Cassettes and the recorder replay through this.
    pub(crate) fn raw(
        status: u16,
        headers: Vec<(String, String)>,
        body: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            status,
            headers,
            body: Body::Bytes(body.into()),
        }
    }

    /// Sends the status line and headers, then holds the connection open
    /// without a body until the provider shuts down.
    pub fn stalled() -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".to_owned(), "text/event-stream".to_owned())],
            body: Body::Stalled,
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
}

/// One body chunk inside a streaming [`Turn`].
#[derive(Debug)]
pub struct Chunk {
    bytes: Vec<u8>,
    gate: Option<oneshot::Receiver<()>>,
    delay: Option<Duration>,
}

impl Chunk {
    pub fn text(text: impl Into<String>) -> Self {
        Self::bytes(text.into().into_bytes())
    }

    pub fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
            gate: None,
            delay: None,
        }
    }

    /// Creates a chunk that is not written until the returned sender fires.
    /// Dropping the sender closes the stream at that point instead.
    pub fn gated(text: impl Into<String>) -> (Self, oneshot::Sender<()>) {
        let (sender, receiver) = oneshot::channel();
        (
            Self {
                bytes: text.into().into_bytes(),
                gate: Some(receiver),
                delay: None,
            },
            sender,
        )
    }

    /// Waits before writing this chunk.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }
}

/// A request the client actually sent.
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn body_text(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap_or_default()
    }

    /// Parsed JSON body. Panics if the body is not JSON, which is always a
    /// test bug.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("recorded request body is not JSON")
    }
}

/// Decides what a [`Server`] answers for a request.
pub(crate) trait Responder: Send + Sync + 'static {
    fn respond(&self, request: &RecordedRequest) -> Turn;
}

/// A running HTTP server on a real socket. [`FauxProvider`] scripts it in
/// order; the cassette player answers from recorded interactions.
pub(crate) struct Server {
    url: String,
    state: Arc<ServerState>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

struct ServerState {
    responder: Arc<dyn Responder>,
    requests: Mutex<VecDeque<RecordedRequest>>,
    notify: Notify,
}

/// Requests kept for inspection. A test server lives for one test, but a
/// recorder-style server can outlive many requests, so the list is bounded
/// instead of growing until the process ends.
const MAX_RECORDED_REQUESTS: usize = 256;

impl Server {
    pub(crate) async fn start(responder: Arc<dyn Responder>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("test server: bind an ephemeral port");
        let address = listener.local_addr().expect("test server: local address");
        let state = Arc::new(ServerState {
            responder,
            requests: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        });
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(accept_loop(listener, Arc::clone(&state), receiver));

        Self {
            url: format!("http://{address}"),
            state,
            shutdown,
            task,
        }
    }

    /// Base URL, without a trailing slash and without a protocol path.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    pub(crate) async fn requests(&self) -> Vec<RecordedRequest> {
        self.state
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    pub(crate) async fn request_count(&self) -> usize {
        self.state
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Waits until at least `count` requests have been received. Panics on
    /// timeout so a deadlocked test fails instead of hanging.
    pub(crate) async fn wait_for_requests(&self, count: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let notified = self.state.notify.notified();
            if self.request_count().await >= count {
                return;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "test server: timed out waiting for {count} requests"
            );
            let _ = tokio::time::timeout(remaining.min(Duration::from_millis(100)), notified).await;
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.task.abort();
    }
}

/// A running scripted server.
pub struct FauxProvider {
    server: Server,
}

impl FauxProvider {
    /// Binds an ephemeral port and starts serving `turns` in order.
    pub async fn start(turns: impl IntoIterator<Item = Turn>) -> Self {
        let script = Arc::new(Script {
            turns: Mutex::new(turns.into_iter().collect()),
        });
        Self {
            server: Server::start(script).await,
        }
    }

    /// Base URL, without a trailing slash and without a protocol path.
    pub fn url(&self) -> &str {
        self.server.url()
    }

    pub async fn requests(&self) -> Vec<RecordedRequest> {
        self.server.requests().await
    }

    pub async fn request_count(&self) -> usize {
        self.server.request_count().await
    }

    /// Waits until at least `count` requests have been received. Panics on
    /// timeout so a deadlocked test fails instead of hanging.
    pub async fn wait_for_requests(&self, count: usize) {
        self.server.wait_for_requests(count).await;
    }
}

struct Script {
    turns: Mutex<VecDeque<Turn>>,
}

impl Responder for Script {
    fn respond(&self, _request: &RecordedRequest) -> Turn {
        self.turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .unwrap_or_else(|| Turn::text(500, "faux provider: no scripted turns left"))
    }
}

async fn accept_loop(
    listener: TcpListener,
    state: Arc<ServerState>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => {
                let Ok((socket, _)) = accepted else { break };
                let state = Arc::clone(&state);
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    handle(socket, state, shutdown).await;
                });
            }
        }
    }
}

async fn handle(
    mut socket: TcpStream,
    state: Arc<ServerState>,
    mut shutdown: watch::Receiver<bool>,
) {
    let Ok(request) = read_request(&mut socket).await else {
        return;
    };
    state.record(request.clone()).await;

    let turn = state.responder.respond(&request);
    let _ = write_turn(&mut socket, turn, &mut shutdown).await;
    let _ = socket.shutdown().await;
}

impl ServerState {
    async fn record(&self, request: RecordedRequest) {
        let mut requests = self
            .requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if requests.len() == MAX_RECORDED_REQUESTS {
            requests.pop_front();
        }
        requests.push_back(request);
        drop(requests);
        self.notify.notify_waiters();
    }
}

pub(crate) async fn read_request(socket: &mut TcpStream) -> std::io::Result<RecordedRequest> {
    let mut buffer = Vec::new();
    let mut scratch = [0u8; 4096];

    let head_end = loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        let read = socket.read(&mut scratch).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "request ended before its headers",
            ));
        }
        buffer.extend_from_slice(&scratch[..read]);
        if buffer.len() > 1 << 20 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request headers too large",
            ));
        }
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_owned();
    let path = parts.next().unwrap_or("/").to_owned();
    let version = parts.next().unwrap_or("HTTP/1.1").to_owned();

    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }

    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = buffer[head_end..].to_vec();
    while body.len() < content_length {
        let read = socket.read(&mut scratch).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&scratch[..read]);
    }
    body.truncate(content_length);

    Ok(RecordedRequest {
        method,
        path,
        version,
        headers,
        body,
    })
}

pub(crate) async fn write_turn(
    socket: &mut TcpStream,
    turn: Turn,
    shutdown: &mut watch::Receiver<bool>,
) -> std::io::Result<()> {
    let mut headers = turn.headers;
    headers.push(("connection".to_owned(), "close".to_owned()));

    let body = turn.body;
    if let Body::Bytes(bytes) = &body {
        headers.push(("content-length".to_owned(), bytes.len().to_string()));
    }
    write_head(socket, turn.status, &headers).await?;

    match body {
        Body::Bytes(bytes) => {
            socket.write_all(&bytes).await?;
        }
        Body::Sse(chunks) => {
            for chunk in chunks {
                let Some(bytes) = deliver(chunk, shutdown).await else {
                    return Ok(());
                };
                socket.write_all(&bytes).await?;
                socket.flush().await?;
            }
        }
        Body::Stalled => {
            let _ = shutdown.changed().await;
        }
    }

    socket.flush().await
}

/// Status line and headers only, for a response whose body is streamed by the
/// caller (the recorder forwards chunks as they arrive).
pub(crate) async fn write_head(
    socket: &mut TcpStream,
    status: u16,
    headers: &[(String, String)],
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");

    socket.write_all(head.as_bytes()).await?;
    socket.flush().await
}

/// Applies a chunk's delay and gate. Returns `None` when the provider is
/// shutting down or the test dropped the gate sender, both of which end the
/// response early.
async fn deliver(chunk: Chunk, shutdown: &mut watch::Receiver<bool>) -> Option<Vec<u8>> {
    let Chunk { bytes, gate, delay } = chunk;

    if let Some(delay) = delay {
        tokio::select! {
            _ = shutdown.changed() => return None,
            _ = tokio::time::sleep(delay) => {}
        }
    }

    if let Some(gate) = gate {
        tokio::select! {
            _ = shutdown.changed() => return None,
            released = gate => {
                if released.is_err() {
                    return None;
                }
            }
        }
    }

    Some(bytes)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
