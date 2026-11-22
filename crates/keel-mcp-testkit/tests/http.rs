//! Streamable HTTP tests against the scripted `FauxProvider` server.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{Harness, RunStop, Session};
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::{Auth, Client, EnvResolver, Framing, Route};
use keel_llm_types::{Compat, ProtocolId};
use keel_mcp::{McpClient, McpConfig, McpError, ServerConfig, ServerSetup};
use keel_testkit::{Chunk, FauxProvider, Turn};
use keel_tools::{PermissionGate, Tool, ToolContext, ToolLimits, ToolRegistry, Workspace};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const MAX_BODY: usize = 1024 * 1024;

fn http_config(url: &str, headers: &[(&str, &str)]) -> ServerConfig {
    ServerConfig {
        command: String::new(),
        args: Vec::new(),
        enabled: true,
        env: Vec::new(),
        parallel_safe: false,
        startup_timeout_ms: 5_000,
        url: Some(url.to_owned()),
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
    }
}

fn endpoint(provider: &FauxProvider) -> String {
    format!("{}/mcp", provider.url())
}

async fn start(provider: &FauxProvider, headers: &[(&str, &str)]) -> McpClient {
    McpClient::start(
        "remote",
        &http_config(&endpoint(provider), headers),
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap()
}

async fn connect(
    provider: &FauxProvider,
    headers: &[(&str, &str)],
    parallel_safe: bool,
) -> ServerSetup {
    let mut server = http_config(&endpoint(provider), headers);
    server.parallel_safe = parallel_safe;
    let mut servers = BTreeMap::new();
    servers.insert("remote".to_owned(), server);
    McpConfig {
        servers,
        warnings: Vec::new(),
    }
    .connect(&[], None)
    .await
}

fn init_turn(version: &str, session: Option<&str>) -> Turn {
    let turn = Turn::json(
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": version,
                "capabilities": {},
                "serverInfo": {"name": "faux", "version": "1"}
            }
        })
        .to_string(),
    );
    match session {
        Some(session) => turn.header("mcp-session-id", session),
        None => turn,
    }
}

fn accepted() -> Turn {
    Turn::text(202, "")
}

fn tools_turn(id: u64, tools: &[&str]) -> Turn {
    let tools = tools
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "description": format!("the {name} tool"),
                "inputSchema": {"type": "object", "properties": {}}
            })
        })
        .collect::<Vec<_>>();
    Turn::json(json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools}}).to_string())
}

fn call_result(id: u64, text: &str) -> Turn {
    Turn::json(
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"content": [{"type": "text", "text": text}]}
        })
        .to_string(),
    )
}

fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}

#[tokio::test]
async fn json_path_negotiates_headers_and_the_session() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", Some("session-1")),
        accepted(),
        tools_turn(2, &["echo"]),
        call_result(3, "from http"),
    ])
    .await;
    let setup = connect(
        &provider,
        &[("x-api-key", "secret-value"), ("x-team", "alpha")],
        false,
    )
    .await;
    assert!(setup.warnings.is_empty(), "{:?}", setup.warnings);
    assert_eq!(setup.connected, vec!["remote"]);
    assert_eq!(setup.names(), vec!["remote__echo"]);

    let tool = &setup.tools[0];
    assert!(!tool.parallel_safe());
    let directory = tempfile::tempdir().unwrap();
    let context = ToolContext {
        workspace: Workspace::open(directory.path()).unwrap(),
        cancel: CancellationToken::new(),
        limits: ToolLimits {
            timeout: Duration::from_secs(5),
            max_output_bytes: 64 * 1024,
            ..ToolLimits::default()
        },
        session_state: None,
    };
    let outcome = tool.execute(json!({"value": "hello"}), &context).await;
    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(outcome.text, "from http");

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 4);
    for request in &requests {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/mcp");
        assert_eq!(request.header("content-type"), Some("application/json"));
        assert_eq!(
            request.header("accept"),
            Some("application/json, text/event-stream")
        );
        assert_eq!(request.header("x-api-key"), Some("secret-value"));
        assert_eq!(request.header("x-team"), Some("alpha"));
    }

    assert_eq!(requests[0].json()["method"], "initialize");
    assert_eq!(requests[0].header("mcp-session-id"), None);
    assert_eq!(requests[0].header("mcp-protocol-version"), None);

    assert_eq!(requests[1].json()["method"], "notifications/initialized");
    assert_eq!(requests[1].header("mcp-session-id"), Some("session-1"));
    assert_eq!(
        requests[1].header("mcp-protocol-version"),
        Some("2025-06-18")
    );

    assert_eq!(requests[2].json()["method"], "tools/list");
    assert_eq!(requests[3].json()["method"], "tools/call");
    assert_eq!(requests[3].json()["params"]["name"], "echo");
    assert_eq!(requests[3].header("mcp-session-id"), Some("session-1"));
    assert_eq!(
        requests[3].header("mcp-protocol-version"),
        Some("2025-06-18")
    );
}

#[tokio::test]
async fn a_2024_11_05_server_sets_the_negotiated_version() {
    let provider = FauxProvider::start(vec![
        init_turn("2024-11-05", None),
        accepted(),
        tools_turn(2, &["echo"]),
    ])
    .await;
    let setup = connect(&provider, &[], false).await;
    assert!(setup.warnings.is_empty(), "{:?}", setup.warnings);

    let requests = provider.requests().await;
    assert_eq!(requests[0].header("mcp-protocol-version"), None);
    assert_eq!(
        requests[1].header("mcp-protocol-version"),
        Some("2024-11-05")
    );
    assert_eq!(
        requests[2].header("mcp-protocol-version"),
        Some("2024-11-05")
    );
}

#[tokio::test]
async fn sse_path_ignores_notifications_and_answers_server_requests() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", Some("session-1")),
        accepted(),
        Turn::sse(vec![Chunk::text(format!(
            "{}{}{}",
            event(json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {"progress": 1}
            })),
            event(json!({
                "jsonrpc": "2.0",
                "id": "server-1",
                "method": "roots/list",
                "params": {}
            })),
            event(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {"content": [{"type": "text", "text": "streamed"}]}
            })),
        ))]),
        accepted(),
    ])
    .await;
    let client = start(&provider, &[]).await;

    let result = client
        .call_tool("echo", json!({}), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(result.text, "streamed");

    provider.wait_for_requests(4).await;
    let requests = provider.requests().await;
    let answer = &requests[3];
    assert_eq!(answer.method, "POST");
    assert_eq!(answer.json()["id"], "server-1");
    assert_eq!(answer.json()["error"]["code"], -32601);
    assert_eq!(answer.header("mcp-session-id"), Some("session-1"));
    assert_eq!(answer.header("mcp-protocol-version"), Some("2025-06-18"));
}

#[tokio::test]
async fn an_sse_stream_without_the_response_is_a_protocol_error() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", None),
        accepted(),
        Turn::sse(vec![Chunk::text(event(json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {"progress": 1}
        })))]),
    ])
    .await;
    let client = start(&provider, &[]).await;

    let error = client
        .call_tool("echo", json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::Protocol { .. }), "{error}");
    assert!(error.to_string().contains("ended without"), "{error}");
}

#[tokio::test]
async fn unauthorized_is_a_typed_headers_error() {
    let provider = FauxProvider::start(vec![Turn::text(401, "not for you")]).await;
    let error = McpClient::start(
        "remote",
        &http_config(&endpoint(&provider), &[("authorization", "Bearer nope")]),
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, McpError::HeadersRejected { status: 401, .. }),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("rejected the request with HTTP 401"),
        "{error}"
    );
    assert!(error.to_string().contains("check the URL"), "{error}");
    assert!(!error.to_string().contains("stderr"), "{error}");
    assert!(!error.to_string().contains("Bearer nope"), "{error}");
}

#[tokio::test]
async fn forbidden_is_a_typed_headers_error() {
    let provider = FauxProvider::start(vec![Turn::text(403, "forbidden")]).await;
    let error = McpClient::start(
        "remote",
        &http_config(&endpoint(&provider), &[]),
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, McpError::HeadersRejected { status: 403, .. }),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("rejected the request with HTTP 403"),
        "{error}"
    );
}

#[tokio::test]
async fn a_404_after_initialize_reports_an_expired_session() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", Some("session-1")),
        accepted(),
        Turn::text(404, "unknown session"),
    ])
    .await;
    let client = start(&provider, &[]).await;
    let error = client
        .call_tool("echo", json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::SessionExpired { .. }), "{error}");
    assert!(error.to_string().contains("session expired"), "{error}");
    assert!(error.to_string().contains("restart"), "{error}");
}

#[tokio::test]
async fn five_hundred_and_429_name_the_status() {
    for status in [500_u16, 429] {
        let provider = FauxProvider::start(vec![
            init_turn("2025-06-18", None),
            accepted(),
            Turn::text(status, "the server is unhappy"),
        ])
        .await;
        let client = start(&provider, &[]).await;
        let error = client
            .call_tool("echo", json!({}), Duration::from_secs(5))
            .await
            .unwrap_err();
        match error {
            McpError::Http { status: got, .. } => assert_eq!(got, status),
            other => panic!("expected an HTTP error, got {other}"),
        }
        assert!(
            error.to_string().contains(&format!("HTTP {status}")),
            "{error}"
        );
        assert!(
            error.to_string().contains("the server is unhappy"),
            "{error}"
        );
        assert!(!error.to_string().contains("stderr"), "{error}");
    }
}

#[tokio::test]
async fn a_server_cannot_reflect_a_configured_header_into_an_error() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", None),
        accepted(),
        Turn::text(500, "rejected token secret-value"),
    ])
    .await;
    let client = start(&provider, &[("x-token", "secret-value")]).await;
    let error = client
        .call_tool("echo", json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("secret-value"), "{error}");
    assert!(error.to_string().contains("[redacted]"), "{error}");
    assert!(error.to_string().contains("rejected token"), "{error}");
}

#[tokio::test]
async fn a_server_cannot_reflect_a_configured_header_into_a_json_rpc_error() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", None),
        accepted(),
        Turn::json(
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "error": {
                    "code": -32000,
                    "message": "rejected token secret-value",
                    "data": {"token": "secret-value", "hint": "send a new secret-value"}
                }
            })
            .to_string(),
        ),
    ])
    .await;
    let client = start(&provider, &[("x-token", "secret-value")]).await;
    let error = client
        .call_tool("echo", json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::Rpc { .. }), "{error}");
    assert!(!error.to_string().contains("secret-value"), "{error}");
    assert!(error.to_string().contains("[redacted]"), "{error}");
    assert!(error.to_string().contains("rejected token"), "{error}");
    assert!(error.to_string().contains("(data:"), "{error}");
}

#[tokio::test]
async fn a_server_cannot_reflect_a_configured_header_into_a_tool_error() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", None),
        accepted(),
        Turn::json(
            json!({"jsonrpc": "2.0", "id": 2, "result": {"tools": ["secret-value"]}}).to_string(),
        ),
    ])
    .await;
    let client = start(&provider, &[("x-token", "secret-value")]).await;
    let error = client.list_tools(Duration::from_secs(5)).await.unwrap_err();
    assert!(matches!(error, McpError::Protocol { .. }), "{error}");
    assert!(!error.to_string().contains("secret-value"), "{error}");
    assert!(error.to_string().contains("[redacted]"), "{error}");
}

#[tokio::test]
async fn a_202_answer_to_a_request_is_a_protocol_error() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", None),
        accepted(),
        Turn::text(202, ""),
    ])
    .await;
    let client = start(&provider, &[]).await;
    let error = client
        .call_tool("echo", json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::Protocol { .. }), "{error}");
    assert!(error.to_string().contains("202 Accepted"), "{error}");
}

#[tokio::test]
async fn a_redirect_is_a_typed_error_and_is_not_followed() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", None),
        accepted(),
        Turn::json(r#"{"moved":true}"#)
            .with_status(302)
            .header("location", "/elsewhere"),
    ])
    .await;
    let client = start(&provider, &[]).await;
    let error = client
        .call_tool("echo", json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(
        matches!(error, McpError::Http { status: 302, .. }),
        "{error}"
    );
    assert!(error.to_string().contains("HTTP 302"), "{error}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        provider.request_count().await,
        3,
        "the client followed the redirect"
    );
}

#[tokio::test]
async fn a_json_body_over_one_mib_is_refused() {
    let huge = format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-06-18","capabilities":{{}},"padding":"{}"}}}}"#,
        "x".repeat(MAX_BODY)
    );
    let provider = FauxProvider::start(vec![Turn::json(huge)]).await;
    let error = McpClient::start(
        "remote",
        &http_config(&endpoint(&provider), &[]),
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, McpError::Protocol { .. }), "{error}");
    assert!(error.to_string().contains("exceeded"), "{error}");
}

#[tokio::test]
async fn an_sse_line_over_one_mib_is_refused() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", None),
        accepted(),
        Turn::sse(vec![Chunk::text(format!(
            "data: {}\n\n",
            "x".repeat(MAX_BODY + 1)
        ))]),
    ])
    .await;
    let client = start(&provider, &[]).await;
    let error = client
        .call_tool("echo", json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("line exceeded"), "{error}");
}

#[tokio::test]
async fn an_sse_frame_over_one_mib_is_refused() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", None),
        accepted(),
        Turn::sse(vec![Chunk::text(format!(
            "data: {}\ndata: {}\n\n",
            "x".repeat(600 * 1024),
            "y".repeat(600 * 1024)
        ))]),
    ])
    .await;
    let client = start(&provider, &[]).await;
    let error = client
        .call_tool("echo", json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("event exceeded"), "{error}");
}

#[tokio::test]
async fn a_json_rpc_batch_is_refused() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", None),
        accepted(),
        Turn::json("[]"),
    ])
    .await;
    let client = start(&provider, &[]).await;
    let error = client
        .call_tool("echo", json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("batch"),
        "unexpected batch error: {error}"
    );
    assert!(matches!(error, McpError::Protocol { .. }), "{error}");
}

#[tokio::test]
async fn a_batch_inside_an_sse_frame_is_refused() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", None),
        accepted(),
        Turn::sse(vec![Chunk::text("data: []\n\n")]),
    ])
    .await;
    let client = start(&provider, &[]).await;
    let error = client
        .call_tool("echo", json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("batch"), "{error}");
}

#[tokio::test]
async fn cancellation_sends_notifications_cancelled_and_returns_promptly() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", Some("session-1")),
        accepted(),
        Turn::stalled(),
        accepted(),
        call_result(3, "after cancellation"),
    ])
    .await;
    let client = start(&provider, &[]).await;

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_millis(150),
        client.call_tool("echo", json!({}), Duration::from_secs(30)),
    )
    .await;
    assert!(result.is_err(), "the stalled call unexpectedly finished");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cancellation took {:?}",
        started.elapsed()
    );

    provider.wait_for_requests(4).await;
    let requests = provider.requests().await;
    let cancel = &requests[3];
    let body = cancel.json();
    assert_eq!(body["method"], "notifications/cancelled");
    assert_eq!(body["params"]["requestId"], 2);
    assert!(
        body["params"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("keel cancelled")),
        "{body}"
    );
    assert_eq!(cancel.header("mcp-session-id"), Some("session-1"));

    let next = client
        .call_tool("echo", json!({}), Duration::from_secs(5))
        .await
        .expect("the client was corrupted by cancellation");
    assert_eq!(next.text, "after cancellation");
}

#[tokio::test]
async fn a_timeout_names_the_timeout() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", None),
        accepted(),
        Turn::stalled(),
        accepted(),
    ])
    .await;
    let client = start(&provider, &[]).await;
    let error = client
        .call_tool("echo", json!({}), Duration::from_millis(50))
        .await
        .unwrap_err();
    match error {
        McpError::Timeout { after, .. } => {
            assert_eq!(after, Duration::from_millis(50));
        }
        other => panic!("expected a timeout, got {other}"),
    }
    assert!(
        error.to_string().contains("did not answer within"),
        "{error}"
    );
}

#[tokio::test]
async fn shutdown_sends_delete_with_the_session_id() {
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", Some("session-1")),
        accepted(),
        Turn::text(204, ""),
    ])
    .await;
    let client = start(&provider, &[]).await;
    client.shutdown().await;

    provider.wait_for_requests(3).await;
    let requests = provider.requests().await;
    let delete = &requests[2];
    assert_eq!(delete.method, "DELETE");
    assert_eq!(delete.path, "/mcp");
    assert_eq!(delete.header("mcp-session-id"), Some("session-1"));
    assert_eq!(delete.header("mcp-protocol-version"), Some("2025-06-18"));
    assert!(!client.is_running().await);
    assert!(matches!(
        client
            .call_tool("echo", json!({}), Duration::from_secs(1))
            .await,
        Err(McpError::Closed { .. })
    ));
}

#[tokio::test]
async fn parallel_calls_overlap_without_a_writer() {
    let (gated, gate) = Chunk::gated(event(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": {"content": [{"type": "text", "text": "first"}]}
    })));
    let provider = FauxProvider::start(vec![
        init_turn("2025-06-18", None),
        accepted(),
        Turn::sse(vec![gated]),
        call_result(3, "second"),
    ])
    .await;
    let client = Arc::new(start(&provider, &[]).await);

    let first = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .call_tool("echo", json!({}), Duration::from_secs(5))
                .await
        })
    };
    provider.wait_for_requests(3).await;
    let second = match tokio::time::timeout(
        Duration::from_secs(1),
        client.call_tool("slow", json!({}), Duration::from_secs(5)),
    )
    .await
    {
        Ok(result) => result.unwrap(),
        Err(_) => {
            let _ = gate.send(());
            panic!("the second HTTP call did not overlap the gated one");
        }
    };
    assert_eq!(second.text, "second");
    assert!(!first.is_finished(), "the gated call finished early");

    gate.send(()).unwrap();
    let first = first.await.unwrap().unwrap();
    assert_eq!(first.text, "first");
    assert_eq!(provider.request_count().await, 4);
}

// ---------------------------------------------------------------------------
// Harness-level coverage: registration, permissions, batching and overlap.
// ---------------------------------------------------------------------------

fn route(endpoint: String) -> Route {
    Route {
        provider: "test".into(),
        model: "gpt-4.1".into(),
        endpoint,
        protocol: Arc::new(OpenAiChat::new(Compat::for_protocol(
            ProtocolId::OpenAiChat,
        ))),
        auth: Auth::None,
        framing: Framing::Sse,
        headers: Vec::new(),
        context_window: 0,
        max_output_tokens: 0,
    }
}

fn tool_turn(id: &str, name: &str, arguments: &str) -> Turn {
    let arguments = serde_json::to_string(arguments).unwrap();
    Turn::sse(vec![
        Chunk::text(format!(
            "data: {{\"model\":\"gpt-4.1\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"{id}\",\"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":{arguments}}}}}]}},\"finish_reason\":null}}]}}\n\n"
        )),
        Chunk::text(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ),
        Chunk::text("data: [DONE]\n\n"),
    ])
}

fn tool_turn_many(calls: &[(&str, &str, &str)]) -> Turn {
    let calls = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            let arguments = serde_json::to_string(arguments).unwrap();
            format!(
                "{{\"index\":{index},\"id\":\"{id}\",\"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":{arguments}}}}}"
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    Turn::sse(vec![
        Chunk::text(format!(
            "data: {{\"model\":\"gpt-4.1\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{calls}]}},\"finish_reason\":null}}]}}\n\n"
        )),
        Chunk::text(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ),
        Chunk::text("data: [DONE]\n\n"),
    ])
}

fn answer_turn(text: &str) -> Turn {
    let text = serde_json::to_string(text).unwrap();
    Turn::sse(vec![
        Chunk::text(format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{text}}},\"finish_reason\":\"stop\"}}]}}\n\n"
        )),
        Chunk::text("data: [DONE]\n\n"),
    ])
}

fn build(
    endpoint: String,
    registry: Arc<ToolRegistry>,
    permissions: PermissionGate,
    workspace: &std::path::Path,
) -> Harness {
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session = Session::new(route(endpoint), client);
    Harness::new(session, registry, Workspace::open(workspace).unwrap())
        .with_permissions(permissions)
}

fn result_rows(harness: &Harness) -> Vec<(String, String, bool)> {
    harness
        .session()
        .history()
        .iter()
        .find(|message| message.tool_results().next().is_some())
        .expect("the run recorded a tool result message")
        .tool_results()
        .map(|(id, output, is_error)| (id.as_str().to_owned(), output.to_owned(), is_error))
        .collect()
}

async fn batch(parallel_safe: bool) -> (bool, RunStop, Vec<(String, String, bool)>) {
    let (gated, gate) = Chunk::gated(event(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {"content": [{"type": "text", "text": "first"}]}
    })));
    let mcp = FauxProvider::start(vec![
        init_turn("2025-06-18", None),
        accepted(),
        tools_turn(2, &["echo", "slow"]),
        Turn::sse(vec![gated]),
        call_result(4, "second"),
    ])
    .await;
    let llm = FauxProvider::start(vec![
        tool_turn_many(&[
            ("call_1", "remote__echo", "{}"),
            ("call_2", "remote__slow", "{}"),
        ]),
        answer_turn("done"),
    ])
    .await;
    let setup = connect(&mcp, &[], parallel_safe).await;
    assert!(setup.warnings.is_empty(), "{:?}", setup.warnings);
    let mut registry = ToolRegistry::new();
    for tool in setup.tools {
        registry.register(tool);
    }
    let registry = Arc::new(registry);
    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        llm.url().to_owned(),
        registry,
        PermissionGate::AllowAll,
        directory.path(),
    );

    let task = tokio::spawn(async move {
        let outcome = harness.run("go", |_| {}).await.unwrap();
        (outcome, harness)
    });

    mcp.wait_for_requests(4).await;
    let overlapped = tokio::time::timeout(Duration::from_secs(1), mcp.wait_for_requests(5))
        .await
        .is_ok();
    gate.send(()).unwrap();

    let (outcome, harness) = task.await.unwrap();
    (overlapped, outcome.stop, result_rows(&harness))
}

#[tokio::test]
async fn parallel_safe_http_tools_overlap_and_unsafe_tools_do_not() {
    let (overlapped, stop, rows) = batch(true).await;
    assert!(overlapped, "parallel-safe HTTP calls did not overlap");
    assert_eq!(stop, RunStop::EndTurn);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, "first");
    assert_eq!(rows[1].1, "second");

    let (overlapped, stop, rows) = batch(false).await;
    assert!(!overlapped, "unsafe HTTP calls overlapped");
    assert_eq!(stop, RunStop::EndTurn);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, "first");
    assert_eq!(rows[1].1, "second");
}

#[tokio::test]
async fn a_denied_http_tool_never_reaches_the_server() {
    let mcp = FauxProvider::start(vec![
        init_turn("2025-06-18", None),
        accepted(),
        tools_turn(2, &["echo"]),
        call_result(3, "called"),
    ])
    .await;
    let llm = FauxProvider::start(vec![
        tool_turn("call_1", "remote__echo", r#"{"value":"hello"}"#),
        answer_turn("denied"),
        tool_turn("call_1", "remote__echo", r#"{"value":"hello"}"#),
        answer_turn("done"),
    ])
    .await;
    let setup = connect(&mcp, &[], false).await;
    assert!(setup.warnings.is_empty(), "{:?}", setup.warnings);
    assert_eq!(setup.names(), vec!["remote__echo"]);
    let mut registry = ToolRegistry::new();
    for tool in setup.tools {
        registry.register(tool);
    }
    let registry = Arc::new(registry);
    let directory = tempfile::tempdir().unwrap();

    let mut denied = build(
        llm.url().to_owned(),
        registry.clone(),
        PermissionGate::DenyAll,
        directory.path(),
    );
    denied.run("go", |_| {}).await.unwrap();
    let rows = result_rows(&denied);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].1.contains("permission denied"), "{}", rows[0].1);
    assert!(rows[0].2);
    assert_eq!(
        mcp.request_count().await,
        3,
        "a denied tool reached the HTTP server"
    );

    let mut allowed = build(
        llm.url().to_owned(),
        registry,
        PermissionGate::AllowAll,
        directory.path(),
    );
    allowed.run("go", |_| {}).await.unwrap();
    let rows = result_rows(&allowed);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "called");
    assert!(!rows[0].2);
    mcp.wait_for_requests(4).await;
    assert_eq!(mcp.requests().await[3].json()["params"]["name"], "echo");
}
