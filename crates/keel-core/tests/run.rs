use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use keel_core::{
    CompactionReason, Harness, RunEvent, RunStop, SUMMARY_PREFIX, Session, SessionLog, SessionMeta,
    TestOutcome, VERIFY_PROMPT_PREFIX,
};
use keel_llm_protocol::anthropic_messages::AnthropicMessages;
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::{Auth, Client, EnvResolver, Framing, RetryPolicy, Route};
use keel_llm_types::{
    CacheBreakpoint, CacheObject, CachePolicy, Compat, Content, ProtocolId, ToolCallId, ToolSpec,
};
use keel_testkit::{Chunk, FauxProvider, Turn};
use keel_tools::{
    PermissionGate, Tool, ToolContext, ToolFuture, ToolOutcome, ToolRegistry, Workspace, tool_spec,
};
use serde_json::{Value, json};

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

fn anthropic_route(endpoint: String) -> Route {
    Route {
        provider: "test".into(),
        model: "claude-test".into(),
        endpoint,
        protocol: Arc::new(AnthropicMessages::new(Compat::for_protocol(
            ProtocolId::AnthropicMessages,
        ))),
        auth: Auth::None,
        framing: Framing::Sse,
        headers: Vec::new(),
        context_window: 0,
        max_output_tokens: 0,
    }
}

fn anthropic_answer_turn(text: &str) -> Turn {
    let text = serde_json::to_string(text).unwrap();
    Turn::sse(vec![Chunk::text(format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-test\",\"stop_reason\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":0}}}}}}\n\n\
         event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{text}}}}}\n\n\
         event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":5}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    ))])
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

struct ScriptedBash {
    outputs: Mutex<VecDeque<(String, bool)>>,
}

impl ScriptedBash {
    fn new(outputs: impl IntoIterator<Item = (&'static str, bool)>) -> Self {
        Self {
            outputs: Mutex::new(
                outputs
                    .into_iter()
                    .map(|(text, error)| (text.to_owned(), error))
                    .collect(),
            ),
        }
    }
}

impl Tool for ScriptedBash {
    fn name(&self) -> &str {
        "bash"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            "bash",
            "a scripted test runner",
            serde_json::json!({"type": "object"}),
        )
    }

    fn execute<'a>(&'a self, _arguments: Value, _context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let (text, is_error) = self
                .outputs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop_front()
                .expect("a scripted test output");
            ToolOutcome { text, is_error }
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

struct CountingTool {
    name: &'static str,
    delay: Duration,
    parallel: bool,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
}

impl Tool for CountingTool {
    fn name(&self) -> &str {
        self.name
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            self.name,
            "echo after counting its overlap",
            serde_json::json!({"type": "object"}),
        )
    }

    fn parallel_safe(&self) -> bool {
        self.parallel
    }

    fn execute<'a>(&'a self, arguments: Value, _context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
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
async fn an_unsafe_call_is_a_barrier_between_safe_calls() {
    let provider = FauxProvider::start(vec![
        tool_turn_many(&[
            ("call_1", "first", r#"{"value":"one"}"#),
            ("call_2", "gate", r#"{"value":"gate"}"#),
            ("call_3", "second", r#"{"value":"two"}"#),
        ]),
        answer_turn("done"),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    for (name, parallel) in [("first", true), ("gate", false), ("second", true)] {
        registry.register(CountingTool {
            name,
            delay: Duration::from_millis(100),
            parallel,
            active: active.clone(),
            max_active: max_active.clone(),
        });
    }
    let mut harness = build(
        provider.url().to_owned(),
        registry,
        PermissionGate::AllowAll,
        directory.path(),
    );

    let outcome = harness.run("go", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    let results = result_rows(&harness);
    assert_eq!(
        results,
        vec![
            ("call_1".to_owned(), "first:one".to_owned(), false),
            ("call_2".to_owned(), "gate:gate".to_owned(), false),
            ("call_3".to_owned(), "second:two".to_owned(), false),
        ]
    );
    assert_eq!(
        max_active.load(Ordering::SeqCst),
        1,
        "an unsafe call overlapped a safe call"
    );
}

#[tokio::test]
async fn parallel_batches_larger_than_eight_keep_order_across_chunks() {
    const NAMES: [&str; 10] = ["t0", "t1", "t2", "t3", "t4", "t5", "t6", "t7", "t8", "t9"];
    let provider = FauxProvider::start(vec![
        tool_turn_many(&[
            ("call_0", "t0", r#"{"value":"0"}"#),
            ("call_1", "t1", r#"{"value":"1"}"#),
            ("call_2", "t2", r#"{"value":"2"}"#),
            ("call_3", "t3", r#"{"value":"3"}"#),
            ("call_4", "t4", r#"{"value":"4"}"#),
            ("call_5", "t5", r#"{"value":"5"}"#),
            ("call_6", "t6", r#"{"value":"6"}"#),
            ("call_7", "t7", r#"{"value":"7"}"#),
            ("call_8", "t8", r#"{"value":"8"}"#),
            ("call_9", "t9", r#"{"value":"9"}"#),
        ]),
        answer_turn("done"),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    for name in NAMES {
        registry.register(CountingTool {
            name,
            delay: Duration::from_millis(30),
            parallel: true,
            active: active.clone(),
            max_active: max_active.clone(),
        });
    }
    let mut harness = build(
        provider.url().to_owned(),
        registry,
        PermissionGate::AllowAll,
        directory.path(),
    );

    let outcome = harness.run("go", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    let results = result_rows(&harness);
    let expected: Vec<_> = (0..10)
        .map(|index| (format!("call_{index}"), format!("t{index}:{index}"), false))
        .collect();
    assert_eq!(results, expected);
    let max = max_active.load(Ordering::SeqCst);
    assert!(max <= 8, "{max} calls ran at once");
    assert!(max > 1, "the ten-call batch never ran concurrently");
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
async fn a_session_cache_policy_reaches_the_wire_request() {
    let provider = FauxProvider::start(vec![answer_turn("done")]).await;
    let directory = tempfile::tempdir().unwrap();
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session =
        Session::new(route(provider.url().to_owned()), client).with_cache(CachePolicy::None);
    let mut harness = Harness::new(
        session,
        Arc::new(ToolRegistry::new()),
        Workspace::open(directory.path()).unwrap(),
    );

    harness.run("go", |_| {}).await.unwrap();

    provider.wait_for_requests(1).await;
    let requests = provider.requests().await;
    let body = requests[0].json();
    assert!(body.get("prompt_cache_key").is_none());
    assert!(!body.to_string().contains("cache_control"));
}

#[tokio::test]
async fn a_session_object_policy_places_exact_anthropic_pointers() {
    let provider = FauxProvider::start(vec![anthropic_answer_turn("done")]).await;
    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool);
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session = Session::new(
        anthropic_route(format!("{}/v1/messages", provider.url())),
        client,
    )
    .with_cache(CachePolicy::Object(
        CacheObject::new()
            .breakpoint(CacheBreakpoint::System)
            .breakpoint(CacheBreakpoint::Tool(0))
            .breakpoint(CacheBreakpoint::Part {
                message: 0,
                part: 0,
            }),
    ));
    let mut harness = Harness::new(
        session,
        Arc::new(registry),
        Workspace::open(directory.path()).unwrap(),
    );

    let outcome = harness.run("go", |_| {}).await.unwrap();

    assert_eq!(outcome.text(), "done");
    let requests = provider.requests().await;
    let body = requests[0].json();
    assert_eq!(
        body.pointer("/system/0/cache_control"),
        Some(&json!({"type": "ephemeral"}))
    );
    assert_eq!(
        body.pointer("/tools/0/cache_control"),
        Some(&json!({"type": "ephemeral"})),
        "Tool(0) places the breakpoint on the first advertised tool"
    );
    assert_eq!(
        body.pointer("/messages/0/content/0/cache_control"),
        Some(&json!({"type": "ephemeral"})),
        "Part(0, 0) places the breakpoint on the user text block"
    );
    assert_eq!(requests[0].header("anthropic-beta"), None);
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
async fn an_idle_timeout_after_partial_output_replays_at_the_turn_boundary() {
    let (held, _release) = Chunk::gated(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"never\"},\"finish_reason\":null}]}\n\n",
    );
    let provider = FauxProvider::start(vec![
        Turn::sse(vec![
            Chunk::text(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
            ),
            held,
        ]),
        answer_turn("done"),
    ])
    .await;
    let client = Client::with_resolver(Arc::new(EnvResolver))
        .unwrap()
        .with_stream_idle_timeout(Some(Duration::from_millis(100)));
    let session = Session::new(route(provider.url().to_owned()), client);
    let directory = tempfile::tempdir().unwrap();
    let mut harness = Harness::new(
        session,
        Arc::new(ToolRegistry::new()),
        Workspace::open(directory.path()).unwrap(),
    )
    .with_permissions(PermissionGate::AllowAll)
    .with_retry(quick_retries(2));

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
    assert_eq!(retries, 1, "the idle timeout is replayed once");
    assert_eq!(provider.request_count().await, 2);

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

const PYTEST_FAILURE: &str = "\
=========================== short test summary info ============================
FAILED tests/test_app.py::test_value - assert 1 == 2
E       assert 1 == 2
========================= 1 failed, 1 passed in 0.04s ==========================
";

#[tokio::test]
async fn the_pre_final_reminder_fires_once_and_does_not_loop() {
    let provider = FauxProvider::start(vec![
        answer_turn("done"),
        answer_turn("Changed app.py; ran the test; nothing unverified."),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        provider.url().to_owned(),
        ToolRegistry::new(),
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_test_loop();

    let mut requested = 0;
    let mut seen = None;
    let outcome = harness
        .run("go", |event| {
            if let RunEvent::VerifyRequested {
                tests_run,
                failing_tests,
            } = event
            {
                requested += 1;
                seen = Some((tests_run, failing_tests));
            }
        })
        .await
        .unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(requested, 1, "the reminder is injected once");
    assert_eq!(seen, Some((false, 0)));
    assert_eq!(
        provider.request_count().await,
        2,
        "one reminder, one answer"
    );
    let history = harness.session().history();
    assert_eq!(history.len(), 4, "user, answer, reminder, final answer");
    assert!(
        history[2].text().starts_with(VERIFY_PROMPT_PREFIX),
        "the injected message is the verification prompt: {:?}",
        history[2].text()
    );
    assert_eq!(
        history[3].text(),
        "Changed app.py; ran the test; nothing unverified."
    );
    let body = provider.requests().await[1].json();
    assert!(
        body["messages"].to_string().contains(VERIFY_PROMPT_PREFIX),
        "the model sees the reminder: {body}"
    );
}

#[tokio::test]
async fn the_pre_final_reminder_is_opt_in() {
    let provider = FauxProvider::start(vec![answer_turn("done")]).await;
    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        provider.url().to_owned(),
        ToolRegistry::new(),
        PermissionGate::AllowAll,
        directory.path(),
    );

    let mut requested = 0;
    let outcome = harness
        .run("go", |event| {
            if matches!(event, RunEvent::VerifyRequested { .. }) {
                requested += 1;
            }
        })
        .await
        .unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(requested, 0);
    assert_eq!(provider.request_count().await, 1);
    assert_eq!(harness.session().history().len(), 2);
}

#[tokio::test]
async fn a_tracked_test_failure_survives_compaction() {
    let provider = FauxProvider::start(vec![
        tool_turn_with_usage(
            "call_1",
            "bash",
            r#"{"command":"python -m pytest tests/test_app.py -q"}"#,
            9_500,
            100,
        ),
        answer_turn_with_usage("handoff", 120, 30),
        answer_turn_with_usage("done", 500, 20),
        answer_turn_with_usage("verified", 500, 20),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(ScriptedBash::new([(PYTEST_FAILURE, true)]));
    let mut harness = build(
        provider.url().to_owned(),
        registry,
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_test_loop();
    harness.session_mut().route.context_window = 10_000;
    harness.session_mut().route.max_output_tokens = 1_000;

    let mut prompts = 0;
    let outcome = harness
        .run("go", |event| {
            if matches!(event, RunEvent::VerifyRequested { .. }) {
                prompts += 1;
            }
        })
        .await
        .unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(outcome.text(), "verified");
    assert_eq!(prompts, 1);
    let status = harness.test_status();
    assert_eq!(status.runs, 1);
    assert_eq!(status.last_outcome, Some(TestOutcome::Failed));
    assert_eq!(status.failing_tests, vec!["tests/test_app.py::test_value"]);

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 4, "main, summary, main, verification");
    let summary_request = requests[1].json().to_string();
    assert!(
        summary_request.contains("Visible test status"),
        "the summary request pins the tracked test state: {summary_request}"
    );
    let compacted_request = requests[2].json();
    let compacted = compacted_request.to_string();
    assert!(
        compacted.contains("tests/test_app.py::test_value"),
        "the post-compaction view keeps the failing test name: {compacted}"
    );
    assert!(
        compacted.contains("[keel handoff summary]"),
        "the view still ends with one handoff summary: {compacted}"
    );
}
