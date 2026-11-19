//! Direct MCP stdio tests against the fixture server.

use std::collections::BTreeMap;
use std::time::Duration;

use keel_mcp::{McpClient, McpConfig, McpError, ServerConfig};

fn server_config(mode: &str, marker: Option<&str>) -> ServerConfig {
    let mut args = vec![mode.to_owned()];
    if let Some(marker) = marker {
        args.push(marker.to_owned());
    }
    ServerConfig {
        command: env!("CARGO_BIN_EXE_mcp_fixture").to_owned(),
        args,
        enabled: true,
        env: Vec::new(),
        parallel_safe: false,
        startup_timeout_ms: 5_000,
    }
}

fn config(mode: &str) -> McpConfig {
    let mut servers = BTreeMap::new();
    servers.insert("fixture".to_owned(), server_config(mode, None));
    McpConfig {
        servers,
        warnings: Vec::new(),
    }
}

async fn start(mode: &str) -> McpClient {
    McpClient::start(
        "fixture",
        &server_config(mode, None),
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn initializes_and_lists_tools() {
    let client = start("normal").await;
    assert_eq!(client.protocol_version(), "2025-06-18");
    let tools = client.list_tools(Duration::from_secs(5)).await.unwrap();
    let names = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "echo",
            "progress",
            "slow",
            "large",
            "fail",
            "malformed",
            "oversized",
            "out_of_order",
            "string_id",
            "ping",
            "roots",
            "crash_mid",
            "hang"
        ]
    );
}

#[tokio::test]
async fn calls_a_tool_and_returns_its_text() {
    let client = start("normal").await;
    let result = client
        .call_tool(
            "echo",
            serde_json::json!({"value": "hi"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.text, "hello from the fixture");
}

#[tokio::test]
async fn ignores_progress_notifications_while_waiting_for_a_result() {
    let client = start("normal").await;
    let result = client
        .call_tool(
            "progress",
            serde_json::json!({"value": "hi"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(result.text, "progress finished");
}

#[tokio::test]
async fn reports_a_tool_error_as_a_tool_result() {
    let client = start("normal").await;
    let result = client
        .call_tool("fail", serde_json::json!({}), Duration::from_secs(5))
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(result.text, "fixture failure");
}

#[tokio::test]
async fn rejects_malformed_server_output() {
    let client = start("normal").await;
    let error = client
        .call_tool("malformed", serde_json::json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    match error {
        McpError::Closed { message, .. } => {
            assert!(message.contains("invalid JSON"), "{message}");
        }
        other => panic!("expected a closed-server error, got {other}"),
    }
    assert!(
        !client.is_running().await,
        "malformed output left the child running"
    );
}

#[tokio::test]
async fn rejects_an_oversized_json_line_without_buffering_it_all() {
    let client = start("normal").await;
    let error = client
        .call_tool("oversized", serde_json::json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    match error {
        McpError::Closed { message, .. } => {
            assert!(message.contains("exceeded"), "{message}");
        }
        other => panic!("expected a closed-server error, got {other}"),
    }
    assert!(
        !client.is_running().await,
        "the oversized line left the child running"
    );
}

#[tokio::test]
async fn a_timeout_does_not_corrupt_the_next_call() {
    let client = start("normal").await;
    let error = client
        .call_tool("slow", serde_json::json!({}), Duration::from_millis(50))
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::Timeout { .. }), "{error}");

    let result = client
        .call_tool("echo", serde_json::json!({}), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(result.text, "hello from the fixture");
}

#[tokio::test]
async fn a_crash_mid_call_closes_the_client_and_reaps_the_child() {
    let client = start("normal").await;
    let error = client
        .call_tool("crash_mid", serde_json::json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::Closed { .. }), "{error}");
    assert!(
        !client.is_running().await,
        "the crashed child was left behind"
    );

    let error = client
        .call_tool("echo", serde_json::json!({}), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::Closed { .. }), "{error}");
}

#[tokio::test]
async fn accepts_crlf_and_blank_output_lines() {
    let client = start("crlf").await;
    let result = client
        .call_tool("echo", serde_json::json!({}), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(result.text, "hello from the fixture");

    let client = start("blank").await;
    let result = client
        .call_tool("echo", serde_json::json!({}), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(result.text, "hello from the fixture");
}

#[tokio::test]
async fn accepts_string_ids_and_ignores_a_bogus_response() {
    let client = start("normal").await;
    let result = client
        .call_tool("string_id", serde_json::json!({}), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(result.text, "string id result");

    let result = client
        .call_tool(
            "out_of_order",
            serde_json::json!({}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(result.text, "out of order result");
}

#[tokio::test]
async fn answers_ping_and_rejects_unknown_server_requests() {
    let client = start("normal").await;
    let result = client
        .call_tool("ping", serde_json::json!({}), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(result.text, "pong");

    let result = client
        .call_tool("roots", serde_json::json!({}), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(result.text, "method not found");
}

#[tokio::test]
async fn follows_tools_list_pages() {
    let client = start("paged").await;
    let tools = client.list_tools(Duration::from_secs(5)).await.unwrap();
    let names = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["echo", "slow"]);
}

#[tokio::test]
async fn a_crashing_server_fails_initialize() {
    let error = McpClient::start(
        "fixture",
        &server_config("crash", None),
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            McpError::Closed { .. } | McpError::Io { .. } | McpError::Timeout { .. }
        ),
        "{error}"
    );
    assert!(
        error.to_string().contains("fixture crashed intentionally"),
        "stderr tail missing from {error}"
    );
}

#[tokio::test]
async fn non_utf8_output_closes_the_server_with_a_specific_error() {
    let error = McpClient::start(
        "fixture",
        &server_config("binary", None),
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("not UTF-8"), "{error}");
}

#[tokio::test]
async fn bounds_server_controlled_metadata() {
    let setup = config("metadata").connect(&[], None).await;
    assert!(!setup.names().contains(&"fixture__bad_schema"));
    assert!(setup.names().contains(&"fixture__long_desc"));
    assert!(
        setup
            .warnings
            .iter()
            .any(|warning| warning.contains("inputSchema")),
        "{:?}",
        setup.warnings
    );
    assert!(
        setup
            .warnings
            .iter()
            .any(|warning| warning.contains("truncated")),
        "{:?}",
        setup.warnings
    );
}

#[tokio::test]
async fn a_missing_server_is_a_warning_and_loads_no_tools() {
    let mut config = config("normal");
    config.servers.get_mut("fixture").unwrap().command = "/definitely/not/a/server".to_owned();
    let setup = config.connect(&[], None).await;
    assert!(setup.tools.is_empty());
    assert!(
        setup
            .warnings
            .iter()
            .any(|warning| warning.contains("did not start")),
        "{:?}",
        setup.warnings
    );
}

#[tokio::test]
async fn a_crashing_server_is_a_warning_and_loads_no_tools() {
    let setup = config("crash").connect(&[], None).await;
    assert!(setup.tools.is_empty());
    assert_eq!(setup.connected, Vec::<String>::new());
    assert!(
        setup
            .warnings
            .iter()
            .any(|warning| warning.contains("did not start")),
        "{:?}",
        setup.warnings
    );
}

#[tokio::test]
async fn a_reserved_name_is_skipped_with_a_warning() {
    let setup = config("normal")
        .connect(&["fixture__echo".to_owned()], None)
        .await;
    assert!(!setup.names().contains(&"fixture__echo"));
    assert!(
        setup
            .warnings
            .iter()
            .any(|warning| warning.contains("already registered")),
        "{:?}",
        setup.warnings
    );
}
