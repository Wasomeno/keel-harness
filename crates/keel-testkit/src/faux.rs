//! A scripted HTTP server for transport tests.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify, oneshot, watch};
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

/// A running scripted server.
#[derive(Debug)]
pub struct FauxProvider {
    url: String,
    state: Arc<State>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

#[derive(Debug)]
struct State {
    turns: Mutex<VecDeque<Turn>>,
    requests: Mutex<Vec<RecordedRequest>>,
    notify: Notify,
}

impl FauxProvider {
    /// Binds an ephemeral port and starts serving `turns` in order.
    pub async fn start(turns: impl IntoIterator<Item = Turn>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("faux provider: bind an ephemeral port");
        let address = listener.local_addr().expect("faux provider: local address");
        let state = Arc::new(State {
            turns: Mutex::new(turns.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
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
    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn requests(&self) -> Vec<RecordedRequest> {
        self.state.requests.lock().await.clone()
    }

    pub async fn request_count(&self) -> usize {
        self.state.requests.lock().await.len()
    }

    /// Waits until at least `count` requests have been received. Panics on
    /// timeout so a deadlocked test fails instead of hanging.
    pub async fn wait_for_requests(&self, count: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if self.request_count().await >= count {
                return;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "faux provider: timed out waiting for {count} requests"
            );
            let _ = tokio::time::timeout(
                remaining.min(Duration::from_millis(100)),
                self.state.notify.notified(),
            )
            .await;
        }
    }
}

impl Drop for FauxProvider {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.task.abort();
    }
}

async fn accept_loop(
    listener: TcpListener,
    state: Arc<State>,
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

async fn handle(mut socket: TcpStream, state: Arc<State>, mut shutdown: watch::Receiver<bool>) {
    let Ok(request) = read_request(&mut socket).await else {
        return;
    };
    state.record(request).await;

    let turn = state.take_turn().await;
    let _ = write_turn(&mut socket, turn, &mut shutdown).await;
    let _ = socket.shutdown().await;
}

impl State {
    async fn record(&self, request: RecordedRequest) {
        self.requests.lock().await.push(request);
        self.notify.notify_waiters();
    }

    async fn take_turn(&self) -> Turn {
        self.turns
            .lock()
            .await
            .pop_front()
            .unwrap_or_else(|| Turn::text(500, "faux provider: no scripted turns left"))
    }
}

async fn read_request(socket: &mut TcpStream) -> std::io::Result<RecordedRequest> {
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

async fn write_turn(
    socket: &mut TcpStream,
    turn: Turn,
    shutdown: &mut watch::Receiver<bool>,
) -> std::io::Result<()> {
    let mut headers = turn.headers;
    headers.push(("connection".to_owned(), "close".to_owned()));

    let mut head = format!("HTTP/1.1 {} {}\r\n", turn.status, reason(turn.status));
    let body = turn.body;
    if let Body::Bytes(bytes) = &body {
        headers.push(("content-length".to_owned(), bytes.len().to_string()));
    }
    for (name, value) in &headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");

    socket.write_all(head.as_bytes()).await?;
    socket.flush().await?;

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
