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
    McpConfig { servers }
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
