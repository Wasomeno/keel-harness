//! Session compaction: the trigger boundary, overflow retry, the summary
//! request, the replacement view, and resume continuity.
//!
//! The transport is the scripted faux provider, so each test sees exactly what
//! keel sent. Numbers are small and exact; the trigger rule is checked on both
//! sides of the boundary.

use std::sync::Arc;

use keel_core::{
    COMPACTION_PROMPT, CompactionReason, DEFAULT_SYSTEM_PROMPT, SUMMARY_PREFIX, Session,
    SessionLog, SessionMeta, WORKING_MEMORY_HEADER, WorkingMemory, compacted_view_with_budget,
    estimate_message_tokens, estimate_request_input_tokens, store, summary_message,
};
use keel_llm_protocol::anthropic_messages::AnthropicMessages;
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::{Auth, Client, EnvResolver, Framing, Route};
use keel_llm_types::{
    CacheBreakpoint, CacheObject, CachePolicy, CacheTtl, Compat, ErrorKind, Message, Part,
    ProtocolId, RepairNote, Source, ToolSpec,
};
use keel_testkit::{Chunk, FauxProvider, RecordedRequest, Turn};
use serde_json::{Value, json};

fn route(endpoint: String, context_window: u32, max_output_tokens: u32) -> Route {
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
        context_window,
        max_output_tokens,
    }
}

fn text_turn(text: &str) -> Turn {
    let text = serde_json::to_string(text).unwrap();
    Turn::sse(vec![
        Chunk::text(format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{text}}},\"finish_reason\":null}}]}}\n\n"
        )),
        Chunk::text(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ),
        Chunk::text("data: [DONE]\n\n"),
    ])
}

fn text_turn_with_usage(text: &str, input: u64, output: u64) -> Turn {
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

fn overflow_turn() -> Turn {
    Turn::text(
        400,
        "This model's maximum context length is 10000 tokens. Please reduce the length.",
    )
}

fn anthropic_route(endpoint: String, context_window: u32, max_output_tokens: u32) -> Route {
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
        context_window,
        max_output_tokens,
    }
}

fn anthropic_text_turn(text: &str, input: u64, output: u64) -> Turn {
    let text = serde_json::to_string(text).unwrap();
    Turn::sse(vec![Chunk::text(format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-test\",\"stop_reason\":null,\"usage\":{{\"input_tokens\":{input},\"output_tokens\":0}}}}}}\n\n\
         event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{text}}}}}\n\n\
         event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"input_tokens\":{input},\"output_tokens\":{output}}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    ))])
}

fn anthropic_mid_stream_overflow_turn() -> Turn {
    Turn::sse(vec![Chunk::text(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-test\",\"stop_reason\":null,\"usage\":{\"input_tokens\":9500,\"output_tokens\":0}}}\n\n\
         event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"request_too_large\",\"message\":\"prompt is too long\"}}\n\n",
    )])
}

fn openai_mid_stream_overflow_turn(text: &str) -> Turn {
    let text = serde_json::to_string(text).unwrap();
    Turn::sse(vec![
        Chunk::text(format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{text}}},\"finish_reason\":null}}]}}\n\n"
        )),
        Chunk::text(
            "data: {\"error\":{\"message\":\"This model's maximum context length is 10000 tokens. Please reduce the length of the messages.\",\"type\":\"invalid_request_error\",\"code\":\"context_length_exceeded\"}}\n\n",
        ),
        Chunk::text("data: [DONE]\n\n"),
    ])
}

fn truncated_turn(text: &str, input: u64, output: u64) -> Turn {
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
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
        ),
        Chunk::text("data: [DONE]\n\n"),
    ])
}

fn anthropic_last_message_text(body: &Value) -> &str {
    body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_array())
        .and_then(|content| content.first())
        .and_then(|block| block["text"].as_str())
        .unwrap_or_default()
}

fn body(request: &RecordedRequest) -> Value {
    request.json()
}

fn messages(body: &Value) -> &[Value] {
    body["messages"].as_array().expect("messages are an array")
}

fn message_texts(body: &Value) -> Vec<String> {
    messages(body)
        .iter()
        .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn last_message_text(body: &Value) -> &str {
    messages(body)
        .last()
        .expect("a last message")
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn has_tools(body: &Value) -> bool {
    body.get("tools").is_some()
}

fn summary_requests(requests: &[RecordedRequest]) -> Vec<&RecordedRequest> {
    requests
        .iter()
        .filter(|request| {
            let body = body(request);
            !has_tools(&body) && last_message_text(&body) == COMPACTION_PROMPT
        })
        .collect()
}

fn build_session(provider: &FauxProvider, window: u32, output: u32) -> Session {
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    Session::new(
        route(
            format!("{}/v1/chat/completions", provider.url()),
            window,
            output,
        ),
        client,
    )
}

fn build_anthropic_session(provider: &FauxProvider, window: u32, output: u32) -> Session {
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    Session::new(
        anthropic_route(format!("{}/v1/messages", provider.url()), window, output),
        client,
    )
}

#[test]
fn kept_user_messages_stay_strictly_under_the_budget() {
    let oldest = Message::user("old");
    let newest = Message::user("newest");
    let budget = estimate_message_tokens(&oldest) + estimate_message_tokens(&newest);

    let (view, kept) = compacted_view_with_budget(&[oldest, newest.clone()], "handoff", budget);

    assert_eq!(
        kept, 1,
        "the older message would make the kept set equal to the budget"
    );
    assert_eq!(view[0].text(), "newest");
    assert!(view[1].text().starts_with(SUMMARY_PREFIX));
}

#[tokio::test]
async fn compaction_happens_at_the_trigger_boundary_and_not_before() {
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 7_000, 500),
        text_turn_with_usage("two", 8_800, 200),
        text_turn("handoff: both turns are done"),
        text_turn("three"),
    ])
    .await;
    let mut session = build_session(&provider, 10_000, 1_000);

    session.step(Some("first")).await.unwrap();
    assert!(
        provider.requests().await.len() == 1,
        "a fresh history does not compact"
    );

    session.step(Some("second")).await.unwrap();
    assert_eq!(
        provider.requests().await.len(),
        2,
        "7000 + 500 + 1000 is below the window, so no compaction yet"
    );

    let outcome = session.step(Some("third")).await.unwrap();
    let requests = provider.requests().await;
    assert_eq!(
        requests.len(),
        4,
        "the summary request and the compacted request both ran"
    );

    let summary = body(&requests[2]);
    assert!(!has_tools(&summary), "the summarizer is offered no tools");
    assert_eq!(last_message_text(&summary), COMPACTION_PROMPT);
    let summary_texts = message_texts(&summary);
    assert!(
        !summary_texts.iter().any(|text| text.contains("third")),
        "the pending input is not part of the summary request: {summary_texts:?}"
    );

    let compacted = body(&requests[3]);
    let kept = message_texts(&compacted);
    assert_eq!(kept[0], DEFAULT_SYSTEM_PROMPT);
    assert_eq!(kept[1], "first");
    assert_eq!(kept[2], "second");
    assert!(kept[3].starts_with(SUMMARY_PREFIX), "{:?}", kept[3]);
    assert_eq!(kept[4], "third");
    assert_eq!(messages(&compacted).len(), 5);
    assert!(
        messages(&compacted)
            .iter()
            .all(|message| message["role"] == "user" || message["role"] == "system"),
        "no assistant turn or tool message crosses the cut: {compacted}"
    );
    assert!(
        !outcome
            .notes
            .iter()
            .any(|note| matches!(note, RepairNote::SynthesizedToolResult { .. })),
        "the compacted view needs no synthesized results: {:?}",
        outcome.notes
    );

    let stats = session.take_compaction().expect("the trigger ran");
    assert_eq!(stats.reason, CompactionReason::Threshold);
    assert_eq!(stats.before_messages, 4);
    assert_eq!(stats.after_messages, 3);
    assert_eq!(stats.kept_user_messages, 2);
    assert_eq!(session.history.len(), 5);
}

#[tokio::test]
async fn a_context_overflow_compacts_once_and_retries() {
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 100, 10),
        overflow_turn(),
        text_turn("handoff: the first turn is done"),
        text_turn("two"),
    ])
    .await;
    let mut session = build_session(&provider, 10_000, 1_000);

    session.step(Some("first")).await.unwrap();
    let outcome = session.step(Some("second")).await.unwrap();

    let requests = provider.requests().await;
    assert_eq!(
        requests.len(),
        4,
        "one normal request, one overflow, one summary, one replay"
    );
    assert_eq!(summary_requests(&requests).len(), 1);

    let replay = body(&requests[3]);
    let texts = message_texts(&replay);
    assert_eq!(
        texts
            .iter()
            .filter(|text| text.as_str() == "second")
            .count(),
        1,
        "the user input is sent once: {texts:?}"
    );
    assert!(texts[1] == "first");
    assert!(texts[2].starts_with(SUMMARY_PREFIX), "{texts:?}");
    assert_eq!(texts[3], "second");
    assert_eq!(outcome.text(), "two");

    let stats = session.take_compaction().expect("the overflow compacted");
    assert_eq!(stats.reason, CompactionReason::Overflow);
    assert_eq!(stats.before_messages, 2);
    assert_eq!(stats.after_messages, 2);
}

#[tokio::test]
async fn a_second_context_overflow_is_a_normal_failure() {
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 100, 10),
        overflow_turn(),
        text_turn("handoff: the first turn is done"),
        overflow_turn(),
    ])
    .await;
    let mut session = build_session(&provider, 10_000, 1_000);

    session.step(Some("first")).await.unwrap();
    let error = session
        .step(Some("second"))
        .await
        .expect_err("a second overflow fails normally");

    assert_eq!(error.kind, ErrorKind::ContextOverflow);
    let requests = provider.requests().await;
    assert_eq!(requests.len(), 4);
    assert_eq!(
        summary_requests(&requests).len(),
        1,
        "exactly one compaction ran"
    );
    assert!(
        !session
            .history
            .iter()
            .any(|message| message.text() == "second"),
        "the failed pending input is not committed"
    );
    let stats = session
        .take_compaction()
        .expect("the one compaction is recorded");
    assert_eq!(stats.reason, CompactionReason::Overflow);
}

#[tokio::test]
async fn a_small_catalog_entry_drives_the_trigger() {
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 9_500, 100),
        text_turn("handoff"),
        text_turn("two"),
    ])
    .await;

    let catalog = keel_llm_catalog::Catalog::parse(&format!(
        r#"
version = 1

[[providers]]
id = "test"
name = "Test"
base_url = "{}"

[[providers.models]]
id = "small-window"
name = "Small window"
protocol = "openai-chat"
context_window = 10000
max_output_tokens = 1000
"#,
        provider.url()
    ))
    .expect("the test catalog parses");
    let (provider_spec, model) = catalog.find("test", "small-window").expect("the entry");
    let route = keel_core::build_route(provider_spec, model).expect("the route builds");
    assert_eq!(route.context_window, 10_000);
    assert_eq!(route.max_output_tokens, 1_000);

    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let mut session = Session::new(route, client);
    session.step(Some("first")).await.unwrap();
    session.step(Some("second")).await.unwrap();

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 3, "summary then compacted request");
    assert_eq!(summary_requests(&requests).len(), 1);
    assert_eq!(
        session.take_compaction().expect("the trigger ran").reason,
        CompactionReason::Threshold
    );
}

#[tokio::test]
async fn a_mid_stream_context_overflow_compacts_and_retries() {
    let provider = FauxProvider::start(vec![
        anthropic_text_turn("one", 100, 10),
        anthropic_mid_stream_overflow_turn(),
        anthropic_text_turn("handoff: the first turn is done", 50, 20),
        anthropic_text_turn("two", 80, 5),
    ])
    .await;
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let mut session = Session::new(
        anthropic_route(format!("{}/v1/messages", provider.url()), 10_000, 1_000),
        client,
    );

    session.step(Some("first")).await.unwrap();
    let outcome = session.step(Some("second")).await.unwrap();

    assert!(!outcome.failed(), "the retry succeeded");
    assert_eq!(outcome.text(), "two");
    let requests = provider.requests().await;
    assert_eq!(requests.len(), 4);
    let summary = body(&requests[2]);
    assert_eq!(summary.get("tools"), None);
    assert_eq!(
        anthropic_last_message_text(&summary),
        COMPACTION_PROMPT,
        "the summary request is the third request"
    );

    let replay = body(&requests[3]);
    let texts: Vec<String> = replay["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| {
            message["content"]
                .as_array()
                .and_then(|content| content.first())
                .and_then(|block| block["text"].as_str())
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(
        texts
            .iter()
            .filter(|text| text.as_str() == "second")
            .count(),
        1,
        "the committed user input is not duplicated: {texts:?}"
    );
    assert!(texts.iter().any(|text| text.starts_with(SUMMARY_PREFIX)));
    let stats = session
        .take_compaction()
        .expect("the mid-stream overflow compacted");
    assert_eq!(stats.reason, CompactionReason::Overflow);
}

#[tokio::test]
async fn a_mid_stream_overflow_keeps_the_newest_user_message_over_budget() {
    // 8,000 bytes is about 2,667 estimated tokens, over the 2,500-token
    // user-message budget for a 10,000-token window.
    let giant = "x".repeat(8_000);
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 500, 10),
        openai_mid_stream_overflow_turn("partial"),
        text_turn_with_usage("handoff", 200, 20),
        text_turn_with_usage("done", 300, 15),
    ])
    .await;
    let mut session = build_session(&provider, 10_000, 1_000);

    session.step(Some("first")).await.unwrap();
    let outcome = session.step(Some(&giant)).await.unwrap();
    assert_eq!(outcome.text(), "done");

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 4);
    let replay = body(&requests[3]);
    let texts = message_texts(&replay);
    assert_eq!(
        texts.iter().filter(|text| text.as_str() == giant).count(),
        1,
        "the current request is kept verbatim and not duplicated"
    );
    assert!(texts.iter().any(|text| text.starts_with(SUMMARY_PREFIX)));
    assert!(
        !texts.iter().any(|text| text == "one"),
        "assistant turns do not cross the cut: {texts:?}"
    );
    assert!(
        messages(&replay)
            .iter()
            .all(|message| message["role"] != "tool"),
        "no tool traffic is replayed"
    );

    let stats = session.take_compaction().expect("the overflow compacted");
    assert_eq!(stats.reason, CompactionReason::Overflow);
    assert_eq!(
        stats.kept_user_messages, 1,
        "the giant newest message is kept and the older one is over budget"
    );
}

#[tokio::test]
async fn a_second_mid_stream_overflow_is_a_normal_failure() {
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 500, 10),
        openai_mid_stream_overflow_turn("partial one"),
        text_turn_with_usage("handoff", 100, 10),
        openai_mid_stream_overflow_turn("partial two"),
    ])
    .await;
    let mut session = build_session(&provider, 10_000, 1_000);

    session.step(Some("first")).await.unwrap();
    let outcome = session.step(Some("second")).await.unwrap();

    assert!(outcome.failed());
    assert_eq!(
        outcome.error.as_ref().map(|error| error.kind),
        Some(ErrorKind::ContextOverflow)
    );
    assert_eq!(
        provider.requests().await.len(),
        4,
        "one compaction and one retry, then the second overflow fails"
    );
    assert_eq!(session.take_compactions().len(), 1);
}

#[tokio::test]
async fn a_threshold_then_overflow_compaction_reports_both() {
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 9_500, 100),
        text_turn_with_usage("handoff one", 100, 10),
        openai_mid_stream_overflow_turn("partial"),
        text_turn_with_usage("handoff two", 100, 10),
        text_turn_with_usage("two", 500, 20),
    ])
    .await;
    let mut session = build_session(&provider, 10_000, 1_000);

    session.step(Some("first")).await.unwrap();
    let outcome = session.step(Some("second")).await.unwrap();
    assert_eq!(outcome.text(), "two");

    let compactions = session.take_compactions();
    assert_eq!(compactions.len(), 2, "both compactions are reported");
    assert_eq!(compactions[0].reason, CompactionReason::Threshold);
    assert_eq!(compactions[1].reason, CompactionReason::Overflow);
    assert_eq!(compactions[0].usage.input_tokens, 100);
    assert_eq!(compactions[1].usage.input_tokens, 100);
    assert_eq!(provider.requests().await.len(), 5);
    assert_eq!(summary_requests(&provider.requests().await).len(), 2);
}

#[tokio::test]
async fn a_truncated_summary_fails_clearly() {
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 9_500, 100),
        truncated_turn("half a handoff", 120, 30),
    ])
    .await;
    let mut session = build_session(&provider, 10_000, 1_000);

    session.step(Some("first")).await.unwrap();
    let error = session
        .step(Some("second"))
        .await
        .expect_err("a summary cut off at the output budget is not usable");

    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    assert!(error.message.contains("summary was truncated"), "{error}");
    assert_eq!(provider.requests().await.len(), 2);
    assert_eq!(session.history.len(), 2);
    assert!(session.take_compaction().is_none());
    let usage = session.take_compaction_usage();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (120, 30),
        "the paid summary request is reported even though compaction failed"
    );
}

#[tokio::test]
async fn a_compacted_view_that_cannot_fit_fails_clearly() {
    // The newest kept user message is large enough that the view cannot fit
    // alongside the 1,000-token output budget, while the summary and pending
    // input on their own still would.
    let giant = "x".repeat(7_000);
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 9_500, 100),
        text_turn_with_usage("handoff", 100, 20),
    ])
    .await;
    let directory = tempfile::tempdir().expect("temp dir");
    let meta = SessionMeta::new("unfit-view", "test", "gpt-4.1", "openai-chat");
    let log = Arc::new(SessionLog::create(directory.path(), &meta).expect("create log"));
    let mut session = build_session(&provider, 3_000, 1_000).with_sink(log.clone());

    session.step(Some(&giant)).await.unwrap();
    let error = session
        .step(Some("second"))
        .await
        .expect_err("the compacted view plus the output budget cannot fit");

    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    assert!(error.message.contains("cannot compact"), "{error}");
    assert!(error.message.contains("replacement view"), "{error}");
    assert!(error.message.contains("plus 1000 output"), "{error}");
    assert!(error.message.contains("3000-token"), "{error}");
    assert!(
        error.message.contains("newest kept user message"),
        "{error}"
    );
    assert!(
        error.message.contains("kept user messages total about"),
        "{error}"
    );
    assert_eq!(
        provider.requests().await.len(),
        2,
        "the main request and the summary request were sent; nothing after"
    );
    assert_eq!(session.history.len(), 2);
    assert!(
        session.take_compaction().is_none(),
        "a view that does not fit is never stored"
    );
    let log_contents = std::fs::read_to_string(log.path()).expect("read log");
    assert!(
        !log_contents.contains("\"type\":\"compaction\""),
        "a view that does not fit is never logged: {log_contents}"
    );
    let usage = session.take_compaction_usage();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (100, 20),
        "the paid summary request is reported even when the view does not fit"
    );
}

#[tokio::test]
async fn a_compacted_view_that_exactly_fills_the_window_is_rejected() {
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 9_500, 100),
        text_turn("handoff"),
    ])
    .await;
    let mut session = build_session(&provider, 10_000, 2_000);
    session.step(Some("first")).await.unwrap();

    let output = 2_000u64;
    let next_input = keel_core::estimate_request_input_tokens(
        session.system.as_deref(),
        &[
            Message::user("first"),
            keel_core::summary_message("handoff"),
            Message::user("second"),
        ],
        &[],
    );
    let window = u32::try_from(next_input + output).expect("the test window fits u32");
    session.route.context_window = window;

    let error = session
        .step(Some("second"))
        .await
        .expect_err("an exactly full next request leaves no room");
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    assert!(error.message.contains("replacement view"), "{error}");
    assert!(error.message.contains("plus 2000 output"), "{error}");
    assert!(
        error.message.contains(&format!("{window}-token")),
        "{error}"
    );
    assert_eq!(
        provider.requests().await.len(),
        2,
        "the main request and the summary request ran; the exact-fit view was not sent"
    );
    assert!(session.take_compaction().is_none());
}

#[tokio::test]
async fn an_available_user_message_budget_boundary_still_compacts() {
    let oldest = Message::user("old");
    let newest = Message::user("newest");
    let pending = Message::user("current");
    let output = 1_000u64;
    let base_input =
        estimate_request_input_tokens(None, &[summary_message("handoff"), pending.clone()], &[]);
    let available = estimate_message_tokens(&oldest) + estimate_message_tokens(&newest);
    let window = u32::try_from(base_input + output + available).expect("window fits u32");

    let provider = FauxProvider::start(vec![text_turn("handoff"), text_turn("done")]).await;
    let mut session = build_session(&provider, window, output as u32);
    session.system = None;
    let mut assistant = Message::assistant(vec![Part::text("one")]);
    assistant.usage.input_tokens = 9_500;
    assistant.usage.output_tokens = 100;
    session.history = vec![oldest, newest.clone(), assistant];

    let outcome = session
        .step(Some("current"))
        .await
        .expect("the boundary view drops one older message instead of failing");
    assert_eq!(outcome.text(), "done");

    let stats = session.take_compaction().expect("compaction ran");
    assert_eq!(
        stats.kept_user_messages, 1,
        "the older message would land exactly on the available budget"
    );
    assert!(
        session
            .history
            .iter()
            .any(|message| message.text() == "newest")
    );
    assert!(
        !session
            .history
            .iter()
            .any(|message| message.text() == "old")
    );

    let next_input =
        estimate_request_input_tokens(None, &[newest, summary_message("handoff"), pending], &[]);
    assert!(
        next_input + output < u64::from(window),
        "the stored view must stay strictly below the window"
    );
}

#[tokio::test]
async fn a_view_fit_failure_names_the_output_budget_when_it_alone_is_too_large() {
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 500, 100),
        text_turn_with_usage("handoff", 100, 20),
    ])
    .await;
    let mut session = build_session(&provider, 1_000, 1_200);
    session.system = None;

    session.step(Some("first")).await.unwrap();
    let error = session
        .step(Some("second"))
        .await
        .expect_err("a 1,200-token output budget cannot fit a 1,000-token window");
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    assert!(error.message.contains("output budget alone"), "{error}");
    assert!(
        !error.message.contains("newest kept user message"),
        "{error}"
    );
    assert!(session.take_compaction().is_none());
}

#[tokio::test]
async fn a_view_fit_failure_names_a_pending_input_that_does_not_fit() {
    let giant = "x".repeat(4_000);
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 9_500, 100),
        text_turn_with_usage("handoff", 100, 20),
    ])
    .await;
    let mut session = build_session(&provider, 1_000, 100);
    session.system = None;

    session.step(Some("first")).await.unwrap();
    let error = session
        .step(Some(&giant))
        .await
        .expect_err("the pending input cannot fit the next request");
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    assert!(
        error.message.contains("pending input needs about"),
        "{error}"
    );
    assert!(error.message.contains("cannot be shortened"), "{error}");
    assert!(
        error.message.contains("input tokens plus 100 output"),
        "{error}"
    );
    assert!(session.take_compaction().is_none());
}

#[tokio::test]
async fn the_summary_request_uses_the_session_route_and_offers_no_tools() {
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 9_500, 100),
        text_turn_with_usage("handoff", 120, 30),
        text_turn("two"),
    ])
    .await;
    let mut session = build_session(&provider, 10_000, 1_000).with_tools(vec![ToolSpec {
        name: "read".to_owned(),
        description: "read a file".to_owned(),
        input_schema: json!({"type": "object"}),
    }]);

    session.step(Some("first")).await.unwrap();
    let outcome = session.step(Some("second")).await.unwrap();

    let requests = provider.requests().await;
    let summaries = summary_requests(&requests);
    assert_eq!(summaries.len(), 1);
    let summary = body(summaries[0]);

    assert_eq!(summary["model"], "gpt-4.1");
    assert_eq!(
        summary["max_tokens"], 2_500,
        "a 10,000-token window caps the summary at a quarter of it"
    );
    assert!(
        summary.get("prompt_cache_key").is_none(),
        "the one-shot summary sends no cache fields"
    );
    assert!(
        !summary.to_string().contains("cache_control"),
        "the one-shot summary sends no cache fields"
    );
    assert!(
        body(&requests[2]).get("prompt_cache_key").is_some(),
        "the session's Auto policy still applies to model turns"
    );
    let stats = session
        .take_compaction()
        .expect("the threshold compaction ran");
    assert_eq!(
        stats.usage.input_tokens, 120,
        "the summary request's usage is reported on the compaction"
    );
    assert_eq!(stats.usage.output_tokens, 30);
    assert_eq!(
        outcome.message.usage.input_tokens, 0,
        "side-request usage stays out of the assistant message"
    );
    assert_eq!(
        summary["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "system")
            .and_then(|message| message["content"].as_str()),
        Some(DEFAULT_SYSTEM_PROMPT)
    );
    assert_eq!(
        summary["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .count(),
        0
    );
    // The main request still advertises the tools.
    assert!(has_tools(&body(&requests[2])));
}

#[tokio::test]
async fn an_anthropic_compaction_summary_never_carries_cache_fields() {
    let provider = FauxProvider::start(vec![
        anthropic_text_turn("one", 9_500, 100),
        anthropic_text_turn("handoff", 120, 30),
        anthropic_text_turn("two", 10, 5),
    ])
    .await;
    let mut session =
        build_anthropic_session(&provider, 10_000, 1_000).with_cache(CachePolicy::Object(
            CacheObject::new()
                .breakpoint(CacheBreakpoint::System)
                .ttl(CacheTtl::OneHour),
        ));

    session.step(Some("first")).await.unwrap();
    session.step(Some("second")).await.unwrap();

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 3, "main, summary, compacted main");
    let main = body(&requests[0]);
    assert_eq!(
        main.pointer("/system/0/cache_control"),
        Some(&json!({"type": "ephemeral", "ttl": "1h"})),
        "the session policy is active on model turns"
    );
    assert_eq!(
        requests[0].header("anthropic-beta"),
        None,
        "the stable Messages API takes the 1h ttl without a beta header"
    );

    let summary = body(&requests[1]);
    assert_eq!(anthropic_last_message_text(&summary), COMPACTION_PROMPT);
    assert!(
        !summary.to_string().contains("cache_control"),
        "the summary request must not inherit the session's breakpoints"
    );
    assert_eq!(requests[1].header("anthropic-beta"), None);
}

#[tokio::test]
async fn summary_usage_does_not_inflate_the_next_trigger() {
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 9_500, 100),
        text_turn_with_usage("handoff", 9_000, 30),
        text_turn_with_usage("two", 200, 10),
        text_turn_with_usage("three", 300, 10),
    ])
    .await;
    let mut session = build_session(&provider, 10_000, 1_000);

    session.step(Some("first")).await.unwrap();
    let compacted = session.step(Some("second")).await.unwrap();
    assert_eq!(compacted.text(), "two");
    session
        .take_compaction()
        .expect("the threshold compaction ran");

    // The summary request reported 9,000 input tokens, which is most of the
    // window. It was a side request, so the next main request (200 + 10 + the
    // 1,000-token output budget) still fits and must not compact.
    let next = session.step(Some("third")).await.unwrap();
    assert_eq!(next.text(), "three");
    assert_eq!(
        summary_requests(&provider.requests().await).len(),
        1,
        "the summary request's usage must not count as the last main request"
    );
    assert!(
        session.take_compaction().is_none(),
        "the next step fits and does not compact"
    );
}

#[tokio::test]
async fn a_summary_request_that_cannot_fit_fails_without_sending() {
    let provider = FauxProvider::start(vec![text_turn_with_usage("one", 90, 5)]).await;
    let mut session = build_session(&provider, 100, 10);
    session.system = None;

    session.step(Some("first")).await.unwrap();
    let error = session
        .step(Some("second"))
        .await
        .expect_err("the summary prompt cannot fit a 100-token window");

    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    assert!(error.message.contains("cannot compact"), "{error}");
    assert!(error.message.contains("100-token"), "{error}");
    assert_eq!(
        provider.requests().await.len(),
        1,
        "nothing was sent after the fit check failed"
    );
    assert_eq!(session.history.len(), 2);
    assert!(session.take_compaction().is_none());
}

#[tokio::test]
async fn the_compacted_view_drops_calls_and_results_together() {
    let provider = FauxProvider::start(vec![
        text_turn("handoff: read the file already"),
        text_turn("done"),
    ])
    .await;
    let mut session = build_session(&provider, 10_000, 1_000);

    let source = Source {
        protocol: ProtocolId::OpenAiChat,
        provider: "test".into(),
        model: "gpt-4.1".into(),
    };
    let mut call = Message::assistant(vec![Part::tool_call(
        "call_1".into(),
        "read",
        "{\"path\":\"a\"}",
        source,
    )]);
    call.usage.input_tokens = 9_500;
    call.usage.output_tokens = 100;
    session.history = vec![
        Message::user("start"),
        call,
        Message::new(keel_llm_types::Role::User).with_part(Part::tool_result(
            "call_1".into(),
            "file contents",
            false,
        )),
        Message::user("next"),
    ];

    let outcome = session.step(Some("go")).await.unwrap();
    let requests = provider.requests().await;
    let compacted = body(&requests[1]);
    let texts = message_texts(&compacted);

    assert_eq!(texts[0], DEFAULT_SYSTEM_PROMPT);
    assert_eq!(texts[1], "start");
    assert_eq!(texts[2], "next");
    assert!(texts[3].starts_with(SUMMARY_PREFIX));
    assert_eq!(texts[4], "go");
    assert!(
        messages(&compacted).iter().all(|message| {
            let role = message["role"].as_str().unwrap_or_default();
            role == "system" || role == "user"
        }),
        "no assistant or tool message is kept: {compacted}"
    );
    assert!(
        !outcome
            .notes
            .iter()
            .any(|note| matches!(note, RepairNote::SynthesizedToolResult { .. })),
        "{:?}",
        outcome.notes
    );
    assert!(
        session
            .history
            .iter()
            .all(|message| !message.has_tool_calls() && message.tool_results().next().is_none()),
        "the compacted history has no tool traffic"
    );
}

#[tokio::test]
async fn resuming_a_compacted_session_sends_the_same_request() {
    let provider = FauxProvider::start(vec![
        text_turn_with_usage("one", 9_000, 300),
        text_turn_with_usage("two", 9_500, 100),
        text_turn("handoff: turns one and two are done"),
        text_turn("three"),
        text_turn("live next"),
        text_turn("resumed next"),
    ])
    .await;

    let directory = tempfile::tempdir().expect("temp dir");
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let route = route(
        format!("{}/v1/chat/completions", provider.url()),
        10_000,
        1_000,
    );
    let meta = SessionMeta::new("resume-compacted", "test", "gpt-4.1", "openai-chat")
        .system(Some(DEFAULT_SYSTEM_PROMPT.to_owned()))
        .max_output_tokens(Some(500))
        .temperature(Some(0.2));
    let log = Arc::new(SessionLog::create(directory.path(), &meta).expect("create log"));
    let mut live = Session::new(route.clone(), client.clone())
        .with_sink(log.clone())
        .with_max_output_tokens(500)
        .with_temperature(0.2);

    live.step(Some("first")).await.unwrap();
    live.step(Some("second")).await.unwrap();
    live.step(Some("third")).await.unwrap();

    let live_requests = provider.requests().await;
    let live_compacted = body(live_requests.last().expect("the compacted request"));
    assert_eq!(live_compacted["max_tokens"], 500);
    assert_eq!(live_compacted["temperature"], json!(0.2_f32));

    let (loaded_meta, messages) = store::read(log.path()).expect("read the log");
    assert_eq!(loaded_meta.max_output_tokens, Some(500));
    assert_eq!(loaded_meta.temperature, Some(0.2));
    assert_eq!(messages, live.history);

    let mut resumed = Session::new(route, client);
    resumed.history = messages;
    resumed.system = loaded_meta.system.clone();
    resumed.max_output_tokens = loaded_meta.max_output_tokens;
    resumed.temperature = loaded_meta.temperature;

    // Both sessions now hold the same resolved history. The next request from
    // each must be the same request.
    live.step(None).await.unwrap();
    resumed.step(None).await.unwrap();

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 6);
    let live_next = body(&requests[4]);
    let resumed_next = body(&requests[5]);
    assert_eq!(resumed_next, live_next);
}

#[tokio::test]
async fn a_long_session_keeps_going_past_the_window() {
    let steps = 20usize;
    let mut turns = vec![text_turn_with_usage("turn 0", 9_000, 100)];
    for step in 1..steps {
        turns.push(text_turn("handoff: earlier turns are summarized"));
        turns.push(text_turn_with_usage(&format!("turn {step}"), 9_000, 100));
    }
    let provider = FauxProvider::start(turns).await;
    let directory = tempfile::tempdir().expect("temp dir");
    let meta = SessionMeta::new("long-horizon", "test", "gpt-4.1", "openai-chat")
        .system(Some(DEFAULT_SYSTEM_PROMPT.to_owned()));
    let log = Arc::new(SessionLog::create(directory.path(), &meta).expect("create log"));
    let mut session = build_session(&provider, 10_000, 1_000).with_sink(log.clone());

    let mut compactions = 0;
    for step in 0..steps {
        session
            .step(Some(&format!("turn {step}")))
            .await
            .unwrap_or_else(|error| panic!("step {step}: {error}"));
        if let Some(stats) = session.take_compaction() {
            compactions += 1;
            assert!(stats.after_messages <= stats.before_messages);
            assert!(
                stats.estimated_tokens_after < 5_000,
                "the view stays far inside the window: {stats:?}"
            );
        }
        // Every reported request claims 9,100 input tokens, so the cumulative
        // reported input is far past the 10,000-token window.
        assert!(
            session.history.len() <= 25,
            "step {step}: {:?}",
            session.history.len()
        );
    }

    assert_eq!(compactions, steps - 1);
    let (_, view) = store::read(log.path()).expect("the log resolves");
    assert_eq!(view, session.history);
    assert_eq!(log.failure(), None);
    assert!(
        session
            .history
            .iter()
            .any(|message| message.text().starts_with(SUMMARY_PREFIX))
    );
}

fn test_source() -> Source {
    Source {
        protocol: ProtocolId::OpenAiChat,
        provider: "test".into(),
        model: "gpt-4.1".into(),
    }
}

fn write_call(id: &str, path: &str) -> Message {
    Message::assistant(vec![Part::tool_call(
        id.into(),
        "write",
        json!({"path": path, "content": "x"}).to_string(),
        test_source(),
    )])
}

fn bash_call(id: &str, command: &str) -> Message {
    Message::assistant(vec![Part::tool_call(
        id.into(),
        "bash",
        json!({"command": command}).to_string(),
        test_source(),
    )])
}

fn tool_result(id: &str, output: &str, is_error: bool) -> Message {
    Message::new(keel_llm_types::Role::User).with_part(Part::tool_result(
        id.into(),
        output,
        is_error,
    ))
}

fn assistant_with_usage(text: &str, input: u64, output: u64) -> Message {
    let mut message = Message::assistant(vec![Part::text(text)]);
    message.usage.input_tokens = input;
    message.usage.output_tokens = output;
    message
}

#[tokio::test]
async fn two_compactions_keep_a_fact_recorded_before_the_first() {
    let first_summary = "```json\n\
        {\"goal\":\"fix the parser\",\"plan\":[{\"step\":\"change the parser\",\"status\":\"done\"}],\
        \"decisions\":[{\"decision\":\"keep the tokenizer\",\"reason\":\"smaller diff\"}],\
        \"blockers\":[\"the test still fails\"],\"next_step\":\"run the focused test\"}\n\
        ```\n\nProgress: wrote src/parser.rs and ran the focused test.";
    let provider = FauxProvider::start(vec![
        text_turn(first_summary),
        text_turn_with_usage("second main turn", 9_000, 100),
        text_turn("handoff: the second cut has no new file facts"),
        text_turn("third main turn"),
    ])
    .await;
    let mut session = build_session(&provider, 10_000, 1_000);
    session.history = vec![
        Message::user("start"),
        write_call("write_1", "src/parser.rs"),
        tool_result("write_1", "wrote 12 bytes to src/parser.rs", false),
        bash_call("bash_1", "cargo test parser"),
        tool_result("bash_1", "exit: 0\ntest result: ok. 3 passed", false),
        assistant_with_usage("first main turn", 9_500, 100),
    ];

    session.step(Some("second")).await.unwrap();
    let first = session.take_compaction().expect("the first cut ran");
    assert_eq!(first.memory.compactions_so_far, 1);
    assert_eq!(first.memory.files_remembered, 1);
    assert_eq!(first.memory.commands_remembered, 1);
    assert!(first.memory.memory_bytes > 0);
    assert!(first.memory.memory_tokens > 0);

    session.step(Some("third")).await.unwrap();
    let second = session.take_compaction().expect("the second cut ran");
    assert_eq!(second.memory.compactions_so_far, 2);

    let memory = session.working_memory().expect("working memory survives");
    assert_eq!(
        memory.files,
        ["src/parser.rs"],
        "a fact recorded before the first cut is still present after the second"
    );
    assert_eq!(memory.commands.len(), 1);
    assert_eq!(memory.narrative.goal, "fix the parser");
    assert_eq!(memory.narrative.plan[0].status, "done");
    assert_eq!(memory.narrative.decisions[0].decision, "keep the tokenizer");
    assert_eq!(memory.narrative.blockers, ["the test still fails"]);
    assert_eq!(memory.compactions, 2);

    let requests = provider.requests().await;
    assert_eq!(
        requests.len(),
        4,
        "summary, main, summary, main across the two cuts"
    );
    let second_summary = body(&requests[2]).to_string();
    assert!(
        second_summary.contains(WORKING_MEMORY_HEADER),
        "the second summary request receives the previous memory: {second_summary}"
    );
    assert!(
        second_summary.contains("src/parser.rs"),
        "the previous file fact is an input to the second cut: {second_summary}"
    );
    let final_request = body(&requests[3]).to_string();
    assert!(
        final_request.contains(WORKING_MEMORY_HEADER) && final_request.contains("src/parser.rs"),
        "the injected block still carries the old fact: {final_request}"
    );
    assert!(
        session
            .history
            .iter()
            .find(|message| message.text().starts_with(SUMMARY_PREFIX))
            .expect("the view carries the summary")
            .text()
            .contains("src/parser.rs")
    );
}

#[tokio::test]
async fn a_malformed_structured_block_falls_back_without_losing_observed_facts() {
    let malformed = "```json\n{\"goal\":\n```\nProgress: wrote src/parser.rs.";
    let provider = FauxProvider::start(vec![text_turn(malformed), text_turn("next")]).await;
    let mut session = build_session(&provider, 10_000, 1_000);
    session.history = vec![
        Message::user("start"),
        write_call("write_1", "src/parser.rs"),
        tool_result("write_1", "wrote 12 bytes to src/parser.rs", false),
        assistant_with_usage("first main turn", 9_500, 100),
    ];

    let outcome = session.step(Some("continue")).await.unwrap();
    assert_eq!(
        outcome.text(),
        "next",
        "the malformed block is not an error"
    );

    let memory = session.working_memory().expect("the cut still ran");
    assert!(
        memory.narrative.is_empty(),
        "the malformed block is ignored"
    );
    assert_eq!(
        memory.files,
        ["src/parser.rs"],
        "deterministic facts do not depend on the model's block"
    );
    let summary = session
        .history
        .iter()
        .find(|message| message.text().starts_with(SUMMARY_PREFIX))
        .expect("the summary message")
        .text();
    assert!(
        summary.contains("Progress: wrote src/parser.rs."),
        "the whole response is the fallback prose: {summary}"
    );
    assert!(
        summary.contains(WORKING_MEMORY_HEADER) && summary.contains("src/parser.rs"),
        "the rendered block is still appended: {summary}"
    );
}

#[tokio::test]
async fn a_resumed_session_carries_its_working_memory_on_the_first_request() {
    let provider = FauxProvider::start(vec![text_turn("resumed")]).await;
    let directory = tempfile::tempdir().expect("temp dir");
    let meta = SessionMeta::new("resume-memory", "test", "gpt-4.1", "openai-chat");
    let log = SessionLog::create(directory.path(), &meta).expect("create log");
    log.append(&Message::user("start")).expect("append");
    let summary = summary_message("handoff: the first cut is done");
    let memory = WorkingMemory {
        files: vec!["src/kept.rs".to_owned()],
        commands: Vec::new(),
        narrative: keel_core::NarrativeMemory {
            goal: "keep the resumed goal".to_owned(),
            next_step: "continue the change".to_owned(),
            ..keel_core::NarrativeMemory::default()
        },
        test_status: None,
        compactions: 1,
    };
    let (_, stats) = memory.render_with_stats();
    log.append_compaction(&keel_core::CompactionRecord {
        reason: CompactionReason::Threshold,
        summary: summary.clone(),
        messages: vec![summary],
        working_memory: Some(memory.clone()),
        memory: Some(stats),
    })
    .expect("append compaction");
    drop(log);

    let (loaded_meta, messages, loaded_memory) =
        store::read_with_state(&store::path_for(directory.path(), "resume-memory"))
            .expect("read state");
    assert_eq!(messages.len(), 1);
    assert_eq!(loaded_memory, Some(memory.clone()));

    let mut resumed = build_session(&provider, 10_000, 1_000);
    resumed.history = messages;
    resumed.system = loaded_meta.system.clone();
    resumed.restore_working_memory(loaded_memory.expect("memory is restored"));
    // The stored summary predates the block format, so the request-scoped
    // injection is what makes the first request carry it.
    resumed.step(None).await.unwrap();

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 1, "the first request after the resume");
    let request = body(&requests[0]).to_string();
    assert!(
        request.contains(WORKING_MEMORY_HEADER),
        "the resumed request carries the block: {request}"
    );
    assert!(
        request.contains("src/kept.rs") && request.contains("keep the resumed goal"),
        "the resumed request carries the remembered fact and goal: {request}"
    );
}

#[tokio::test]
async fn a_session_that_never_compacted_sends_no_working_memory() {
    let provider = FauxProvider::start(vec![text_turn("one")]).await;
    let mut session = build_session(&provider, 10_000, 1_000);

    session.step(Some("first")).await.unwrap();

    assert!(session.working_memory().is_none());
    let requests = provider.requests().await;
    assert_eq!(requests.len(), 1);
    let request = requests[0].json().to_string();
    assert!(
        !request.contains(WORKING_MEMORY_HEADER),
        "a never-compacted session has no memory block: {request}"
    );
    assert!(
        session
            .history
            .iter()
            .all(|message| !message.text().contains(WORKING_MEMORY_HEADER)),
        "no memory message is added to a never-compacted history"
    );
}
