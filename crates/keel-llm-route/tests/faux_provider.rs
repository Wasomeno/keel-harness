//! Transport tests driven by the scripted faux provider.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use keel_llm_protocol::anthropic_messages::AnthropicMessages;
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_protocol::openai_responses::OpenAiResponses;
use keel_llm_route::{
    Auth, Client, CredentialRef, EventStream, Framing, RetryPolicy, Route, SecretResolver,
};
use keel_llm_types::{
    CacheBreakpoint, CacheObject, CachePolicy, Compat, ErrorKind, LlmError, LlmEvent, Message,
    ProtocolId, Request, StopReason,
};
use keel_testkit::{Chunk, FauxProvider, Turn};

fn route(endpoint: impl Into<String>) -> Route {
    Route {
        provider: "test".into(),
        model: "gpt-4.1".into(),
        endpoint: endpoint.into(),
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

fn anthropic_route(endpoint: impl Into<String>) -> Route {
    Route {
        provider: "test".into(),
        model: "claude-sonnet-4-5".into(),
        endpoint: endpoint.into(),
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

fn responses_route(endpoint: impl Into<String>) -> Route {
    Route {
        provider: "test".into(),
        model: "gpt-5.5".into(),
        endpoint: endpoint.into(),
        protocol: Arc::new(OpenAiResponses::new(Compat::for_protocol(
            ProtocolId::OpenAiResponses,
        ))),
        auth: Auth::None,
        framing: Framing::Sse,
        headers: Vec::new(),
        context_window: 0,
        max_output_tokens: 0,
    }
}

fn request() -> Request {
    Request::new("gpt-4.1").messages(vec![Message::user("hi")])
}

fn text_frame(text: &str) -> Chunk {
    Chunk::text(format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}},\"finish_reason\":null}}]}}\n\n",
        serde_json::json!(text)
    ))
}

fn stop_frame() -> Chunk {
    Chunk::text("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n")
}

fn done_frame() -> Chunk {
    Chunk::text("data: [DONE]\n\n")
}

struct Fixed;

impl SecretResolver for Fixed {
    fn resolve(&self, _reference: &CredentialRef) -> Option<String> {
        Some("sk-test".to_owned())
    }
}

#[tokio::test]
async fn holds_a_chunk_until_the_test_releases_it() {
    let (second, release) = Chunk::gated(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
    );
    let provider = FauxProvider::start(vec![Turn::sse(vec![
        text_frame("hel"),
        second,
        stop_frame(),
        done_frame(),
    ])])
    .await;
    let client = Client::new().unwrap();

    let mut stream = client
        .stream(&route(provider.url()), &request())
        .await
        .unwrap();

    let mut seen = Vec::new();
    while seen.len() < 2 {
        let event = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("start and the first delta arrive")
            .unwrap()
            .unwrap();
        seen.push(event);
    }
    assert!(matches!(seen[0], LlmEvent::Start { .. }));
    assert!(matches!(&seen[1], LlmEvent::TextDelta { delta, .. } if delta == "hel"));

    // The second chunk is gated, so nothing may arrive while it is held.
    assert!(
        tokio::time::timeout(Duration::from_millis(150), stream.next())
            .await
            .is_err(),
        "a gated chunk leaked before it was released"
    );

    release.send(()).unwrap();
    let (text, stop) = drain(stream).await;
    assert_eq!(format!("{}{}", seen_text(&seen), text), "hello");
    assert_eq!(stop, Some(StopReason::EndTurn));
}

#[tokio::test]
async fn a_truncated_stream_is_closed_with_a_warning() {
    let provider = FauxProvider::start(vec![Turn::sse(vec![text_frame("partial")])]).await;
    let client = Client::new().unwrap();
    let mut stream = client
        .stream(&route(provider.url()), &request())
        .await
        .unwrap();

    let mut text = String::new();
    let mut stop = None;
    let mut warnings = Vec::new();
    while let Some(item) = stream.next().await {
        match item.expect("a truncated stream is not an error") {
            LlmEvent::TextDelta { delta, .. } => text.push_str(&delta),
            LlmEvent::Done { stop: reason } => stop = Some(reason),
            LlmEvent::Warning(warning) => warnings.push(warning),
            _ => {}
        }
    }

    assert_eq!(text, "partial");
    assert_eq!(stop, Some(StopReason::EndTurn));
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("without a finish reason")),
        "a truncated stream must warn: {warnings:?}"
    );
}

#[tokio::test]
async fn dropped_cache_breakpoints_are_visible_warnings_at_stream_start() {
    let provider = FauxProvider::start(vec![Turn::sse(vec![
        Chunk::text(
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":1}}}\n\n",
        ),
        Chunk::text("data: {\"type\":\"message_stop\"}\n\n"),
    ])])
    .await;
    let client = Client::new().unwrap();
    let request = Request::new("claude-sonnet-4-5")
        .system("be terse")
        .messages(vec![Message::user("hi")])
        .cache(CachePolicy::Object(
            CacheObject::new()
                .breakpoint(CacheBreakpoint::System)
                .breakpoint(CacheBreakpoint::Message(9)),
        ));

    let mut stream = client
        .stream(&anthropic_route(provider.url()), &request)
        .await
        .unwrap();

    let first = stream.next().await.unwrap().unwrap();
    let LlmEvent::Warning(warning) = first else {
        panic!("the cache warning must come first, got {first:?}");
    };
    assert!(
        warning.contains("dropped cache breakpoint message[9]"),
        "warning does not name the dropped breakpoint: {warning}"
    );

    while stream.next().await.is_some() {}
    provider.wait_for_requests(1).await;
    let recorded = provider.requests().await;
    let body = recorded.first().expect("one request").json();
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert!(
        body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );
}

#[tokio::test]
async fn retries_a_429_before_the_stream_opens() {
    let provider = FauxProvider::start(vec![
        Turn::json(r#"{"error":{"message":"slow down"}}"#)
            .with_status(429)
            .header("retry-after", "0"),
        Turn::sse(vec![stop_frame(), done_frame()]),
    ])
    .await;
    let client = Client::new().unwrap();
    let policy = RetryPolicy {
        max_attempts: 2,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    };

    let stream = client
        .stream_with_retry(&route(provider.url()), &request(), &policy)
        .await
        .expect("the second attempt succeeds");
    let (_, stop) = drain(stream).await;

    assert_eq!(stop, Some(StopReason::EndTurn));
    assert_eq!(provider.request_count().await, 2);
}

#[tokio::test]
async fn records_the_request_and_the_resolved_credential() {
    let provider = FauxProvider::start(vec![Turn::sse(vec![stop_frame(), done_frame()])]).await;
    let client = Client::with_resolver(Arc::new(Fixed)).unwrap();
    let mut route = route(provider.url());
    route.auth = Auth::bearer(CredentialRef::from_env("KEEL_TEST_KEY"));

    let stream = client.stream(&route, &request()).await.unwrap();
    drain(stream).await;
    provider.wait_for_requests(1).await;

    let recorded = provider.requests().await;
    let first = recorded.first().expect("one request");
    assert_eq!(first.method, "POST");
    assert_eq!(first.header("authorization"), Some("Bearer sk-test"));
    assert_eq!(first.header("content-type"), Some("application/json"));

    let body = first.json();
    assert_eq!(body["model"], "gpt-4.1");
    assert_eq!(body["stream"], true);
    assert_eq!(body["messages"][0]["content"], "hi");
}

#[tokio::test]
async fn a_stalled_response_produces_nothing_until_the_provider_stops() {
    let provider = FauxProvider::start(vec![Turn::stalled()]).await;
    let client = Client::new().unwrap();
    let mut stream = client
        .stream(&route(provider.url()), &request())
        .await
        .unwrap();

    assert!(
        tokio::time::timeout(Duration::from_millis(150), stream.next())
            .await
            .is_err(),
        "a stalled response must not produce events"
    );

    drop(provider);
    let ended = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(item) = stream.next().await {
            if item.is_err() {
                return true;
            }
        }
        true
    })
    .await;
    assert!(ended.is_ok(), "dropping the provider must end the stream");
}

#[tokio::test]
async fn a_stalled_body_times_out_instead_of_hanging() {
    let provider = FauxProvider::start(vec![Turn::stalled()]).await;
    let client = Client::new()
        .unwrap()
        .with_stream_idle_timeout(Some(Duration::from_millis(100)));
    let mut stream = client
        .stream(&route(provider.url()), &request())
        .await
        .unwrap();

    let item = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("the idle timeout fires")
        .expect("the stream yields the timeout");
    let error = item.expect_err("a stalled body is an error");
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_eq!(error.message, "no data from the provider for 100ms");
    assert!(
        tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("the stream ends")
            .is_none(),
        "the stream must end after the timeout error"
    );
    assert_eq!(provider.request_count().await, 1);
}

#[tokio::test]
async fn chunks_spaced_below_the_idle_timeout_complete() {
    let gap = Duration::from_millis(40);
    let mut chunks: Vec<Chunk> = (0..8).map(|_| text_frame("a").with_delay(gap)).collect();
    chunks.push(stop_frame().with_delay(gap));
    chunks.push(done_frame().with_delay(gap));
    let provider = FauxProvider::start(vec![Turn::sse(chunks)]).await;
    let client = Client::new()
        .unwrap()
        .with_stream_idle_timeout(Some(Duration::from_millis(250)));
    let stream = client
        .stream(&route(provider.url()), &request())
        .await
        .unwrap();

    // The chunks together take much longer than the timeout; each individual
    // gap is well below it, so only a per-chunk timer lets the stream finish.
    let (text, stop) = tokio::time::timeout(Duration::from_secs(3), drain(stream))
        .await
        .expect("each gap is below the idle timeout");
    assert_eq!(text, "aaaaaaaa");
    assert_eq!(stop, Some(StopReason::EndTurn));
}

#[tokio::test]
async fn a_timeout_after_body_bytes_is_not_retried_by_the_transport() {
    let (held, _release) = Chunk::gated(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"never\"},\"finish_reason\":null}]}\n\n",
    );
    let provider = FauxProvider::start(vec![
        Turn::sse(vec![text_frame("partial"), held]),
        Turn::sse(vec![stop_frame(), done_frame()]),
    ])
    .await;
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    };
    let client = Client::new()
        .unwrap()
        .with_stream_idle_timeout(Some(Duration::from_millis(100)));
    let mut stream = client
        .stream_with_retry(&route(provider.url()), &request(), &policy)
        .await
        .expect("the response status is known before the stall");

    let mut saw_partial = false;
    while !saw_partial {
        let event = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("the first chunk arrives")
            .unwrap()
            .unwrap();
        if let LlmEvent::TextDelta { delta, .. } = &event {
            assert_eq!(delta, "partial");
            saw_partial = true;
        }
    }

    let item = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("the idle timeout fires")
        .unwrap();
    let error = item.expect_err("the stalled tail is an error");
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_eq!(
        provider.request_count().await,
        1,
        "the transport must not retry after body bytes have started"
    );
}

#[tokio::test]
async fn a_disabled_idle_timeout_leaves_a_stall_alone() {
    let provider = FauxProvider::start(vec![Turn::stalled()]).await;
    let client = Client::new()
        .unwrap()
        // A no-op setter would leave the 100 ms bound armed.
        .with_stream_idle_timeout(Some(Duration::from_millis(100)))
        .with_stream_idle_timeout(None);
    let mut stream = client
        .stream(&route(provider.url()), &request())
        .await
        .unwrap();

    assert!(
        tokio::time::timeout(Duration::from_millis(250), stream.next())
            .await
            .is_err(),
        "a disabled idle timeout must not fire"
    );

    drop(provider);
    let ended = tokio::time::timeout(Duration::from_secs(2), async {
        while stream.next().await.is_some() {}
    })
    .await;
    assert!(ended.is_ok(), "dropping the provider must end the stream");
}

#[tokio::test]
async fn a_complete_turn_ends_without_waiting_for_the_body_to_close() {
    let completed = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_test\",\"status\":\"completed\",\"output\":[]}}\n\n";
    let (held, _release) = Chunk::gated(
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"late\"}\n\n",
    );
    let provider = FauxProvider::start(vec![Turn::sse(vec![Chunk::text(completed), held])]).await;
    let client = Client::new()
        .unwrap()
        .with_stream_idle_timeout(Some(Duration::from_millis(100)));
    let mut stream = client
        .stream(&responses_route(provider.url()), &request())
        .await
        .unwrap();

    let mut saw_done = false;
    while let Some(item) = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("a complete turn must not wait for the open body")
    {
        if let LlmEvent::Done { .. } = item.expect("a complete turn has no stream error") {
            saw_done = true;
        }
    }

    assert!(saw_done, "the terminal event must reach the caller");
    assert_eq!(provider.request_count().await, 1);
}

#[tokio::test]
async fn a_stalled_error_body_is_bounded() {
    let provider = FauxProvider::start(vec![Turn::stalled().with_status(500)]).await;
    let client = Client::new()
        .unwrap()
        .with_stream_idle_timeout(Some(Duration::from_millis(100)));
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        client.stream(&route(provider.url()), &request()),
    )
    .await
    .expect("a stalled error body must not hang the request")
    .expect_err("HTTP 500 is an error");

    assert_eq!(error.kind, ErrorKind::ServerError);
    assert_eq!(error.status, Some(500));
    assert!(error.message.contains("error body"), "{}", error.message);
    assert!(error.message.contains("100ms"), "{}", error.message);
}

#[tokio::test]
async fn a_stalled_error_body_does_not_block_the_pre_stream_retry() {
    let provider = FauxProvider::start(vec![
        Turn::stalled().with_status(500),
        Turn::sse(vec![stop_frame(), done_frame()]),
    ])
    .await;
    let policy = RetryPolicy {
        max_attempts: 2,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    };
    let client = Client::new()
        .unwrap()
        .with_stream_idle_timeout(Some(Duration::from_millis(100)));
    let stream = tokio::time::timeout(
        Duration::from_secs(2),
        client.stream_with_retry(&route(provider.url()), &request(), &policy),
    )
    .await
    .expect("the retry loop must not hang")
    .expect("the second attempt succeeds");

    let (_, stop) = drain(stream).await;
    assert_eq!(stop, Some(StopReason::EndTurn));
    assert_eq!(provider.request_count().await, 2);
}

#[tokio::test]
async fn an_absurd_idle_timeout_is_clamped_instead_of_panicking() {
    let (held, _release) = Chunk::gated(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"never\"},\"finish_reason\":null}]}\n\n",
    );
    let provider = FauxProvider::start(vec![Turn::sse(vec![text_frame("partial"), held])]).await;
    let client = Client::new()
        .unwrap()
        .with_stream_idle_timeout(Some(Duration::MAX));
    let mut stream = client
        .stream(&route(provider.url()), &request())
        .await
        .unwrap();

    let mut saw_text = false;
    while !saw_text {
        let event = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("the first chunk arrives")
            .unwrap()
            .unwrap();
        if let LlmEvent::TextDelta { .. } = event {
            saw_text = true;
        }
    }

    assert!(
        tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .is_err(),
        "a clamped deadline must not fire immediately"
    );
}

async fn drain(stream: EventStream) -> (String, Option<StopReason>) {
    let (text, stop, error) = drain_all(stream).await;
    assert!(error.is_none(), "unexpected stream error: {error:?}");
    (text, stop)
}

async fn drain_all(mut stream: EventStream) -> (String, Option<StopReason>, Option<LlmError>) {
    let mut text = String::new();
    let mut stop = None;
    let mut error = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(LlmEvent::TextDelta { delta, .. }) => text.push_str(&delta),
            Ok(LlmEvent::Done { stop: reason }) => stop = Some(reason),
            Ok(_) => {}
            Err(caught) => {
                error = Some(caught);
                break;
            }
        }
    }
    (text, stop, error)
}

fn seen_text(events: &[LlmEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}
