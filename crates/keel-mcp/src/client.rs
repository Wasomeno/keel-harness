//! A small MCP stdio client.
//!
//! The client speaks newline-delimited JSON-RPC 2.0, one request at a time per
//! call but with response routing by id so parallel-safe tools can overlap.
//! Reads are line-bounded; a server cannot make keel buffer an unbounded line.

use std::collections::{BTreeSet, HashMap};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::Instant;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

use crate::config::ServerConfig;

const MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_TOOL_PAGES: usize = 32;
const MAX_TOOLS: usize = 512;
const MAX_ERROR_DATA_BYTES: usize = 4 * 1024;

/// Versions this client understands. The initialize response selects one.
const SUPPORTED_VERSIONS: [&str; 2] = ["2025-06-18", "2024-11-05"];
const PREFERRED_VERSION: &str = "2025-06-18";

type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpError>>>>>;

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

pub struct McpClient {
    server: String,
    protocol_version: String,
    _child: Mutex<Child>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    reader: Mutex<Option<JoinHandle<()>>>,
    closed: Arc<AtomicBool>,
    closed_reason: Arc<Mutex<Option<String>>>,
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
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
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

        let pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let closed_reason = Arc::new(Mutex::new(None));
        let reader = tokio::spawn(reader_loop(
            server.to_owned(),
            stdout,
            pending.clone(),
            closed.clone(),
            closed_reason.clone(),
            MAX_LINE_BYTES,
        ));

        let client = Self {
            server: server.to_owned(),
            protocol_version: String::new(),
            _child: Mutex::new(child),
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            next_id: AtomicU64::new(1),
            reader: Mutex::new(Some(reader)),
            closed,
            closed_reason,
        };

        let protocol = client.initialize(timeout).await?;
        let mut client = client;
        client.protocol_version = protocol;
        Ok(client)
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    async fn initialize(&self, timeout: Duration) -> Result<String, McpError> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PREFERRED_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "keel", "version": env!("CARGO_PKG_VERSION")}
                }),
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
        self.notify("notifications/initialized", json!({}), timeout)
            .await?;
        Ok(version.to_owned())
    }

    /// Fetches every page of `tools/list`.
    pub async fn list_tools(&self, timeout: Duration) -> Result<Vec<RemoteTool>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let result = self.request("tools/list", params, timeout).await?;
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

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<CallResult, McpError> {
        let result = self
            .request(
                "tools/call",
                json!({"name": name, "arguments": arguments}),
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

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(self.closed_error().await);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, sender);
        if self.closed.load(Ordering::Relaxed) {
            self.pending.lock().unwrap().remove(&id);
            return Err(self.closed_error().await);
        }
        let _guard = PendingGuard {
            id,
            pending: self.pending.clone(),
            stdin: self.stdin.clone(),
            server: self.server.clone(),
        };

        let deadline = Instant::now() + timeout;
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        match tokio::time::timeout_at(deadline, self.write_line(&line)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(self.timeout_error(timeout)),
        }
        match tokio::time::timeout_at(deadline, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(self.protocol_error("the response sender was dropped")),
            Err(_) => Err(self.timeout_error(timeout)),
        }
    }

    async fn notify(&self, method: &str, params: Value, timeout: Duration) -> Result<(), McpError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(self.closed_error().await);
        }
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        match tokio::time::timeout(timeout, self.write_line(&line)).await {
            Ok(result) => result,
            Err(_) => Err(self.timeout_error(timeout)),
        }
    }

    async fn write_line(&self, line: &str) -> Result<(), McpError> {
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| self.io_error(format!("could not write to stdin: {error}")))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|error| self.io_error(format!("could not write to stdin: {error}")))?;
        stdin
            .flush()
            .await
            .map_err(|error| self.io_error(format!("could not flush stdin: {error}")))
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

    async fn closed_error(&self) -> McpError {
        McpError::Closed {
            server: self.server.clone(),
            message: self
                .closed_reason
                .lock()
                .await
                .clone()
                .unwrap_or_else(|| "the server is not running".to_owned()),
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Ok(mut child) = self._child.try_lock() {
            let _ = child.start_kill();
        }
        if let Ok(mut reader) = self.reader.try_lock()
            && let Some(handle) = reader.take()
        {
            handle.abort();
        }
        // `kill_on_drop(true)` is the fallback when the child mutex is busy.
    }
}

struct PendingGuard {
    id: u64,
    pending: Pending,
    stdin: Arc<Mutex<ChildStdin>>,
    server: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let was_pending = self
            .pending
            .lock()
            .map(|mut pending| pending.remove(&self.id).is_some())
            .unwrap_or(false);
        if !was_pending {
            return;
        }
        let stdin = self.stdin.clone();
        let id = self.id;
        let server = self.server.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let line = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {
                        "requestId": id,
                        "reason": format!("keel cancelled the call on `{server}`")
                    }
                })
                .to_string();
                let mut stdin = stdin.lock().await;
                let _ = stdin.write_all(line.as_bytes()).await;
                let _ = stdin.write_all(b"\n").await;
                let _ = stdin.flush().await;
            });
        }
    }
}

async fn reader_loop(
    server: String,
    stdout: ChildStdout,
    pending: Pending,
    closed: Arc<AtomicBool>,
    closed_reason: Arc<Mutex<Option<String>>>,
    max_line_bytes: usize,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let line = match read_bounded_line(&mut reader, max_line_bytes).await {
            Ok(Some(line)) => line,
            Ok(None) => {
                fail_pending(
                    &server,
                    &pending,
                    &closed,
                    &closed_reason,
                    "the server closed stdout".to_owned(),
                )
                .await;
                return;
            }
            Err(error) => {
                fail_pending(
                    &server,
                    &pending,
                    &closed,
                    &closed_reason,
                    error.to_string(),
                )
                .await;
                return;
            }
        };
        let value: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(error) => {
                fail_pending(
                    &server,
                    &pending,
                    &closed,
                    &closed_reason,
                    format!("invalid JSON on stdout: {error}"),
                )
                .await;
                return;
            }
        };

        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            // Notifications, including progress, have no id and are ignored.
            continue;
        };
        let sender = pending.lock().unwrap().remove(&id);
        let Some(sender) = sender else {
            continue;
        };
        let result = if let Some(result) = value.get("result") {
            Ok(result.clone())
        } else if let Some(error) = value.get("error") {
            Err(rpc_error(&server, error))
        } else {
            Err(McpError::Protocol {
                server: server.clone(),
                message: format!("response {id} has neither result nor error"),
            })
        };
        let _ = sender.send(result);
    }
}

async fn fail_pending(
    server: &str,
    pending: &Pending,
    closed: &Arc<AtomicBool>,
    closed_reason: &Arc<Mutex<Option<String>>>,
    message: String,
) {
    *closed_reason.lock().await = Some(message.clone());
    closed.store(true, Ordering::Relaxed);
    let error = McpError::Closed {
        server: server.to_owned(),
        message,
    };
    let mut pending = pending.lock().unwrap();
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err(error.clone()));
    }
}

fn rpc_error(server: &str, value: &Value) -> McpError {
    let code = value.get("code").and_then(Value::as_i64).unwrap_or(-32603);
    let mut message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown JSON-RPC error")
        .to_owned();
    if let Some(data) = value.get("data") {
        let text = serde_json::to_string(data).unwrap_or_else(|_| data.to_string());
        let text = if text.len() <= MAX_ERROR_DATA_BYTES {
            text
        } else {
            format!(
                "{}…",
                &text[..floor_char_boundary(&text, MAX_ERROR_DATA_BYTES)]
            )
        };
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
}
