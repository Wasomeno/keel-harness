//! MCP tools through `Harness`: the same invariants as built-ins, with a real
//! fixture subprocess on the other side of stdio.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use keel_core::{Harness, RunEvent, RunStop, Session};
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::{Auth, Client, EnvResolver, Framing, Route};
use keel_llm_types::{Compat, ProtocolId};
use keel_mcp::{McpConfig, ServerConfig, ServerSetup};
use keel_testkit::{Chunk, FauxProvider, Turn};
use keel_tools::{
    PermissionGate, Tool, ToolContext, ToolFuture, ToolLimits, ToolOutcome, ToolRegistry,
    Workspace, tool_spec,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

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

fn server_config(mode: &str, marker: Option<&Path>) -> ServerConfig {
    let mut args = vec![mode.to_owned()];
    if let Some(marker) = marker {
        args.push(marker.display().to_string());
    }
    ServerConfig {
        command: env!("CARGO_BIN_EXE_mcp_fixture").to_owned(),
        args,
        enabled: true,
        env: Vec::new(),
        parallel_safe: false,
        startup_timeout_ms: 5_000,
        url: None,
        headers: BTreeMap::new(),
    }
}

async fn setup(mode: &str, marker: Option<&Path>) -> ServerSetup {
    let mut servers = BTreeMap::new();
    servers.insert("fixture".to_owned(), server_config(mode, marker));
    let setup = McpConfig {
        servers,
        warnings: Vec::new(),
    }
    .connect(&[], None)
    .await;
    assert!(
        setup.warnings.is_empty(),
        "unexpected MCP warnings: {:?}",
        setup.warnings
    );
    setup
}

fn registry(setup: ServerSetup) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for tool in setup.tools {
        registry.register(tool);
    }
    registry
}

fn build(
    endpoint: String,
    registry: ToolRegistry,
    permissions: PermissionGate,
    workspace: &Path,
) -> Harness {
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session = Session::new(route(endpoint), client);
    Harness::new(
        session,
        Arc::new(registry),
        Workspace::open(workspace).unwrap(),
    )
    .with_permissions(permissions)
}

struct ProbeTool {
    delay: Duration,
    finished_at: Arc<Mutex<Option<u128>>>,
}

impl Tool for ProbeTool {
    fn name(&self) -> &str {
        "probe"
    }

    fn spec(&self) -> keel_llm_types::ToolSpec {
        tool_spec(
            "probe",
            "record when it finishes",
            serde_json::json!({"type": "object"}),
        )
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn execute<'a>(&'a self, _arguments: Value, _context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            *self.finished_at.lock().unwrap() = Some(now_nanos());
            ToolOutcome::ok("probe finished")
        })
    }
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn marker_time(path: &Path, prefix: &str) -> u128 {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .and_then(|rest| rest.split_whitespace().last())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("marker has no `{prefix}` line: {path:?}"))
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

#[tokio::test]
async fn an_mcp_tool_runs_through_the_harness() {
    let provider = FauxProvider::start(vec![
        tool_turn(
            "call_1",
            "fixture__echo",
            r#"{"value":"hello from the model"}"#,
        ),
        answer_turn("done"),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        provider.url().to_owned(),
        registry(setup("normal", None).await),
        PermissionGate::AllowAll,
        directory.path(),
    );

    let outcome = harness.run("go", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(outcome.text(), "done");
    let rows = result_rows(&harness);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "hello from the fixture");
    assert!(!rows[0].2);

    let requests = provider.requests().await;
    let request = requests[0].json();
    let advertised = request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(
        advertised.iter().any(|name| name == "fixture__echo"),
        "{advertised:?}"
    );
}

#[tokio::test]
async fn a_large_mcp_result_is_truncated_by_the_harness() {
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "fixture__large", "{}"),
        answer_turn("done"),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        provider.url().to_owned(),
        registry(setup("normal", None).await),
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_limits(ToolLimits {
        timeout: Duration::from_secs(5),
        max_output_bytes: 1024,
        ..ToolLimits::default()
    });

    harness.run("go", |_| {}).await.unwrap();

    let rows = result_rows(&harness);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].1.starts_with("START"), "{}", rows[0].1);
    assert!(rows[0].1.ends_with("END"), "{}", rows[0].1);
    assert!(rows[0].1.contains("bytes truncated"), "{}", rows[0].1);
    assert!(rows[0].1.len() < 1500, "{}", rows[0].1.len());
}

#[tokio::test]
async fn an_mcp_tool_error_is_a_tool_result_not_a_harness_error() {
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "fixture__fail", "{}"),
        answer_turn("done"),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        provider.url().to_owned(),
        registry(setup("normal", None).await),
        PermissionGate::AllowAll,
        directory.path(),
    );

    let outcome = harness.run("go", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    let rows = result_rows(&harness);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "fixture failure");
    assert!(rows[0].2);
}

#[tokio::test]
async fn a_denied_mcp_call_is_one_error_result_and_never_reaches_the_server() {
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "fixture__echo", r#"{"value":"nope"}"#),
        answer_turn("done"),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("calls");
    let mut harness = build(
        provider.url().to_owned(),
        registry(setup("normal", Some(&marker)).await),
        PermissionGate::DenyAll,
        directory.path(),
    );

    harness.run("go", |_| {}).await.unwrap();

    let rows = result_rows(&harness);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].1.contains("permission denied"), "{}", rows[0].1);
    assert!(
        std::fs::read_to_string(&marker).is_err(),
        "the denied call reached the MCP server"
    );
}

#[tokio::test]
async fn an_unknown_mcp_tool_is_one_error_result() {
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "fixture__missing", "{}"),
        answer_turn("done"),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        provider.url().to_owned(),
        registry(setup("normal", None).await),
        PermissionGate::AllowAll,
        directory.path(),
    );

    harness.run("go", |_| {}).await.unwrap();

    let rows = result_rows(&harness);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].1.contains("unknown tool"), "{}", rows[0].1);
    assert!(rows[0].2);
}

#[tokio::test]
async fn a_timed_out_mcp_call_is_one_error_result() {
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "fixture__slow", "{}"),
        answer_turn("done"),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        provider.url().to_owned(),
        registry(setup("normal", None).await),
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_limits(ToolLimits {
        timeout: Duration::from_millis(50),
        max_output_bytes: 64 * 1024,
        ..ToolLimits::default()
    });

    harness.run("go", |_| {}).await.unwrap();

    let rows = result_rows(&harness);
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].1.contains("timed out") || rows[0].1.contains("did not answer"),
        "{}",
        rows[0].1
    );
    assert!(rows[0].2);
}

#[tokio::test]
async fn a_cancelled_mcp_call_is_one_result() {
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "fixture__hang", "{}"),
        answer_turn("never"),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let mut harness = build(
        provider.url().to_owned(),
        registry(setup("normal", None).await),
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_cancellation(cancel.clone());

    let (started, started_rx) = tokio::sync::oneshot::channel();
    let mut started = Some(started);
    let task = tokio::spawn(async move {
        let outcome = harness
            .run("go", move |event| {
                if let RunEvent::ToolStarted { name, .. } = event
                    && name == "fixture__hang"
                    && let Some(started) = started.take()
                {
                    let _ = started.send(());
                }
            })
            .await;
        (outcome, harness)
    });
    started_rx.await.unwrap();
    cancel.cancel();
    let (outcome, harness) = task.await.unwrap();
    let outcome = outcome.unwrap();

    assert_eq!(outcome.stop, RunStop::Cancelled);
    let rows = result_rows(&harness);
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].1.contains("cancel"),
        "unexpected cancellation wording: {}",
        rows[0].1
    );
    assert!(rows[0].2);
}

#[tokio::test]
async fn malformed_mcp_output_is_one_error_result() {
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "fixture__malformed", "{}"),
        answer_turn("done"),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        provider.url().to_owned(),
        registry(setup("normal", None).await),
        PermissionGate::AllowAll,
        directory.path(),
    );

    harness.run("go", |_| {}).await.unwrap();

    let rows = result_rows(&harness);
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].1.contains("invalid JSON") || rows[0].1.contains("closed"),
        "{}",
        rows[0].1
    );
    assert!(rows[0].2);
}

#[tokio::test]
async fn mcp_tools_are_unsafe_barriers_by_default() {
    let setup = setup("normal", None).await;
    let registry = registry(setup);
    assert!(!registry.get("fixture__echo").unwrap().parallel_safe());
}

#[tokio::test]
async fn an_mcp_server_can_opt_into_parallel_safety() {
    let mut servers = BTreeMap::new();
    let mut server = server_config("normal", None);
    server.parallel_safe = true;
    servers.insert("fixture".to_owned(), server);
    let setup = McpConfig {
        servers,
        warnings: Vec::new(),
    }
    .connect(&[], None)
    .await;
    let registry = registry(setup);
    assert!(registry.get("fixture__echo").unwrap().parallel_safe());
}

#[tokio::test]
async fn an_unsafe_mcp_tool_splits_the_batch_after_a_safe_call() {
    let provider = FauxProvider::start(vec![
        tool_turn_many(&[("call_1", "probe", "{}"), ("call_2", "fixture__slow", "{}")]),
        answer_turn("done"),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("calls");
    let finished_at = Arc::new(Mutex::new(None));
    let mut registry = registry(setup("normal", Some(&marker)).await);
    registry.register(ProbeTool {
        delay: Duration::from_millis(150),
        finished_at: finished_at.clone(),
    });
    let mut harness = build(
        provider.url().to_owned(),
        registry,
        PermissionGate::AllowAll,
        directory.path(),
    );

    let outcome = harness.run("go", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    let probe_finished = finished_at.lock().unwrap().expect("probe finished");
    let slow_started = marker_time(&marker, "start slow ");
    assert!(
        slow_started >= probe_finished,
        "the unsafe MCP call overlapped the safe call: slow started at {slow_started}, probe ended at {probe_finished}"
    );
}
