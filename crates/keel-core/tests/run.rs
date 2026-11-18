use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use keel_core::{Harness, RunEvent, RunStop, Session};
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::{Auth, Client, EnvResolver, Framing, Route};
use keel_llm_types::{Compat, Content, ProtocolId, ToolCallId, ToolSpec};
use keel_testkit::{Chunk, FauxProvider, Turn};
use keel_tools::{
    PermissionGate, Tool, ToolContext, ToolFuture, ToolOutcome, ToolRegistry, Workspace, tool_spec,
};
use serde_json::Value;

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

struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            "echo",
            "echo the value",
            serde_json::json!({"type": "object"}),
        )
    }

    fn execute<'a>(&'a self, arguments: Value, _context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            ToolOutcome::ok(format!(
                "echo:{}",
                arguments.get("value").and_then(Value::as_str).unwrap_or("")
            ))
        })
    }
}

struct SlowTool;

impl Tool for SlowTool {
    fn name(&self) -> &str {
        "slow"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec("slow", "sleep", serde_json::json!({"type": "object"}))
    }

    fn execute<'a>(&'a self, _arguments: Value, _context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            ToolOutcome::ok("late")
        })
    }
}

struct PanicTool;

impl Tool for PanicTool {
    fn name(&self) -> &str {
        "panic"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            "panic",
            "must not run",
            serde_json::json!({"type": "object"}),
        )
    }

    fn execute<'a>(&'a self, _arguments: Value, _context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move { panic!("a denied tool was executed") })
    }
}

#[tokio::test]
async fn executes_a_tool_and_replays_its_output() {
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "echo", r#"{"value":"hi"}"#),
        answer_turn("done"),
    ])
    .await;

    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool);
    let mut harness = build(
        provider.url().to_owned(),
        registry,
        PermissionGate::AllowAll,
        directory.path(),
    );

    let mut started = Vec::new();
    let outcome = harness
        .run("go", |event| {
            if let RunEvent::ToolStarted { name, .. } = event {
                started.push(name.to_owned());
            }
        })
        .await
        .unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(outcome.turns, 2);
    assert_eq!(outcome.text(), "done");
    assert_eq!(started, vec!["echo"]);

    let requests = provider.requests().await;
    let body = requests[1].json();
    let tool_message = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("a tool result was replayed");
    assert_eq!(tool_message["tool_call_id"], "call_1");
    assert_eq!(tool_message["content"], "echo:hi");
}

#[tokio::test]
async fn a_denied_tool_returns_an_error_result_without_running() {
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "panic", "{}"),
        answer_turn("done"),
    ])
    .await;

    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(PanicTool);
    let mut harness = build(
        provider.url().to_owned(),
        registry,
        PermissionGate::DenyAll,
        directory.path(),
    );

    let outcome = harness.run("go", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);

    let requests = provider.requests().await;
    let body = requests[1].json();
    let tool_message = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("a tool result was replayed");
    assert!(
        tool_message["content"]
            .as_str()
            .unwrap()
            .contains("permission denied")
    );
}

#[tokio::test]
async fn an_unknown_tool_returns_an_error_result() {
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "missing", "{}"),
        answer_turn("done"),
    ])
    .await;

    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        provider.url().to_owned(),
        ToolRegistry::new(),
        PermissionGate::AllowAll,
        directory.path(),
    );

    let outcome = harness.run("go", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);

    let requests = provider.requests().await;
    let body = requests[1].json();
    let tool_message = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    assert!(
        tool_message["content"]
            .as_str()
            .unwrap()
            .contains("unknown tool")
    );
}

#[tokio::test]
async fn the_turn_budget_stops_a_model_that_keeps_calling_tools() {
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "echo", r#"{"value":"a"}"#),
        tool_turn("call_2", "echo", r#"{"value":"b"}"#),
        tool_turn("call_3", "echo", r#"{"value":"c"}"#),
    ])
    .await;

    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool);
    let mut harness = build(
        provider.url().to_owned(),
        registry,
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_max_turns(2);

    let outcome = harness.run("go", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::MaxTurns);
    assert_eq!(outcome.turns, 2);
    assert_eq!(provider.request_count().await, 2);
}

#[tokio::test]
async fn a_cancelled_harness_never_calls_the_model() {
    let directory = tempfile::tempdir().unwrap();
    let harness = build(
        "http://127.0.0.1:1/v1/chat/completions".to_owned(),
        ToolRegistry::new(),
        PermissionGate::AllowAll,
        directory.path(),
    );
    let cancel = harness.cancellation_token();
    cancel.cancel();
    let mut harness = harness.with_cancellation(cancel);

    let outcome = harness.run("go", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::Cancelled);
    assert_eq!(outcome.turns, 0);
}

#[tokio::test]
async fn cancellation_during_a_tool_leaves_a_complete_transcript() {
    let provider =
        FauxProvider::start(vec![tool_turn("call_1", "slow", "{}"), answer_turn("done")]).await;

    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(SlowTool);
    let mut harness = build(
        provider.url().to_owned(),
        registry,
        PermissionGate::AllowAll,
        directory.path(),
    );
    let cancel = harness.cancellation_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });

    let outcome = harness.run("go", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::Cancelled);
    assert_eq!(outcome.turns, 1);
    assert_eq!(harness.session().history().len(), 3);

    let last = harness.session().history().last().unwrap();
    match &last.parts[0].content {
        Content::ToolResult {
            call_id,
            output,
            is_error,
        } => {
            assert_eq!(*call_id, ToolCallId::new("call_1"));
            assert_eq!(output, "cancelled");
            assert!(is_error);
        }
        other => panic!("expected a tool result, got {other:?}"),
    }
}

#[tokio::test]
async fn a_timed_out_tool_returns_an_error_result_and_the_loop_continues() {
    let provider =
        FauxProvider::start(vec![tool_turn("call_1", "slow", "{}"), answer_turn("done")]).await;

    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(SlowTool);
    let mut harness = build(
        provider.url().to_owned(),
        registry,
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_limits(keel_core::short_limits(Duration::from_millis(100)));

    let outcome = harness.run("go", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);

    let requests = provider.requests().await;
    let body = requests[1].json();
    let tool_message = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    assert!(
        tool_message["content"]
            .as_str()
            .unwrap()
            .contains("timed out")
    );
}
