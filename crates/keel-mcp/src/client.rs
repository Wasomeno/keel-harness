//! A small MCP stdio client.
//!
//! The client speaks newline-delimited JSON-RPC 2.0. One writer task owns the
//! child's stdin so a cancelled call can never leave half a line behind, and
//! the reader routes responses by id so parallel-safe tools can overlap.
//! Reads are line-bounded; a server cannot make keel buffer an unbounded line.

use std::collections::{BTreeSet, HashMap};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::config::{MAX_STARTUP_TIMEOUT, ServerConfig};

const MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_TOOL_PAGES: usize = 32;
const MAX_TOOLS: usize = 512;
const MAX_ERROR_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_ERROR_DATA_BYTES: usize = 4 * 1024;
const MAX_STDERR_BYTES: usize = 4 * 1024;
const WRITE_QUEUE: usize = 32;
const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);

/// Versions this client understands. The initialize response selects one.
const SUPPORTED_VERSIONS: [&str; 2] = ["2025-06-18", "2024-11-05"];
const PREFERRED_VERSION: &str = "2025-06-18";

type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpError>>>>>;
type Reason = Arc<std::sync::Mutex<Option<String>>>;
type StderrTail = Arc<std::sync::Mutex<String>>;

#[derive(Clone)]
struct Connection {
    server: String,
    pending: Pending,
    closed: Arc<AtomicBool>,
    closed_reason: Reason,
    child: Arc<Mutex<Child>>,
    stderr_tail: StderrTail,
    writer: mpsc::Sender<WriteCommand>,
}

struct WriteCommand {
    line: String,
    ack: oneshot::Sender<Result<(), String>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RemoteTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Clone, Debug)]
pub struct CallResult {
    pub text: String,
    pub is_error: bool,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum McpError {
    #[error("could not start MCP server `{server}`: {message}")]
    Spawn { server: String, message: String },
    #[error("MCP server `{server}` I/O failed: {message}")]
    Io { server: String, message: String },
    #[error("MCP server `{server}` did not answer within {after:?}")]
    Timeout { server: String, after: Duration },
    #[error("MCP server `{server}` protocol error: {message}")]
    Protocol { server: String, message: String },
    #[error("MCP server `{server}` closed: {message}")]
    Closed { server: String, message: String },
    #[error("MCP server `{server}` returned error {code}: {message}")]
    Rpc {
        server: String,
        code: i64,
        message: String,
    },
}

impl McpError {
    fn with_stderr(self, tail: &str) -> Self {
        let tail = tail.split_whitespace().collect::<Vec<_>>().join(" ");
        if tail.is_empty() {
            return self;
        }
        match self {
            Self::Spawn { server, message } => Self::Spawn {
                server,
                message: format!("{message}; stderr: {tail}"),
            },
            Self::Io { server, message } => Self::Io {
                server,
                message: format!("{message}; stderr: {tail}"),
            },
            Self::Timeout { server, after } => Self::Timeout { server, after },
            Self::Protocol { server, message } => Self::Protocol {
                server,
                message: format!("{message}; stderr: {tail}"),
            },
            Self::Closed { server, message } => Self::Closed {
                server,
                message: format!("{message}; stderr: {tail}"),
            },
            Self::Rpc {
                server,
                code,
                message,
            } => Self::Rpc {
                server,
                code,
                message: format!("{message}; stderr: {tail}"),
            },
        }
    }
}

pub struct McpClient {
    server: String,
    protocol_version: String,
    child: Arc<Mutex<Child>>,
    writer: mpsc::Sender<WriteCommand>,
    pending: Pending,
    next_id: AtomicU64,
    reader: JoinHandle<()>,
    writer_task: JoinHandle<()>,
    stderr_task: Mutex<Option<JoinHandle<()>>>,
    stderr_tail: StderrTail,
    closed: Arc<AtomicBool>,
    closed_reason: Reason,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("server", &self.server)
            .field("protocol_version", &self.protocol_version)
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish()
    }
}

impl McpClient {
    pub async fn start(
        server: &str,
        config: &ServerConfig,
        timeout: Duration,
        current_dir: Option<&std::path::Path>,
    ) -> Result<Self, McpError> {
        let timeout = timeout.min(MAX_STARTUP_TIMEOUT);
        let deadline = Instant::now() + timeout;
        Self::start_until(server, config, deadline, timeout, current_dir).await
    }

    pub(crate) async fn start_until(
        server: &str,
        config: &ServerConfig,
        deadline: Instant,
        timeout: Duration,
        current_dir: Option<&std::path::Path>,
    ) -> Result<Self, McpError> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(current_dir) = current_dir {
            command.current_dir(current_dir);
        }
        command.env_clear();
        for name in environment_names(&config.env) {
            if let Some(value) = std::env::var_os(&name) {
                command.env(name, value);
            }
        }

        let mut child = command.spawn().map_err(|error| McpError::Spawn {
            server: server.to_owned(),
            message: error.to_string(),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| McpError::Spawn {
            server: server.to_owned(),
            message: "the child has no stdin pipe".to_owned(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::Spawn {
            server: server.to_owned(),
            message: "the child has no stdout pipe".to_owned(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| McpError::Spawn {
            server: server.to_owned(),
            message: "the child has no stderr pipe".to_owned(),
        })?;

        let pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let closed_reason = Arc::new(std::sync::Mutex::new(None));
        let stderr_tail = Arc::new(std::sync::Mutex::new(String::new()));
        let child = Arc::new(Mutex::new(child));
        let (writer, writer_rx) = mpsc::channel(WRITE_QUEUE);
        let connection = Connection {
            server: server.to_owned(),
            pending: pending.clone(),
            closed: closed.clone(),
            closed_reason: closed_reason.clone(),
            child: child.clone(),
            stderr_tail: stderr_tail.clone(),
            writer: writer.clone(),
        };

        let reader = tokio::spawn(reader_loop(connection.clone(), stdout, MAX_LINE_BYTES));
        let writer_task = tokio::spawn(writer_loop(connection, stdin, writer_rx));
        let stderr_task = tokio::spawn(stderr_loop(stderr, stderr_tail.clone()));

        let mut client = Self {
            server: server.to_owned(),
            protocol_version: String::new(),
            child,
            writer,
            pending,
            next_id: AtomicU64::new(1),
            reader,
            writer_task,
            stderr_task: Mutex::new(Some(stderr_task)),
            stderr_tail,
            closed,
            closed_reason,
        };

        match client.initialize(deadline, timeout).await {
            Ok(protocol) => {
                client.protocol_version = protocol;
                Ok(client)
            }
            Err(error) => {
                client
                    .shutdown("the server did not finish initialization")
                    .await;
                client.finish_stderr().await;
                Err(error.with_stderr(&client.stderr_text()))
            }
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    /// Whether the child is still running. After a reader failure this is
    /// false because the reader kills and reaps the child.
    pub async fn is_running(&self) -> bool {
        let mut child = self.child.lock().await;
        matches!(child.try_wait(), Ok(None))
    }

    async fn initialize(&self, deadline: Instant, timeout: Duration) -> Result<String, McpError> {
        let result = self
            .request_until(
                "initialize",
                json!({
                    "protocolVersion": PREFERRED_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "keel", "version": env!("CARGO_PKG_VERSION")}
                }),
                deadline,
                timeout,
            )
            .await?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| self.protocol_error("initialize response has no protocolVersion"))?;
        if !SUPPORTED_VERSIONS.contains(&version) {
            return Err(self.protocol_error(format!(
                "server chose unsupported MCP protocol version `{version}`"
            )));
        }
        self.notify_until("notifications/initialized", json!({}), deadline, timeout)
            .await?;
        Ok(version.to_owned())
    }

    /// Fetches every page of `tools/list`, sharing one startup deadline.
    pub(crate) async fn list_tools_until(
        &self,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<Vec<RemoteTool>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let result = self
                .request_until("tools/list", params, deadline, timeout)
                .await?;
            let page = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| self.protocol_error("tools/list result has no tools array"))?;
            for value in page {
                let tool: RemoteTool = serde_json::from_value(value.clone())
                    .map_err(|error| self.protocol_error(format!("invalid tool entry: {error}")))?;
                tools.push(tool);
                if tools.len() > MAX_TOOLS {
                    return Err(self.protocol_error(format!(
                        "tools/list returned more than {MAX_TOOLS} tools"
                    )));
                }
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                return Ok(tools);
            }
        }
        Err(self.protocol_error(format!("tools/list exceeded {MAX_TOOL_PAGES} pages")))
    }

    /// Fetches every page of `tools/list` within a fresh timeout.
    pub async fn list_tools(&self, timeout: Duration) -> Result<Vec<RemoteTool>, McpError> {
        self.list_tools_until(Instant::now() + timeout, timeout)
            .await
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<CallResult, McpError> {
        let result = self
            .request_until(
                "tools/call",
                json!({"name": name, "arguments": arguments}),
                Instant::now() + timeout,
                timeout,
            )
            .await?;
        self.render_call_result(&result)
    }

    fn render_call_result(&self, result: &Value) -> Result<CallResult, McpError> {
        let object = result
            .as_object()
            .ok_or_else(|| self.protocol_error("tools/call result is not an object"))?;
        let is_error = object
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut parts = Vec::new();
        if let Some(content) = object.get("content").and_then(Value::as_array) {
            for item in content {
                parts.push(render_content(item));
            }
        } else if let Some(structured) = object.get("structuredContent") {
            parts.push(
                serde_json::to_string(structured)
                    .unwrap_or_else(|_| "[structured content]".to_owned()),
            );
        } else {
            return Err(self.protocol_error("tools/call result has no content array"));
        }
        Ok(CallResult {
            text: parts.join("\n"),
            is_error,
        })
    }

    async fn request_until(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(self.closed_error());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, sender);
        if self.closed.load(Ordering::Relaxed) {
            self.pending.lock().unwrap().remove(&id);
            return Err(self.closed_error());
        }
        let _guard = PendingGuard {
            id,
            pending: self.pending.clone(),
            writer: self.writer.clone(),
            server: self.server.clone(),
            closed: self.closed.clone(),
        };

        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        self.write_line_until(&line, deadline, timeout).await?;
        match tokio::time::timeout_at(deadline, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(self.protocol_error("the response sender was dropped")),
            Err(_) => Err(self.timeout_error(timeout)),
        }
    }

    async fn notify_until(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<(), McpError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(self.closed_error());
        }
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        self.write_line_until(&line, deadline, timeout).await
    }

    /// Writes one whole line through the writer task. A timeout here can drop
    /// the caller, but the writer still completes the line, so the next request
    /// never starts in the middle of a JSON object.
    async fn write_line_until(
        &self,
        line: &str,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<(), McpError> {
        let (ack, receiver) = oneshot::channel();
        let command = WriteCommand {
            line: line.to_owned(),
            ack,
        };
        match tokio::time::timeout_at(deadline, self.writer.send(command)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(self.closed_error()),
            Err(_) => return Err(self.timeout_error(timeout)),
        }
        match tokio::time::timeout_at(deadline, receiver).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(message))) => Err(self.io_error(message)),
            Ok(Err(_)) => Err(self.protocol_error("the writer task dropped the request")),
            Err(_) => Err(self.timeout_error(timeout)),
        }
    }

    fn protocol_error(&self, message: impl Into<String>) -> McpError {
        McpError::Protocol {
            server: self.server.clone(),
            message: message.into(),
        }
    }

    fn io_error(&self, message: impl Into<String>) -> McpError {
        McpError::Io {
            server: self.server.clone(),
            message: message.into(),
        }
    }

    fn timeout_error(&self, after: Duration) -> McpError {
        McpError::Timeout {
            server: self.server.clone(),
            after,
        }
    }

    fn closed_error(&self) -> McpError {
        McpError::Closed {
            server: self.server.clone(),
            message: self
                .closed_reason
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "the server is not running".to_owned()),
        }
    }

    fn stderr_text(&self) -> String {
        self.stderr_tail.lock().unwrap().clone()
    }

    async fn finish_stderr(&self) {
        let handle = self.stderr_task.lock().await.take();
        if let Some(handle) = handle {
            let _ = tokio::time::timeout(Duration::from_millis(200), handle).await;
        }
    }

    fn connection(&self) -> Connection {
        Connection {
            server: self.server.clone(),
            pending: self.pending.clone(),
            closed: self.closed.clone(),
            closed_reason: self.closed_reason.clone(),
            child: self.child.clone(),
            stderr_tail: self.stderr_tail.clone(),
            writer: self.writer.clone(),
        }
    }

    async fn shutdown(&self, message: &str) {
        close_and_kill(&self.connection(), message.to_owned()).await;
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
        }
        self.reader.abort();
        self.writer_task.abort();
        if let Ok(mut task) = self.stderr_task.try_lock()
            && let Some(handle) = task.take()
        {
            handle.abort();
        }
        // `kill_on_drop(true)` is the fallback when the child mutex is busy.
    }
}

struct PendingGuard {
    id: u64,
    pending: Pending,
    writer: mpsc::Sender<WriteCommand>,
    server: String,
    closed: Arc<AtomicBool>,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let was_pending = self
            .pending
            .lock()
            .map(|mut pending| pending.remove(&self.id).is_some())
            .unwrap_or(false);
        if !was_pending || self.closed.load(Ordering::Relaxed) {
            return;
        }
        let line = json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {
                "requestId": self.id,
                "reason": format!("keel cancelled the call on `{}`", self.server)
            }
        })
        .to_string();
        let (ack, _receiver) = oneshot::channel();
        let _ = self.writer.try_send(WriteCommand { line, ack });
    }
}

async fn writer_loop(
    connection: Connection,
    mut stdin: ChildStdin,
    mut receiver: mpsc::Receiver<WriteCommand>,
) {
    while let Some(command) = receiver.recv().await {
        let mut bytes = Vec::with_capacity(command.line.len() + 1);
        bytes.extend_from_slice(command.line.as_bytes());
        bytes.push(b'\n');
        let result = async {
            stdin
                .write_all(&bytes)
                .await
                .map_err(|error| format!("could not write to stdin: {error}"))?;
            stdin
                .flush()
                .await
                .map_err(|error| format!("could not flush stdin: {error}"))
        }
        .await;
        match result {
            Ok(()) => {
                let _ = command.ack.send(Ok(()));
            }
            Err(message) => {
                let _ = command.ack.send(Err(message.clone()));
                close_and_kill(&connection, message).await;
                return;
            }
        }
    }
}

async fn reader_loop(connection: Connection, stdout: ChildStdout, max_line_bytes: usize) {
    let mut reader = BufReader::new(stdout);
    loop {
        let line = match read_bounded_line(&mut reader, max_line_bytes).await {
            Ok(Some(line)) => line,
            Ok(None) => {
                close_and_kill(&connection, "the server closed stdout".to_owned()).await;
                return;
            }
            Err(error) => {
                close_and_kill(&connection, error.to_string()).await;
                return;
            }
        };
        let line = match String::from_utf8(line) {
            Ok(line) => line,
            Err(error) => {
                close_and_kill(
                    &connection,
                    format!("a stdout line is not UTF-8: {error}; closing the server"),
                )
                .await;
                return;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                close_and_kill(&connection, format!("invalid JSON on stdout: {error}")).await;
                return;
            }
        };
        if !value.is_object() {
            close_and_kill(
                &connection,
                "a stdout message is not a JSON object".to_owned(),
            )
            .await;
            return;
        }
        handle_inbound(
            &connection.server,
            value,
            &connection.pending,
            &connection.writer,
        );
    }
}

fn handle_inbound(
    server: &str,
    value: Value,
    pending: &Pending,
    writer: &mpsc::Sender<WriteCommand>,
) {
    if let Some(method) = value.get("method").and_then(Value::as_str) {
        match value.get("id") {
            Some(id) if !id.is_null() => {
                let response = if method == "ping" {
                    json!({"jsonrpc": "2.0", "id": id, "result": {}})
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!("unsupported MCP server request `{method}`")
                        }
                    })
                };
                send_control(writer, &response.to_string());
                debug(
                    server,
                    &format!("answered server request `{method}` with id {id}"),
                );
            }
            _ => debug(server, &format!("ignored notification `{method}`")),
        }
        return;
    }

    let Some(id) = response_id(&value) else {
        debug(
            server,
            "ignored a message that has neither a method nor a usable id",
        );
        return;
    };
    let sender = pending.lock().unwrap().remove(&id);
    let Some(sender) = sender else {
        debug(
            server,
            &format!("ignored a response for unknown request id {id}"),
        );
        return;
    };
    let result = if let Some(result) = value.get("result") {
        Ok(result.clone())
    } else if let Some(error) = value.get("error") {
        Err(rpc_error(server, error))
    } else {
        Err(McpError::Protocol {
            server: server.to_owned(),
            message: format!("response {id} has neither result nor error"),
        })
    };
    let _ = sender.send(result);
}

fn response_id(value: &Value) -> Option<u64> {
    match value.get("id")? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn send_control(writer: &mpsc::Sender<WriteCommand>, line: &str) {
    let (ack, _receiver) = oneshot::channel();
    let _ = writer.try_send(WriteCommand {
        line: line.to_owned(),
        ack,
    });
}

fn debug(server: &str, message: &str) {
    if std::env::var_os("KEEL_MCP_DEBUG").is_some() {
        eprintln!("keel: mcp[{server}]: {message}");
    }
}

async fn close_and_kill(connection: &Connection, message: String) {
    connection.closed.store(true, Ordering::SeqCst);
    let message = with_stderr(message, &connection.stderr_tail);
    {
        let mut reason = connection.closed_reason.lock().unwrap();
        if reason.is_none() {
            *reason = Some(message.clone());
        }
    }
    fail_pending(connection);
    let mut child = connection.child.lock().await;
    let _ = child.start_kill();
    let _ = tokio::time::timeout(SHUTDOWN_WAIT, child.wait()).await;
}

fn fail_pending(connection: &Connection) {
    let message = connection
        .closed_reason
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| "the server is not running".to_owned());
    let error = McpError::Closed {
        server: connection.server.clone(),
        message,
    };
    let mut pending = connection.pending.lock().unwrap();
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err(error.clone()));
    }
}

fn with_stderr(message: String, stderr_tail: &StderrTail) -> String {
    let tail = stderr_tail.lock().unwrap();
    let tail = tail.split_whitespace().collect::<Vec<_>>().join(" ");
    if tail.is_empty() {
        return message;
    }
    format!("{message} (stderr: {})", truncate_text(&tail, 2048))
}

async fn stderr_loop(mut stderr: ChildStderr, tail: StderrTail) {
    let mut buffer = [0u8; 1024];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => append_stderr(&tail, &buffer[..read]),
        }
    }
}

fn append_stderr(tail: &StderrTail, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let mut tail = tail.lock().unwrap();
    tail.push_str(&text);
    if tail.len() > MAX_STDERR_BYTES {
        let cut = floor_char_boundary(&tail, tail.len() - MAX_STDERR_BYTES);
        tail.drain(..cut);
    }
}

fn rpc_error(server: &str, value: &Value) -> McpError {
    let code = value.get("code").and_then(Value::as_i64).unwrap_or(-32603);
    let mut message = truncate_text(
        value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown JSON-RPC error"),
        MAX_ERROR_MESSAGE_BYTES,
    );
    if let Some(data) = value.get("data") {
        let text = serde_json::to_string(data).unwrap_or_else(|_| data.to_string());
        let text = truncate_text(&text, MAX_ERROR_DATA_BYTES);
        message.push_str(" (data: ");
        message.push_str(&text);
        message.push(')');
    }
    McpError::Rpc {
        server: server.to_owned(),
        code,
        message,
    }
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    format!("{}…", &text[..floor_char_boundary(text, limit)])
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn render_content(item: &Value) -> String {
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match kind {
        "text" => item
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| "[text content without text]".to_owned()),
        "image" => format!(
            "[image content: {}]",
            item.get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("unknown media type")
        ),
        "audio" => format!(
            "[audio content: {}]",
            item.get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("unknown media type")
        ),
        "resource" => {
            let resource = item.get("resource").unwrap_or(&Value::Null);
            if let Some(text) = resource.get("text").and_then(Value::as_str) {
                text.to_owned()
            } else if let Some(uri) = resource.get("uri").and_then(Value::as_str) {
                format!("[resource: {uri}]")
            } else {
                "[resource content]".to_owned()
            }
        }
        "resource_link" | "resourceLink" => format!(
            "[resource link: {}]",
            item.get("uri")
                .and_then(Value::as_str)
                .unwrap_or("unknown uri")
        ),
        other => format!("[unsupported MCP content type: {other}]"),
    }
}

async fn read_bounded_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        if let Some(index) = available.iter().position(|byte| *byte == b'\n') {
            if line.len() + index > max_bytes {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("a JSON-RPC line exceeded {max_bytes} bytes"),
                ));
            }
            line.extend_from_slice(&available[..index]);
            reader.consume(index + 1);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
        if line.len() + available.len() > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("a JSON-RPC line exceeded {max_bytes} bytes"),
            ));
        }
        line.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }
}

fn environment_names(configured: &[String]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for name in [
        "PATH",
        "HOME",
        "TMPDIR",
        "TMP",
        "TEMP",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "SYSTEMROOT",
        "PATHEXT",
    ] {
        names.insert(name.to_owned());
    }
    names.extend(configured.iter().cloned());
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_line_without_a_newline_is_bounded_at_eof() {
        let mut reader = BufReader::new(&b"abc"[..]);
        let line = read_bounded_line(&mut reader, 8).await.unwrap();
        assert_eq!(line.as_deref(), Some(&b"abc"[..]));
    }

    #[tokio::test]
    async fn an_overlong_line_is_rejected() {
        let mut reader = BufReader::new(&b"abcdef\n"[..]);
        let error = read_bounded_line(&mut reader, 3).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn non_text_content_becomes_a_visible_placeholder() {
        assert_eq!(
            render_content(&json!({"type": "image", "mimeType": "image/png"})),
            "[image content: image/png]"
        );
        assert_eq!(
            render_content(&json!({"type": "text", "text": "hello"})),
            "hello"
        );
    }

    #[test]
    fn rpc_error_messages_are_bounded() {
        let long = "x".repeat(MAX_ERROR_MESSAGE_BYTES * 2);
        let error = rpc_error("test", &json!({"code": -1, "message": long}));
        match error {
            McpError::Rpc { message, .. } => {
                assert!(message.len() <= MAX_ERROR_MESSAGE_BYTES + 4);
            }
            other => panic!("expected an RPC error, got {other}"),
        }
    }

    #[test]
    fn response_ids_accept_numbers_and_numeric_strings() {
        assert_eq!(response_id(&json!({"id": 7})), Some(7));
        assert_eq!(response_id(&json!({"id": "7"})), Some(7));
        assert_eq!(response_id(&json!({"id": "seven"})), None);
    }
}
