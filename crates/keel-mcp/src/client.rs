//! The MCP client API shared by both transports.
//!
//! One `McpClient` owns exactly one connection: a stdio child process or a
//! Streamable HTTP session. Both transports implement the same four
//! operations (`request`, notification, shutdown, liveness), so the handshake,
//! `tools/list` pagination, `tools/call`, result rendering, timeout and naming
//! live here once.
//!
//! The HTTP transport is writer-free: every JSON-RPC message is one POST, so
//! calls naturally overlap. The stdio transport keeps one writer task owning
//! the child's stdin so a cancelled call cannot leave half a line behind.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::config::{MAX_STARTUP_TIMEOUT, ServerConfig, TransportKind};
use crate::http::HttpTransport;
use crate::stdio::StdioTransport;

const MAX_TOOL_PAGES: usize = 32;
const MAX_TOOLS: usize = 512;
pub(crate) const MAX_ERROR_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_ERROR_DATA_BYTES: usize = 4 * 1024;
pub(crate) const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Versions this client understands. The initialize response selects one.
const SUPPORTED_VERSIONS: [&str; 2] = ["2025-06-18", "2024-11-05"];
const PREFERRED_VERSION: &str = "2025-06-18";

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
    #[error(
        "MCP server `{server}` rejected the configured headers with HTTP {status}; check the header values in `.keel/mcp.toml`"
    )]
    HeadersRejected { server: String, status: u16 },
    #[error(
        "MCP server `{server}` session expired (HTTP 404); restart keel so the server can be initialized again"
    )]
    SessionExpired { server: String },
    #[error("MCP server `{server}` answered with HTTP {status}: {message}")]
    Http {
        server: String,
        status: u16,
        message: String,
    },
}

impl McpError {
    pub(crate) fn with_stderr(self, tail: &str) -> Self {
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
            Self::HeadersRejected { .. } | Self::SessionExpired { .. } | Self::Http { .. } => self,
        }
    }
}

enum Transport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

impl Transport {
    async fn request(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        match self {
            Self::Stdio(transport) => transport.request(method, params, deadline, timeout).await,
            Self::Http(transport) => transport.request(method, params, deadline, timeout).await,
        }
    }

    async fn notify(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<(), McpError> {
        match self {
            Self::Stdio(transport) => transport.notify(method, params, deadline, timeout).await,
            Self::Http(transport) => transport.notify(method, params, deadline, timeout).await,
        }
    }

    fn set_protocol_version(&self, version: &str) {
        match self {
            Self::Stdio(_) => {}
            Self::Http(transport) => transport.set_protocol_version(version),
        }
    }

    async fn shutdown(&self, message: &str) {
        match self {
            Self::Stdio(transport) => transport.shutdown(message).await,
            Self::Http(transport) => transport.shutdown(message).await,
        }
    }

    async fn is_running(&self) -> bool {
        match self {
            Self::Stdio(transport) => transport.is_running().await,
            Self::Http(transport) => transport.is_running(),
        }
    }

    fn is_closed(&self) -> bool {
        match self {
            Self::Stdio(transport) => transport.is_closed(),
            Self::Http(transport) => transport.is_closed(),
        }
    }

    fn decorate_error(&self, error: McpError) -> McpError {
        match self {
            Self::Stdio(transport) => transport.decorate_error(error),
            Self::Http(_) => error,
        }
    }

    fn drop_cleanup(&self) {
        match self {
            Self::Stdio(transport) => transport.abort(),
            Self::Http(transport) => transport.delete_on_drop(),
        }
    }
}

pub struct McpClient {
    server: String,
    protocol_version: String,
    transport: Transport,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("server", &self.server)
            .field("protocol_version", &self.protocol_version)
            .field("closed", &self.closed())
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
        let transport = match config.transport_kind() {
            Ok(TransportKind::Stdio) => {
                Transport::Stdio(StdioTransport::spawn(server, config, current_dir).await?)
            }
            Ok(TransportKind::Http) => Transport::Http(HttpTransport::connect(server, config)?),
            Err(reason) => {
                return Err(McpError::Spawn {
                    server: server.to_owned(),
                    message: reason,
                });
            }
        };
        let mut client = Self {
            server: server.to_owned(),
            protocol_version: String::new(),
            transport,
        };

        match client.initialize(deadline, timeout).await {
            Ok(protocol) => {
                client.protocol_version = protocol;
                Ok(client)
            }
            Err(error) => {
                client
                    .transport
                    .shutdown("the server did not finish initialization")
                    .await;
                Err(client.transport.decorate_error(error))
            }
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    /// Whether the connection is still usable. A stdio child that exited is
    /// false; an HTTP session is false after shutdown.
    pub async fn is_running(&self) -> bool {
        self.transport.is_running().await
    }

    fn closed(&self) -> bool {
        self.transport.is_closed()
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
        // The version header rides on every later request, starting with
        // `notifications/initialized`.
        self.transport.set_protocol_version(version);
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

    /// Stops the connection and releases its resources. A stdio server is
    /// killed and reaped; an HTTP server is sent a best-effort `DELETE`.
    pub async fn shutdown(&self) {
        self.transport.shutdown("the client was shut down").await;
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
        self.transport
            .request(method, params, deadline, timeout)
            .await
    }

    async fn notify_until(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<(), McpError> {
        self.transport
            .notify(method, params, deadline, timeout)
            .await
    }

    fn protocol_error(&self, message: impl Into<String>) -> McpError {
        McpError::Protocol {
            server: self.server.clone(),
            message: message.into(),
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.transport.drop_cleanup();
    }
}

pub(crate) fn response_id(value: &Value) -> Option<u64> {
    match value.get("id")? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

pub(crate) fn rpc_error(server: &str, value: &Value) -> McpError {
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

pub(crate) fn truncate_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    format!("{}…", &text[..floor_char_boundary(text, limit)])
}

pub(crate) fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub(crate) fn render_content(item: &Value) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

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
