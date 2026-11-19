use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use keel_core::{
    CompactionReason, Harness, RunEvent, RunStop, SUMMARY_PREFIX, Session, SessionLog, SessionMeta,
};
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::{Auth, Client, EnvResolver, Framing, RetryPolicy, Route};
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

fn tool_turn_with_usage(
    id: &str,
    name: &str,
    arguments: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Turn {
    let arguments = serde_json::to_string(arguments).unwrap();
    Turn::sse(vec![
        Chunk::text(format!(
            "data: {{\"model\":\"gpt-4.1\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"{id}\",\"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":{arguments}}}}}]}},\"finish_reason\":null}}]}}\n\n"
        )),
        Chunk::text(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ),
        Chunk::text(format!(
            "data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{input_tokens},\"completion_tokens\":{output_tokens},\"total_tokens\":{}}}}}\n\n",
            input_tokens + output_tokens
        )),
        Chunk::text("data: [DONE]\n\n"),
    ])
}

fn answer_turn_with_usage(text: &str, input: u64, output: u64) -> Turn {
    let text = serde_json::to_string(text).unwrap();
    Turn::sse(vec![
        Chunk::text(format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{text}}},\"finish_reason\":null}}]}}\n\n"
        )),
        Chunk::text(format!(
            "data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{input},\"completion_tokens\":{output},\"total_tokens\":{}}}}}\n\n",
            input + output
        )),
        Chunk::text(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ),
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

struct TimedTool {
    name: &'static str,
    delay: Duration,
    parallel: bool,
}

impl Tool for TimedTool {
    fn name(&self) -> &str {
        self.name
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            self.name,
            "echo after a delay",
            serde_json::json!({"type": "object"}),
        )
    }

    fn parallel_safe(&self) -> bool {
        self.parallel
    }

    fn execute<'a>(&'a self, arguments: Value, _context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            ToolOutcome::ok(format!(
                "{}:{}",
                self.name,
                arguments.get("value").and_then(Value::as_str).unwrap_or("")
            ))
        })
    }
}

struct BarrierTool {
    name: &'static str,
    barrier: Arc<tokio::sync::Barrier>,
}

impl Tool for BarrierTool {
    fn name(&self) -> &str {
        self.name
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            self.name,
            "wait for its sibling",
            serde_json::json!({"type": "object"}),
        )
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn execute<'a>(&'a self, arguments: Value, _context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            self.barrier.wait().await;
            ToolOutcome::ok(format!(
                "{}:{}",
                self.name,
                arguments.get("value").and_then(Value::as_str).unwrap_or("")
            ))
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
    assert!(
        requests[0].json()["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == "echo")),
        "the first request advertises the registry's tools"
    );
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
async fn parallel_safe_tools_run_concurrently_and_keep_call_order() {
    let provider = FauxProvider::start(vec![
        tool_turn_many(&[
            ("call_1", "first", r#"{"value":"one"}"#),
            ("call_2", "second", r#"{"value":"two"}"#),
        ]),
        answer_turn("done"),
    ])
    .await;

    let directory = tempfile::tempdir().unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut registry = ToolRegistry::new();
    registry.register(BarrierTool {
        name: "first",
        barrier: barrier.clone(),
    });
    registry.register(BarrierTool {
        name: "second",
        barrier,
    });
    let mut harness = build(
        provider.url().to_owned(),
        registry,
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_limits(keel_core::short_limits(Duration::from_secs(2)));

    let mut started = Vec::new();
    let mut finished = Vec::new();
    let outcome = harness
        .run("go", |event| match event {
            RunEvent::ToolStarted { name, .. } => started.push(name.to_owned()),
            RunEvent::ToolFinished { call_id, .. } => finished.push(call_id.to_owned()),
            _ => {}
        })
        .await
        .unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(started, vec!["first", "second"]);
    assert_eq!(finished, vec!["call_1", "call_2"]);

    let results = result_rows(&harness);
    assert_eq!(
        results,
        vec![
            ("call_1".to_owned(), "first:one".to_owned(), false),
            ("call_2".to_owned(), "second:two".to_owned(), false),
        ]
    );
}

#[tokio::test]
async fn parallel_results_match_a_sequential_run() {
    async fn run(parallel: bool) -> Vec<(String, String, bool)> {
        let provider = FauxProvider::start(vec![
            tool_turn_many(&[
                ("call_1", "first", r#"{"value":"one"}"#),
                ("call_2", "second", r#"{"value":"two"}"#),
            ]),
            answer_turn("done"),
        ])
        .await;
        let directory = tempfile::tempdir().unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(TimedTool {
            name: "first",
            delay: Duration::from_millis(60),
            parallel,
        });
        registry.register(TimedTool {
            name: "second",
            delay: Duration::from_millis(5),
            parallel,
        });
        let mut harness = build(
            provider.url().to_owned(),
            registry,
            PermissionGate::AllowAll,
            directory.path(),
        );
        let outcome = harness.run("go", |_| {}).await.unwrap();
        assert_eq!(outcome.stop, RunStop::EndTurn);
        result_rows(&harness)
    }

    let parallel = run(true).await;
    let sequential = run(false).await;
    assert_eq!(parallel, sequential);
    assert_eq!(parallel[0].1, "first:one");
    assert_eq!(parallel[1].1, "second:two");
}

#[tokio::test]
async fn cancellation_before_a_parallel_batch_matches_the_sequential_loop() {
    async fn run(parallel: bool) -> (RunStop, Vec<(String, String, bool)>) {
        let provider = FauxProvider::start(vec![
            tool_turn_many(&[
                ("call_1", "gate", "{}"),
                ("call_2", "first", r#"{"value":"one"}"#),
                ("call_3", "second", r#"{"value":"two"}"#),
            ]),
            answer_turn("done"),
        ])
        .await;
        let directory = tempfile::tempdir().unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(TimedTool {
            name: "gate",
            delay: Duration::from_secs(5),
            parallel: false,
        });
        registry.register(TimedTool {
            name: "first",
            delay: Duration::from_millis(5),
            parallel,
        });
        registry.register(TimedTool {
            name: "second",
            delay: Duration::from_millis(5),
            parallel,
        });
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
        (outcome.stop, result_rows(&harness))
    }

    let parallel = run(true).await;
    let sequential = run(false).await;
    assert_eq!(parallel.0, RunStop::Cancelled);
    assert_eq!(parallel, sequential);
    assert_eq!(parallel.1.len(), 3);
    assert_eq!(parallel.1[0].1, "cancelled");
    assert!(
        parallel.1[1..]
            .iter()
            .all(|(_, output, error)| { output == "cancelled before execution" && *error })
    );
}

#[tokio::test]
async fn cancellation_inside_a_parallel_batch_leaves_one_result_per_call() {
    let provider = FauxProvider::start(vec![
        tool_turn_many(&[
            ("call_1", "first", r#"{"value":"one"}"#),
            ("call_2", "second", r#"{"value":"two"}"#),
        ]),
        answer_turn("done"),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(TimedTool {
        name: "first",
        delay: Duration::from_secs(5),
        parallel: true,
    });
    registry.register(TimedTool {
        name: "second",
        delay: Duration::from_secs(5),
        parallel: true,
    });
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
    let results = result_rows(&harness);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "call_1");
    assert_eq!(results[1].0, "call_2");
    assert!(
        results
            .iter()
            .all(|(_, output, is_error)| { *output == "cancelled" && *is_error })
    );
}

#[tokio::test]
async fn a_harness_advertises_its_registry_tools() {
    let provider = FauxProvider::start(vec![answer_turn("done")]).await;

    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool);
    let mut harness = build(
        provider.url().to_owned(),
        registry,
        PermissionGate::AllowAll,
        directory.path(),
    );

    harness.run("go", |_| {}).await.unwrap();

    let requests = provider.requests().await;
    let body = requests[0].json();
    let tools = body["tools"]
        .as_array()
        .expect("the encoded request advertises tools");
    let echo = tools
        .iter()
        .find(|tool| tool["function"]["name"] == "echo")
        .expect("the echo spec is advertised");
    assert_eq!(echo["function"]["description"], "echo the value");
    assert_eq!(echo["function"]["parameters"]["type"], "object");
}

#[tokio::test]
async fn a_non_empty_session_tool_list_is_kept() {
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "echo", r#"{"value":"hi"}"#),
        answer_turn("done"),
    ])
    .await;

    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool);
    registry.register(SlowTool);
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session =
        Session::new(route(provider.url().to_owned()), client).with_tools(vec![EchoTool.spec()]);
    let mut harness = Harness::new(
        session,
        Arc::new(registry),
        Workspace::open(directory.path()).unwrap(),
    )
    .with_permissions(PermissionGate::AllowAll);

    let outcome = harness.run("go", |_| {}).await.unwrap();
    assert_eq!(outcome.text(), "done");

    let requests = provider.requests().await;
    let body = requests[0].json();
    let names: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["echo"]);

    let body = requests[1].json();
    let tool_message = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("the curated tool executed");
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

#[tokio::test]
async fn a_session_log_holds_tool_results_in_order() {
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "echo", r#"{"value":"hi"}"#),
        answer_turn("done"),
    ])
    .await;

    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool);
    let meta = SessionMeta::new(keel_core::store::new_id(), "test", "gpt-4.1", "openai-chat");
    let log = Arc::new(SessionLog::create(directory.path(), &meta).expect("create the log"));
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session = Session::new(route(provider.url().to_owned()), client).with_sink(log.clone());
    let mut harness = Harness::new(
        session,
        Arc::new(registry),
        Workspace::open(directory.path()).unwrap(),
    )
    .with_permissions(PermissionGate::AllowAll);

    let outcome = harness.run("go", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(log.failure(), None);

    let (_, messages) = keel_core::store::read(log.path()).expect("read the log");
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert_eq!(messages[0].text(), "go");
    assert!(messages[1].has_tool_calls());
    assert_eq!(messages[2].tool_results().count(), 1);
    assert_eq!(messages[3].text(), "done");
}

fn failing_turn() -> Turn {
    Turn::sse(vec![
        Chunk::text(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
        ),
        Chunk::text("data: {\"error\":{\"message\":\"upstream exploded\"}}\n\n"),
    ])
}

fn quick_retries(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
    }
}

#[tokio::test]
async fn a_retryable_failure_replays_the_turn_without_duplicating_input() {
    let provider = FauxProvider::start(vec![failing_turn(), answer_turn("done")]).await;
    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        provider.url().to_owned(),
        ToolRegistry::new(),
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_retry(quick_retries(3));

    let mut retries = 0;
    let outcome = harness
        .run("go", |event| {
            if matches!(event, RunEvent::Retrying { .. }) {
                retries += 1;
            }
        })
        .await
        .unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(outcome.text(), "done");
    assert_eq!(retries, 1);

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 2);
    let body = requests[1].json();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "go");

    let history = harness.session().history();
    assert_eq!(history.len(), 3, "user, the failed turn, the answer");
    assert!(history[1].error.is_some(), "{:?}", history[1]);
    assert_eq!(history[2].text(), "done");
}

#[tokio::test]
async fn a_decode_failure_is_not_retried() {
    let malformed = Turn::sse(vec![Chunk::text("data: {not json}\n\n")]);
    let provider = FauxProvider::start(vec![malformed]).await;
    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        provider.url().to_owned(),
        ToolRegistry::new(),
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_retry(quick_retries(3));

    let outcome = harness.run("go", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::Failed);
    assert_eq!(
        provider.requests().await.len(),
        1,
        "a decode failure is not replayable"
    );
}

#[tokio::test]
async fn retries_stop_at_max_attempts() {
    let provider = FauxProvider::start(vec![failing_turn(), failing_turn()]).await;
    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        provider.url().to_owned(),
        ToolRegistry::new(),
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_retry(quick_retries(2));

    let outcome = harness.run("go", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::Failed);
    assert_eq!(provider.requests().await.len(), 2);
}

#[tokio::test]
async fn cancellation_during_backoff_stops_the_run() {
    let provider = FauxProvider::start(vec![failing_turn(), answer_turn("never")]).await;
    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        provider.url().to_owned(),
        ToolRegistry::new(),
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_retry(RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_secs(30),
        max_delay: Duration::from_secs(30),
    });

    let cancel = harness.cancellation_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });

    let outcome = tokio::time::timeout(Duration::from_secs(5), harness.run("go", |_| {}))
        .await
        .expect("cancellation ends the backoff instead of sleeping it out")
        .unwrap();

    assert_eq!(outcome.stop, RunStop::Cancelled);
    assert_eq!(provider.requests().await.len(), 1);
}

#[tokio::test]
async fn a_compaction_emits_one_status_event() {
    let provider = FauxProvider::start(vec![
        answer_turn_with_usage("one", 9_000, 100),
        answer_turn_with_usage("handoff", 120, 30),
        answer_turn_with_usage("two", 500, 20),
    ])
    .await;
    let mut route = route(format!("{}/v1/chat/completions", provider.url()));
    route.context_window = 10_000;
    route.max_output_tokens = 1_000;

    let directory = tempfile::tempdir().unwrap();
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session = Session::new(route, client);
    let mut harness = Harness::new(
        session,
        Arc::new(ToolRegistry::new()),
        Workspace::open(directory.path()).unwrap(),
    )
    .with_permissions(PermissionGate::AllowAll);

    harness.run("first", |_| {}).await.unwrap();

    let mut compacted = 0;
    let outcome = harness
        .run("second", |event| {
            if let RunEvent::Compacted {
                reason,
                before_messages,
                after_messages,
                kept_user_messages,
                summary_usage,
                ..
            } = event
            {
                compacted += 1;
                assert_eq!(reason, CompactionReason::Threshold);
                assert_eq!(before_messages, 2);
                assert_eq!(after_messages, 2);
                assert_eq!(kept_user_messages, 1);
                assert_eq!(summary_usage.input_tokens, 120);
                assert_eq!(summary_usage.output_tokens, 30);
            }
        })
        .await
        .unwrap();

    assert_eq!(compacted, 1, "the host sees the compaction once");
    assert_eq!(
        outcome.usage.input_tokens, 500,
        "the assistant message carries only the main request"
    );
    assert_eq!(outcome.usage.output_tokens, 20);
    assert_eq!(
        outcome.compaction_usage.input_tokens, 120,
        "the summary usage is reported separately"
    );
    assert_eq!(outcome.compaction_usage.output_tokens, 30);
}

#[tokio::test]
async fn a_compacted_tool_turn_leaves_no_orphan_result() {
    let provider = FauxProvider::start(vec![
        tool_turn_with_usage("call_1", "echo", r#"{"value":"hi"}"#, 9_500, 100),
        answer_turn_with_usage("handoff", 120, 30),
        answer_turn_with_usage("done", 500, 20),
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
    harness.session_mut().route.context_window = 10_000;
    harness.session_mut().route.max_output_tokens = 1_000;

    let outcome = harness.run("go", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(outcome.text(), "done");
    assert_eq!(outcome.compaction_usage.input_tokens, 120);

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 3, "main, summary, main");
    let summary = requests[1].json();
    assert!(
        summary["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool"),
        "the summary request saw the executed tool result: {summary}"
    );

    let body = requests[2].json();
    let messages = body["messages"].as_array().expect("messages are an array");
    assert!(
        messages.iter().all(|message| message["role"] != "tool"),
        "no tool result crossed the cut: {body}"
    );
    assert!(
        messages
            .iter()
            .all(|message| message.get("tool_calls").is_none()),
        "no tool call crossed the cut: {body}"
    );
    let texts: Vec<&str> = messages
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert!(texts.contains(&"go"));
    assert!(texts.iter().any(|text| text.starts_with(SUMMARY_PREFIX)));
}
