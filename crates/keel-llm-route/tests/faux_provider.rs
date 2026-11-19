//! Transport tests driven by the scripted faux provider.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::{
    Auth, Client, CredentialRef, EventStream, Framing, RetryPolicy, Route, SecretResolver,
};
use keel_llm_types::{Compat, LlmError, LlmEvent, Message, ProtocolId, Request, StopReason};
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
