//! Streamable HTTP transport.
//!
//! One JSON-RPC message is one POST. A request carries `Content-Type:
//! application/json` and `Accept: application/json, text/event-stream`; the
//! response is either one JSON message or an SSE stream whose `data:` frames
//! are JSON-RPC messages. There is no writer task: `reqwest::Client` is
//! cloneable and each call owns its request, so `parallel_safe` calls overlap
//! without a shared lock.
//!
//! Session and protocol headers are added after `initialize`:
//! `Mcp-Session-Id` when the server offered one, and `MCP-Protocol-Version`
//! with the negotiated version. Non-2xx responses become typed errors that
//! never mention stderr.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use tokio::runtime::Handle;
use tokio::time::Instant;

use crate::client::{
    MAX_ERROR_MESSAGE_BYTES, MAX_MESSAGE_BYTES, McpError, response_id, rpc_error, truncate_text,
};
use crate::config::{ServerConfig, is_reserved_header, validate_url};

const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
const ERROR_BODY_BYTES: usize = 4 * 1024;
const CANCEL_TIMEOUT: Duration = Duration::from_secs(2);
const DELETE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct HttpTransport {
    server: String,
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    closed: AtomicBool,
    closed_reason: Mutex<Option<String>>,
    next_id: AtomicU64,
    shutdown_started: AtomicBool,
    runtime: Handle,
}

/// The immutable part of one POST: a snapshot of the session headers at the
/// time the request was issued. Cancellation and server-request answers reuse
/// the same snapshot on a detached task.
#[derive(Clone)]
struct PostContext {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
}

impl PostContext {
    fn post(&self, body: String) -> reqwest::RequestBuilder {
        self.client
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .headers(self.headers.clone())
            .body(body)
    }
}

impl HttpTransport {
    pub(crate) fn connect(server: &str, config: &ServerConfig) -> Result<Self, McpError> {
        let url = config.url.as_deref().unwrap_or_default().trim().to_owned();
        validate_url(&url).map_err(|message| McpError::Protocol {
            server: server.to_owned(),
            message,
        })?;
        let mut headers = HeaderMap::new();
        for (name, value) in &config.headers {
            if is_reserved_header(name) {
                return Err(McpError::Protocol {
                    server: server.to_owned(),
                    message: format!(
                        "header `{name}` is reserved by the transport and cannot be configured"
                    ),
                });
            }
            let header_name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| McpError::Protocol {
                    server: server.to_owned(),
                    message: format!("header name `{name}` is not a valid HTTP header name"),
                })?;
            let header_value = HeaderValue::from_str(value).map_err(|_| McpError::Protocol {
                server: server.to_owned(),
                message: format!(
                    "header `{name}` has a value that HTTP header values cannot carry"
                ),
            })?;
            headers.insert(header_name, header_value);
        }
        let client = reqwest::Client::builder()
            // The MCP endpoint is the configured URL. Following a redirect
            // would move the session and configured headers somewhere else.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| McpError::Io {
                server: server.to_owned(),
                message: format!("could not create the HTTP client: {error}"),
            })?;
        Ok(Self {
            server: server.to_owned(),
            client,
            url,
            headers,
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
            closed: AtomicBool::new(false),
            closed_reason: Mutex::new(None),
            next_id: AtomicU64::new(1),
            shutdown_started: AtomicBool::new(false),
            runtime: Handle::current(),
        })
    }

    pub(crate) fn set_protocol_version(&self, version: &str) {
        *self.protocol_version.lock().unwrap() = Some(version.to_owned());
    }

    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        self.ensure_open()?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        let context = if method == "initialize" {
            self.context_without_session()
        } else {
            self.current_context()
        };
        let mut guard = CancelGuard::new(
            self.runtime.clone(),
            context.clone(),
            self.server.clone(),
            id,
        );

        let result =
            tokio::time::timeout_at(deadline, self.exchange_request(&context, &body, id, method))
                .await;
        match result {
            Ok(result) => {
                guard.disarm();
                // A server can reflect a configured header into any message it
                // sends, including a JSON-RPC error with a `data` payload.
                result.map_err(|error| self.redact_error(error))
            }
            Err(_) => {
                guard.cancel_now();
                Err(self.timeout_error(timeout))
            }
        }
    }

    pub(crate) async fn notify(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<(), McpError> {
        self.ensure_open()?;
        let body = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        let context = self.current_context();
        match tokio::time::timeout_at(deadline, self.exchange_notification(&context, &body)).await {
            Ok(result) => result.map_err(|error| self.redact_error(error)),
            Err(_) => Err(self.timeout_error(timeout)),
        }
    }

    pub(crate) async fn shutdown(&self, message: &str) {
        self.closed.store(true, Ordering::SeqCst);
        *self.closed_reason.lock().unwrap() = Some(message.to_owned());
        if self.shutdown_started.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.session_id.lock().unwrap().is_none() {
            return;
        }
        let request = self
            .client
            .delete(&self.url)
            .headers(self.current_context().headers);
        let _ = tokio::time::timeout(DELETE_TIMEOUT, request.send()).await;
    }

    /// `Drop` cannot await. Spawn the DELETE when a runtime is still alive and
    /// ignore every failure; an explicit `shutdown` is the reliable path.
    pub(crate) fn delete_on_drop(&self) {
        if self.shutdown_started.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.session_id.lock().unwrap().is_none() {
            return;
        }
        let client = self.client.clone();
        let url = self.url.clone();
        let headers = self.current_context().headers;
        let runtime = self.runtime.clone();
        let future = async move {
            let request = client.delete(url).headers(headers);
            let _ = tokio::time::timeout(DELETE_TIMEOUT, request.send()).await;
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.spawn(future);
        }));
    }

    pub(crate) fn is_running(&self) -> bool {
        !self.closed.load(Ordering::Relaxed)
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    fn ensure_open(&self) -> Result<(), McpError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(self.closed_error());
        }
        Ok(())
    }

    async fn exchange_request(
        &self,
        context: &PostContext,
        body: &str,
        id: u64,
        method: &str,
    ) -> Result<Value, McpError> {
        let response = context
            .post(body.to_owned())
            .send()
            .await
            .map_err(|error| self.io_error(format!("the HTTP request failed: {error}")))?;
        self.capture_session(method, &response)?;

        let status = response.status();
        if status == StatusCode::ACCEPTED {
            return Err(self.protocol_error(
                "the server answered a request with 202 Accepted and no JSON-RPC response",
            ));
        }
        if !status.is_success() {
            return Err(self.http_status_error(status, response).await);
        }
        match self.response_content_type(&response)?.as_str() {
            "application/json" => {
                let value = self.read_json_body(response).await?;
                self.route_json(value, id)
            }
            "text/event-stream" => self.read_event_stream(response, id).await,
            other => {
                Err(self.protocol_error(format!("unsupported response content type `{other}`")))
            }
        }
    }

    async fn exchange_notification(
        &self,
        context: &PostContext,
        body: &str,
    ) -> Result<(), McpError> {
        let response = context
            .post(body.to_owned())
            .send()
            .await
            .map_err(|error| self.io_error(format!("the HTTP request failed: {error}")))?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(self.http_status_error(response.status(), response).await)
    }

    fn route_json(&self, value: Value, expected: u64) -> Result<Value, McpError> {
        match self.route_inbound(value, expected, false)? {
            Inbound::Response(value) => Ok(value),
            Inbound::Continue => Err(self.protocol_error(format!(
                "the server's JSON response was not a response to request {expected}"
            ))),
        }
    }

    async fn read_event_stream(
        &self,
        response: reqwest::Response,
        expected: u64,
    ) -> Result<Value, McpError> {
        let mut stream = response.bytes_stream();
        let mut parser = SseParser::new(MAX_MESSAGE_BYTES);
        loop {
            match stream.next().await {
                Some(chunk) => {
                    let chunk = chunk.map_err(|error| {
                        self.io_error(format!("reading the event stream failed: {error}"))
                    })?;
                    for payload in parser
                        .push(&chunk)
                        .map_err(|message| self.protocol_error(message))?
                    {
                        if let Some(value) = self.handle_stream_event(&payload, expected)? {
                            return Ok(value);
                        }
                    }
                }
                None => {
                    if let Some(payload) = parser
                        .finish()
                        .map_err(|message| self.protocol_error(message))?
                        && let Some(value) = self.handle_stream_event(&payload, expected)?
                    {
                        return Ok(value);
                    }
                    return Err(self.protocol_error(format!(
                        "the event stream ended without a response to request {expected}"
                    )));
                }
            }
        }
    }

    fn handle_stream_event(
        &self,
        payload: &[u8],
        expected: u64,
    ) -> Result<Option<Value>, McpError> {
        if payload.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        let text = std::str::from_utf8(payload)
            .map_err(|_| self.protocol_error("an event-stream message is not UTF-8"))?;
        let value: Value = serde_json::from_str(text).map_err(|error| {
            self.protocol_error(format!(
                "an event-stream message is not valid JSON: {error}"
            ))
        })?;
        match self.route_inbound(value, expected, true)? {
            Inbound::Response(value) => Ok(Some(value)),
            Inbound::Continue => Ok(None),
        }
    }

    /// Dispatches one JSON-RPC message. Notifications are ignored, responses
    /// for another id are ignored on a stream, and a server-initiated request
    /// gets a bounded, best-effort `-32601` POST.
    fn route_inbound(
        &self,
        value: Value,
        expected: u64,
        streaming: bool,
    ) -> Result<Inbound, McpError> {
        if value.is_array() {
            return Err(self.protocol_error(
                "JSON-RPC batches are not supported; MCP 2025-06-18 removed them",
            ));
        }
        if !value.is_object() {
            return Err(self.protocol_error("a JSON-RPC message is not an object"));
        }
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            match value.get("id") {
                Some(id) if !id.is_null() => {
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!("unsupported MCP server request `{method}`")
                        }
                    });
                    spawn_post(
                        self.runtime.clone(),
                        self.current_context(),
                        response.to_string(),
                        CANCEL_TIMEOUT,
                    );
                    debug(
                        &self.server,
                        &format!("answered server request `{method}` with id {id}"),
                    );
                }
                _ => debug(&self.server, &format!("ignored notification `{method}`")),
            }
            return Ok(Inbound::Continue);
        }

        let Some(id) = response_id(&value) else {
            if streaming {
                debug(
                    &self.server,
                    "ignored a message that has neither a method nor a usable id",
                );
                return Ok(Inbound::Continue);
            }
            return Err(self.protocol_error(
                "the server's JSON response has neither a method nor a usable id",
            ));
        };
        if id != expected {
            if streaming {
                debug(
                    &self.server,
                    &format!("ignored a response for unknown request id {id}"),
                );
                return Ok(Inbound::Continue);
            }
            return Err(self.protocol_error(format!(
                "the server answered with a response for id {id}, expected {expected}"
            )));
        }
        if let Some(result) = value.get("result") {
            return Ok(Inbound::Response(result.clone()));
        }
        if let Some(error) = value.get("error") {
            return Err(rpc_error(&self.server, error));
        }
        Err(self.protocol_error(format!("response {id} has neither result nor error")))
    }

    async fn read_json_body(&self, response: reqwest::Response) -> Result<Value, McpError> {
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                self.io_error(format!("reading the JSON response failed: {error}"))
            })?;
            if body.len() + chunk.len() > MAX_MESSAGE_BYTES {
                return Err(self.protocol_error(format!(
                    "the JSON response body exceeded {MAX_MESSAGE_BYTES} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        if body.is_empty() {
            return Err(self.protocol_error("the JSON response body is empty"));
        }
        let text = std::str::from_utf8(&body)
            .map_err(|_| self.protocol_error("the JSON response body is not UTF-8"))?;
        serde_json::from_str(text).map_err(|error| {
            self.protocol_error(format!("the JSON response body is not valid JSON: {error}"))
        })
    }

    fn response_content_type(&self, response: &reqwest::Response) -> Result<String, McpError> {
        let Some(value) = response.headers().get(CONTENT_TYPE) else {
            return Err(self.protocol_error("the response has no content type"));
        };
        let value = value
            .to_str()
            .map_err(|_| self.protocol_error("the response content type is not valid ASCII"))?;
        Ok(value
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase())
    }

    fn capture_session(&self, method: &str, response: &reqwest::Response) -> Result<(), McpError> {
        if method != "initialize" {
            return Ok(());
        }
        let Some(value) = response.headers().get(SESSION_HEADER) else {
            return Ok(());
        };
        let value = value
            .to_str()
            .map_err(|_| self.protocol_error("the server's session id is not valid ASCII"))?
            .trim();
        if value.is_empty() {
            return Err(self.protocol_error("the server returned an empty session id"));
        }
        *self.session_id.lock().unwrap() = Some(value.to_owned());
        Ok(())
    }

    async fn http_status_error(&self, status: StatusCode, response: reqwest::Response) -> McpError {
        let code = status.as_u16();
        if code == 401 || code == 403 {
            return McpError::HeadersRejected {
                server: self.server.clone(),
                status: code,
            };
        }
        if code == 404 && self.session_id.lock().unwrap().is_some() {
            return McpError::SessionExpired {
                server: self.server.clone(),
            };
        }
        let body = read_error_body(response).await;
        let detail = body.split_whitespace().collect::<Vec<_>>().join(" ");
        let detail = truncate_text(&detail, MAX_ERROR_MESSAGE_BYTES);
        // A server can reflect a request header into its error body. Never
        // echo a configured value, even when the server hands it back.
        let detail = self.redact_configured_values(&detail);
        McpError::Http {
            server: self.server.clone(),
            status: code,
            message: if detail.is_empty() {
                "the server returned no details".to_owned()
            } else {
                detail
            },
        }
    }

    fn redact_configured_values(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for value in self.headers.values() {
            if let Ok(value) = value.to_str()
                && !value.is_empty()
            {
                text = text.replace(value, "[redacted]");
            }
        }
        text
    }

    /// Scrubs configured values from every string a server can influence.
    fn redact_error(&self, error: McpError) -> McpError {
        match error {
            McpError::Spawn { server, message } => McpError::Spawn {
                server,
                message: self.redact_configured_values(&message),
            },
            McpError::Io { server, message } => McpError::Io {
                server,
                message: self.redact_configured_values(&message),
            },
            McpError::Protocol { server, message } => McpError::Protocol {
                server,
                message: self.redact_configured_values(&message),
            },
            McpError::Closed { server, message } => McpError::Closed {
                server,
                message: self.redact_configured_values(&message),
            },
            McpError::Rpc {
                server,
                code,
                message,
            } => McpError::Rpc {
                server,
                code,
                message: self.redact_configured_values(&message),
            },
            McpError::Http {
                server,
                status,
                message,
            } => McpError::Http {
                server,
                status,
                message: self.redact_configured_values(&message),
            },
            McpError::Timeout { .. }
            | McpError::HeadersRejected { .. }
            | McpError::SessionExpired { .. } => error,
        }
    }

    /// Headers for a request after `initialize`. The negotiated version is
    /// set before `notifications/initialized`, so even that first follow-up
    /// carries it.
    fn current_context(&self) -> PostContext {
        let mut headers = self.headers.clone();
        if let Some(session) = self.session_id.lock().unwrap().as_deref() {
            insert_header(&mut headers, SESSION_HEADER, session);
        }
        if let Some(version) = self.protocol_version.lock().unwrap().as_deref() {
            insert_header(&mut headers, VERSION_HEADER, version);
        }
        PostContext {
            client: self.client.clone(),
            url: self.url.clone(),
            headers,
        }
    }

    fn context_without_session(&self) -> PostContext {
        PostContext {
            client: self.client.clone(),
            url: self.url.clone(),
            headers: self.headers.clone(),
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
                .unwrap_or_else(|| "the client was shut down".to_owned()),
        }
    }
}

enum Inbound {
    Response(Value),
    Continue,
}

struct CancelGuard {
    runtime: Option<Handle>,
    context: Option<PostContext>,
    server: String,
    id: u64,
    armed: bool,
}

impl CancelGuard {
    fn new(runtime: Handle, context: PostContext, server: String, id: u64) -> Self {
        Self {
            runtime: Some(runtime),
            context: Some(context),
            server,
            id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.context = None;
        self.runtime = None;
    }

    /// Sends `notifications/cancelled` now, without waiting for the guard to
    /// be dropped.
    fn cancel_now(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let (Some(runtime), Some(context)) = (self.runtime.take(), self.context.take()) else {
            return;
        };
        let body = json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {
                "requestId": self.id,
                "reason": format!("keel cancelled the call on `{}`", self.server)
            }
        })
        .to_string();
        spawn_post(runtime, context, body, CANCEL_TIMEOUT);
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.cancel_now();
    }
}

fn spawn_post(runtime: Handle, context: PostContext, body: String, timeout: Duration) {
    let future = async move {
        let _ = tokio::time::timeout(timeout, context.post(body).send()).await;
    };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.spawn(future);
    }));
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn debug(server: &str, message: &str) {
    if std::env::var_os("KEEL_MCP_DEBUG").is_some() {
        eprintln!("keel: mcp[{server}]: {message}");
    }
}

async fn read_error_body(response: reqwest::Response) -> String {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = ERROR_BODY_BYTES.saturating_sub(body.len());
        if chunk.len() >= remaining {
            body.extend_from_slice(&chunk[..remaining]);
            break;
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&body).into_owned()
}

/// A small SSE parser: `data:` fields accumulate one frame, blank lines
/// dispatch it, and every other field is ignored. Lines and frames are capped
/// at `limit` bytes before anything is buffered without bound. LF and CRLF are
/// accepted; a final line without a newline is flushed at EOF.
struct SseParser {
    line: Vec<u8>,
    data: Vec<u8>,
    limit: usize,
}

impl SseParser {
    fn new(limit: usize) -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            limit,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let mut events = Vec::new();
        let mut start = 0;
        for (index, byte) in bytes.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            self.line.extend_from_slice(&bytes[start..index]);
            if self.line.len() > self.limit {
                return Err(format!(
                    "an event-stream line exceeded {} bytes",
                    self.limit
                ));
            }
            let mut line = std::mem::take(&mut self.line);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.process_line(&line, &mut events)?;
            start = index + 1;
        }
        if start < bytes.len() {
            self.line.extend_from_slice(&bytes[start..]);
            if self.line.len() > self.limit {
                return Err(format!(
                    "an event-stream line exceeded {} bytes",
                    self.limit
                ));
            }
        }
        Ok(events)
    }

    fn finish(&mut self) -> Result<Option<Vec<u8>>, String> {
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            let mut events = Vec::new();
            self.process_line(&line, &mut events)?;
            debug_assert!(events.is_empty());
        }
        if self.data.is_empty() {
            Ok(None)
        } else {
            Ok(Some(std::mem::take(&mut self.data)))
        }
    }

    fn process_line(&mut self, line: &[u8], events: &mut Vec<Vec<u8>>) -> Result<(), String> {
        if line.is_empty() {
            if !self.data.is_empty() {
                events.push(std::mem::take(&mut self.data));
            }
            return Ok(());
        }
        if line.starts_with(b":") {
            return Ok(());
        }
        let (field, value) = match line.iter().position(|byte| *byte == b':') {
            Some(index) => (&line[..index], &line[index + 1..]),
            None => (line, &b""[..]),
        };
        if field != b"data" {
            return Ok(());
        }
        let value = value.strip_prefix(b" ").unwrap_or(value);
        if !self.data.is_empty() {
            self.data.push(b'\n');
        }
        if self.data.len() + value.len() > self.limit {
            return Err(format!(
                "an event-stream event exceeded {} bytes",
                self.limit
            ));
        }
        self.data.extend_from_slice(value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_frames_survive_chunk_boundaries() {
        let mut parser = SseParser::new(1024);
        assert!(parser.push(b"data: {\"a\":1").unwrap().is_empty());
        assert!(parser.push(b"}\n\ndata: second\n").unwrap().len() == 1);
        assert_eq!(parser.finish().unwrap().as_deref(), Some(&b"second"[..]));
    }

    #[test]
    fn sse_ignores_other_fields_and_comments() {
        let mut parser = SseParser::new(1024);
        let events = parser
            .push(b": keepalive\nevent: message\nid: 7\ndata: one\ndata:two\n\n")
            .unwrap();
        assert_eq!(events, vec![b"one\ntwo".to_vec()]);
    }

    #[test]
    fn sse_caps_a_line() {
        let mut parser = SseParser::new(8);
        let error = parser.push(b"data: 123456789\n\n").unwrap_err();
        assert!(error.contains("line"), "{error}");
    }

    #[test]
    fn sse_caps_a_frame() {
        let mut parser = SseParser::new(8);
        let error = parser.push(b"data: 1234\ndata: 56789\n\n").unwrap_err();
        assert!(error.contains("event"), "{error}");
    }
}
