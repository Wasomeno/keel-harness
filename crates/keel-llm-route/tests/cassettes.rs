//! Cassettes replayed through the real client.
//!
//! Each cassette pairs the exact request keel encodes with the raw response a
//! provider sent. Replay goes through `Client`, the route, the SSE framing and
//! the protocol decoder, so a changed request, a changed endpoint or a broken
//! decoder fails here. The request half is regenerated from the encoder:
//!
//! ```text
//! UPDATE_CASSETTE=1 cargo test -p keel-llm-route --test cassettes
//! ```
//!
//! Response bytes are not regenerated; they come from `fixtures/recorded` (see
//! that directory's `PROVENANCE.md`), except the live capture, which
//! `record_live` wrote from a real provider response.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use keel_llm_protocol::Protocol;
use keel_llm_protocol::anthropic_messages::AnthropicMessages;
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_protocol::openai_responses::OpenAiResponses;
use keel_llm_route::{Auth, Client, EnvResolver, Framing, Route, SseDecoder};
use keel_llm_types::{
    Compat, LlmEvent, Message, ProtocolId, ReasoningEffort, Request, StopReason, ToolCall,
    ToolSpec, Usage,
};
use keel_testkit::{Cassette, CassetteRequest, CassetteResponse, Interaction};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const RESPONSES_CASSETTE: &str = "openai-responses/gpt-5-5-reasoning.json";
const CHAT_CASSETTE: &str = "openai-chat/azure-text.json";
const LIVE_CASSETTE: &str = "neuralwatt/chat-text.json";
const LIVE_TOOL_CASSETTE: &str = "neuralwatt/chat-tool-call.json";
const LIVE_RESPONSES_CASSETTE: &str = "neuralwatt/responses-text.json";
const LIVE_REASONING_CASSETTE: &str = "neuralwatt/responses-reasoning.json";
const LIVE_MESSAGES_CASSETTE: &str = "neuralwatt/messages-text.json";

const PROVENANCE: &str = "request encoded by keel; response body from \
     sst/opencode@a9a35b4b4ce35573d71207e246e770cfa26ff0e1 (MIT)";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cassettes")
}

fn load_or_record(relative: &str, make: impl FnOnce() -> Cassette) -> Cassette {
    let path = fixtures().join(relative);
    if std::env::var_os("UPDATE_CASSETTE").is_some() {
        make().save(&path).expect("write the cassette");
    }
    Cassette::load(&path).unwrap_or_else(|error| {
        panic!(
            "cassette {} could not be read: {error}; run with UPDATE_CASSETTE=1",
            path.display()
        )
    })
}

/// Loads a live capture, replacing only its request half from the encoder when
/// `UPDATE_CASSETTE` is set. The response bytes are left exactly as recorded.
fn load_live(
    relative: &str,
    protocol: ProtocolId,
    request: &Request,
    request_path: &str,
) -> Cassette {
    let path = fixtures().join(relative);
    let mut cassette = Cassette::load(&path)
        .unwrap_or_else(|error| panic!("cassette {} could not be read: {error}", path.display()));
    if std::env::var_os("UPDATE_CASSETTE").is_some() {
        let body = encoded_body_compact(protocol, request);
        let interaction = cassette
            .interactions
            .first_mut()
            .expect("a live cassette has one interaction");
        assert_eq!(interaction.request.path, request_path);
        let length = body.len();
        interaction.request.body = body;
        update_content_length(&mut interaction.request.headers, length);
        cassette.save(&path).expect("write the cassette");
    }
    cassette
}

/// Keeps the recorded header in step with a re-encoded live body. The
/// recorder writes it, so an update that only replaced the body would leave
/// the fixture internally inconsistent.
fn update_content_length(headers: &mut Vec<(String, String)>, length: usize) {
    if let Some((_, value)) = headers
        .iter_mut()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
    {
        *value = length.to_string();
    } else {
        headers.push(("content-length".to_owned(), length.to_string()));
    }
}

fn responses_request() -> Request {
    let mut request = Request::new("gpt-5.5");
    request.system = Some("You are terse.".to_owned());
    request.max_output_tokens = Some(256);
    request.messages = vec![Message::user("Hello!")];
    request
}

fn chat_request() -> Request {
    Request::new("gpt-4.1").messages(vec![Message::user("hi")])
}

fn live_request(prompt: &str, max_output_tokens: u32) -> Request {
    let mut request = Request::new("nw-small");
    request.system = Some("You are terse.".to_owned());
    request.max_output_tokens = Some(max_output_tokens);
    request.messages = vec![Message::user(prompt)];
    request
}

fn live_tool_request() -> Request {
    let mut request = live_request("What is the weather in Paris? Use the tool.", 512);
    request.tools = vec![ToolSpec {
        name: "get_weather".to_owned(),
        description: "Report the weather for one city.".to_owned(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }),
    }];
    request
}

fn encoded_request(protocol: ProtocolId, request: &Request) -> keel_llm_protocol::EncodedRequest {
    match protocol {
        ProtocolId::OpenAiResponses => {
            OpenAiResponses::new(Compat::for_protocol(protocol)).encode(request)
        }
        ProtocolId::OpenAiChat => OpenAiChat::new(Compat::for_protocol(protocol)).encode(request),
        ProtocolId::AnthropicMessages => {
            AnthropicMessages::new(Compat::for_protocol(protocol)).encode(request)
        }
        other => panic!("no cassette request for {other:?}"),
    }
    .expect("the request encodes")
}

fn encoded_body(protocol: ProtocolId, request: &Request) -> (Vec<(String, String)>, String) {
    let encoded = encoded_request(protocol, request);
    let body = serde_json::to_string_pretty(&encoded.body).expect("the body serializes");
    (encoded.headers, body)
}

/// Live captures recorded a compact body. Keep that shape so a refresh only
/// changes the fields the encoder actually changed.
fn encoded_body_compact(protocol: ProtocolId, request: &Request) -> String {
    let encoded = encoded_request(protocol, request);
    serde_json::to_string(&encoded.body).expect("the body serializes")
}

fn responses_cassette() -> Cassette {
    let (headers, body) = encoded_body(ProtocolId::OpenAiResponses, &responses_request());
    Cassette::builder("openai-responses/gpt-5-5-reasoning")
        .provider("openai")
        .protocol("openai-responses")
        .model("gpt-5.5-2026-04-23")
        .source(PROVENANCE)
        .interaction(Interaction::new(
            CassetteRequest::new("POST", "/v1/responses", headers, body),
            CassetteResponse::sse(include_str!(
                "fixtures/recorded/openai-responses/gpt-5-5-reasoning.sse"
            )),
        ))
        .build()
}

fn chat_cassette() -> Cassette {
    let (headers, body) = encoded_body(ProtocolId::OpenAiChat, &chat_request());
    Cassette::builder("openai-chat/azure-text")
        .provider("azure")
        .protocol("openai-chat")
        .model("gpt-4.1")
        .source(PROVENANCE)
        .interaction(Interaction::new(
            CassetteRequest::new("POST", "/v1/chat/completions", headers, body),
            CassetteResponse::sse(include_str!("fixtures/recorded/openai-chat/azure-text.sse")),
        ))
        .build()
}

fn protocol(id: ProtocolId) -> Arc<dyn Protocol> {
    match id {
        ProtocolId::OpenAiResponses => Arc::new(OpenAiResponses::new(Compat::for_protocol(id))),
        ProtocolId::OpenAiChat => Arc::new(OpenAiChat::new(Compat::for_protocol(id))),
        ProtocolId::AnthropicMessages => Arc::new(AnthropicMessages::new(Compat::for_protocol(id))),
        other => panic!("no route for {other:?}"),
    }
}

fn route(id: ProtocolId, model: &str, endpoint: impl Into<String>) -> Route {
    Route {
        provider: "cassette".into(),
        model: model.into(),
        endpoint: endpoint.into(),
        protocol: protocol(id),
        auth: Auth::None,
        framing: Framing::Sse,
        headers: Vec::new(),
        context_window: 0,
        max_output_tokens: 0,
    }
}

fn client() -> Client {
    Client::with_resolver(Arc::new(EnvResolver)).expect("client")
}

#[derive(Debug, Default)]
struct Decoded {
    text: String,
    signatures: Vec<String>,
    calls: Vec<ToolCall>,
    stop: Option<StopReason>,
    usage: Usage,
    warnings: Vec<String>,
}

async fn decode(mut stream: keel_llm_route::EventStream) -> Decoded {
    let mut decoded = Decoded::default();
    while let Some(event) = stream.next().await {
        match event.expect("the replayed stream decodes") {
            LlmEvent::TextDelta { delta, .. } => decoded.text.push_str(&delta),
            LlmEvent::ReasoningEnd {
                signature: Some(signature),
                ..
            } => decoded.signatures.push(signature),
            LlmEvent::ToolCall { call, .. } => decoded.calls.push(call),
            LlmEvent::Done { stop } => decoded.stop = Some(stop),
            LlmEvent::Usage(usage) => decoded.usage = usage,
            LlmEvent::Warning(warning) => decoded.warnings.push(warning),
            _ => {}
        }
    }
    decoded
}

#[tokio::test]
async fn replays_a_recorded_responses_turn() {
    let cassette = load_or_record(RESPONSES_CASSETTE, responses_cassette);
    assert_eq!(cassette.name, "openai-responses/gpt-5-5-reasoning");

    let server = cassette.start().await;
    let endpoint = format!("{}/v1/responses", server.url());
    let stream = client()
        .stream(
            &route(ProtocolId::OpenAiResponses, "gpt-5.5", endpoint),
            &responses_request(),
        )
        .await
        .expect("the cassette answers");
    let decoded = decode(stream).await;

    assert_eq!(decoded.text, "Hello!");
    assert_eq!(decoded.stop, Some(StopReason::EndTurn));
    assert_eq!(decoded.usage.input_tokens, 31);
    assert_eq!(decoded.usage.output_tokens, 21);
    assert_eq!(decoded.usage.reasoning_tokens, 13);
    assert_eq!(decoded.signatures.len(), 1);
    let signature: serde_json::Value =
        serde_json::from_str(&decoded.signatures[0]).expect("the signature is JSON");
    assert!(signature["id"].as_str().unwrap().starts_with("rs_"));

    assert_eq!(server.hits(), vec![0]);
    server.assert_no_misses();
}

#[tokio::test]
async fn replays_a_recorded_chat_turn() {
    let cassette = load_or_record(CHAT_CASSETTE, chat_cassette);
    let server = cassette.start().await;
    let endpoint = format!("{}/v1/chat/completions", server.url());

    let stream = client()
        .stream(
            &route(ProtocolId::OpenAiChat, "gpt-4.1", endpoint),
            &chat_request(),
        )
        .await
        .expect("the cassette answers");
    let decoded = decode(stream).await;

    assert_eq!(decoded.text, "hello");
    assert_eq!(decoded.stop, Some(StopReason::EndTurn));
    assert_eq!(decoded.usage.input_tokens, 13);
    assert_eq!(decoded.usage.output_tokens, 5);
    assert!(decoded.warnings.is_empty());
    server.assert_no_misses();
}

#[tokio::test]
async fn the_live_neuralwatt_capture_decodes() {
    let request = live_request("Reply with exactly one word: pong", 64);
    let cassette = load_live(
        LIVE_CASSETTE,
        ProtocolId::OpenAiChat,
        &request,
        "/v1/chat/completions",
    );
    let server = cassette.start().await;

    let stream = client()
        .stream(
            &route(
                ProtocolId::OpenAiChat,
                "nw-small",
                format!("{}/v1/chat/completions", server.url()),
            ),
            &request,
        )
        .await
        .expect("the cassette answers");
    let decoded = decode(stream).await;

    assert_eq!(decoded.text, "pong");
    assert_eq!(decoded.stop, Some(StopReason::EndTurn));
    assert_eq!(decoded.usage.input_tokens, 64);
    assert_eq!(decoded.usage.output_tokens, 35);
    assert_eq!(decoded.usage.reasoning_tokens, 30);
    assert!(
        decoded.warnings.is_empty(),
        "the energy trailer and the comment lines are not warnings: {:?}",
        decoded.warnings
    );
    server.assert_no_misses();
}

#[tokio::test]
async fn the_live_neuralwatt_tool_call_decodes() {
    let request = live_tool_request();
    let cassette = load_live(
        LIVE_TOOL_CASSETTE,
        ProtocolId::OpenAiChat,
        &request,
        "/v1/chat/completions",
    );
    let server = cassette.start().await;

    let stream = client()
        .stream(
            &route(
                ProtocolId::OpenAiChat,
                "nw-small",
                format!("{}/v1/chat/completions", server.url()),
            ),
            &request,
        )
        .await
        .expect("the cassette answers");
    let decoded = decode(stream).await;

    assert_eq!(decoded.calls.len(), 1, "{:?}", decoded.calls);
    assert_eq!(decoded.calls[0].name, "get_weather");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&decoded.calls[0].arguments).expect("JSON"),
        serde_json::json!({"city": "Paris"})
    );
    assert_eq!(decoded.stop, Some(StopReason::ToolUse));
    assert_eq!(decoded.usage.input_tokens, 322);
    assert_eq!(decoded.usage.output_tokens, 58);
    assert_eq!(decoded.usage.reasoning_tokens, 29);
    assert!(decoded.warnings.is_empty(), "{:?}", decoded.warnings);
    server.assert_no_misses();
}

#[tokio::test]
async fn the_live_neuralwatt_responses_turn_decodes() {
    let request = live_request("Reply with exactly one word: pong", 64);
    let cassette = load_live(
        LIVE_RESPONSES_CASSETTE,
        ProtocolId::OpenAiResponses,
        &request,
        "/v1/responses",
    );
    let server = cassette.start().await;

    let stream = client()
        .stream(
            &route(
                ProtocolId::OpenAiResponses,
                "nw-small",
                format!("{}/v1/responses", server.url()),
            ),
            &request,
        )
        .await
        .expect("the cassette answers");
    let decoded = decode(stream).await;

    assert_eq!(decoded.text, "pong");
    assert_eq!(decoded.stop, Some(StopReason::EndTurn));
    assert_eq!(decoded.usage.input_tokens, 64);
    assert_eq!(decoded.usage.output_tokens, 19);
    assert_eq!(decoded.usage.reasoning_tokens, 14);
    assert_eq!(
        decoded.warnings.len(),
        1,
        "the gateway sends reasoning with no blob: {:?}",
        decoded.warnings
    );
    assert!(
        decoded.warnings[0].contains("encrypted_content"),
        "{:?}",
        decoded.warnings
    );
    server.assert_no_misses();
}

#[tokio::test]
async fn a_gateway_without_encrypted_reasoning_still_decodes() {
    let mut request = live_request("Reply with exactly one word: pong", 512);
    request.reasoning = Some(ReasoningEffort::Low);
    let cassette = load_live(
        LIVE_REASONING_CASSETTE,
        ProtocolId::OpenAiResponses,
        &request,
        "/v1/responses",
    );
    let server = cassette.start().await;

    let stream = client()
        .stream(
            &route(
                ProtocolId::OpenAiResponses,
                "nw-small",
                format!("{}/v1/responses", server.url()),
            ),
            &request,
        )
        .await
        .expect("the cassette answers");
    let decoded = decode(stream).await;

    assert_eq!(decoded.text, "pong");
    assert_eq!(decoded.usage.reasoning_tokens, 14);
    assert!(
        decoded.signatures.is_empty(),
        "NeuralWatt answers `include: [reasoning.encrypted_content]` with a summary \
         and an item id but no blob, so there is nothing to replay: {:?}",
        decoded.signatures
    );
    assert_eq!(
        decoded.warnings.len(),
        1,
        "the missing blob is reported once: {:?}",
        decoded.warnings
    );
    assert!(
        decoded.warnings[0].contains("cannot be replayed statelessly"),
        "{:?}",
        decoded.warnings
    );
    server.assert_no_misses();
}

#[tokio::test]
async fn the_live_neuralwatt_messages_turn_decodes() {
    let request = live_request("Reply with exactly one word: pong", 64);
    let cassette = load_live(
        LIVE_MESSAGES_CASSETTE,
        ProtocolId::AnthropicMessages,
        &request,
        "/v1/messages",
    );
    let server = cassette.start().await;

    let stream = client()
        .stream(
            &route(
                ProtocolId::AnthropicMessages,
                "nw-small",
                format!("{}/v1/messages", server.url()),
            ),
            &request,
        )
        .await
        .expect("the cassette answers");
    let decoded = decode(stream).await;

    assert_eq!(decoded.text, "pong");
    assert_eq!(decoded.stop, Some(StopReason::EndTurn));
    assert_eq!(decoded.usage.input_tokens, 64);
    assert_eq!(decoded.usage.output_tokens, 14);
    assert_eq!(
        decoded.signatures.len(),
        1,
        "the thinking block keeps the signature the gateway sent"
    );
    assert!(decoded.warnings.is_empty(), "{:?}", decoded.warnings);
    server.assert_no_misses();
}

#[tokio::test]
async fn a_changed_request_does_not_replay() {
    let cassette = Cassette::load(fixtures().join(RESPONSES_CASSETTE)).expect("cassette");
    let server = cassette.start().await;

    let mut request = responses_request();
    request.messages = vec![Message::user("Goodbye!")];
    let result = client()
        .stream(
            &route(
                ProtocolId::OpenAiResponses,
                "gpt-5.5",
                format!("{}/v1/responses", server.url()),
            ),
            &request,
        )
        .await;

    let error = result.expect_err("an unrecorded request fails");
    assert!(error.to_string().contains("cassette miss"), "{error}");
    let misses = server.misses();
    assert_eq!(misses.len(), 1);
    let closest = misses[0]
        .closest
        .as_deref()
        .expect("the closest interaction");
    assert!(closest.contains("input"), "{closest}");
}

#[tokio::test]
async fn a_consumed_exchange_is_not_served_again() {
    let cassette = Cassette::load(fixtures().join(RESPONSES_CASSETTE)).expect("cassette");
    let server = cassette.start().await;
    let route = route(
        ProtocolId::OpenAiResponses,
        "gpt-5.5",
        format!("{}/v1/responses", server.url()),
    );

    let stream = client()
        .stream(&route, &responses_request())
        .await
        .expect("the cassette answers once");
    let decoded = decode(stream).await;
    assert_eq!(decoded.text, "Hello!");

    // The cursor rule makes the second request an immediate, non-retryable
    // miss instead of a repeat of the recorded turn.
    let error = client()
        .stream(&route, &responses_request())
        .await
        .expect_err("a consumed exchange is a miss");
    assert!(error.to_string().contains("cassette miss"), "{error}");
    assert!(!error.is_retryable(), "{error:?}");
    assert_eq!(server.hits(), vec![0]);
    assert_eq!(server.remaining(), 0);
}

#[tokio::test]
async fn the_same_exchange_replays_twice_when_recorded_twice() {
    let cassette = Cassette::load(fixtures().join(RESPONSES_CASSETTE)).expect("cassette");
    let interaction = cassette.interactions[0].clone();
    let cassette = Cassette::builder("openai-responses/repeated")
        .provider(cassette.provider.clone())
        .protocol(cassette.protocol.clone())
        .model(cassette.model.clone())
        .source("designed by this test from a recorded exchange")
        .interaction(interaction.clone())
        .interaction(interaction)
        .build();
    let server = cassette.start().await;
    let route = route(
        ProtocolId::OpenAiResponses,
        "gpt-5.5",
        format!("{}/v1/responses", server.url()),
    );

    for _ in 0..2 {
        let stream = client()
            .stream(&route, &responses_request())
            .await
            .expect("each listed exchange answers once");
        let decoded = decode(stream).await;
        assert_eq!(decoded.text, "Hello!");
    }

    assert_eq!(server.hits(), vec![0, 1]);
    server.assert_no_misses();
    server.assert_all_consumed();
}

#[derive(Debug, serde::Deserialize)]
struct Manifest {
    scenario: Vec<Scenario>,
}

#[derive(Debug, serde::Deserialize)]
struct Scenario {
    name: String,
    file: String,
    protocol: String,
    provider: String,
    covers: String,
}

/// The scenario manifest, parsed. Missing or invalid is a test failure.
fn manifest() -> Manifest {
    let path = fixtures().join("scenarios.toml");
    toml::from_str(
        &fs::read_to_string(&path).expect("the scenario manifest is in the fixture tree"),
    )
    .expect("the manifest parses")
}

/// The manifest is what makes a missing cassette a failure instead of a test
/// that quietly stopped running. Every cassette on disk is covered, and every
/// covered cassette is replayable.
#[test]
fn every_cassette_is_covered_by_the_manifest() {
    let manifest = manifest();
    assert!(!manifest.scenario.is_empty(), "the manifest is empty");

    let mut listed = BTreeSet::new();
    for scenario in &manifest.scenario {
        assert!(
            listed.insert(scenario.file.clone()),
            "{} is listed twice",
            scenario.file
        );
        assert!(
            !scenario.covers.is_empty(),
            "{} has no description",
            scenario.name
        );

        let path = fixtures().join(&scenario.file);
        let cassette =
            Cassette::load(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(cassette.name, scenario.name);
        assert_eq!(cassette.protocol, scenario.protocol);
        assert_eq!(cassette.provider, scenario.provider);
        assert!(
            !cassette.source.is_empty(),
            "{} has no provenance",
            scenario.name
        );
        assert!(
            !cassette.interactions.is_empty(),
            "{} has no interactions",
            scenario.name
        );

        for interaction in &cassette.interactions {
            assert!(
                serde_json::from_str::<serde_json::Value>(&interaction.request.body).is_ok(),
                "{}: the request body is not JSON",
                scenario.name
            );
            assert!(
                !interaction.response.body.is_empty(),
                "{}: an interaction has an empty response",
                scenario.name
            );
            for (name, value) in &interaction.request.headers {
                if is_sensitive(name) {
                    assert_eq!(
                        value, "REDACTED",
                        "{}: {name} is not redacted",
                        scenario.name
                    );
                }
            }
        }
    }

    for path in json_files(&fixtures()) {
        let relative = path
            .strip_prefix(fixtures())
            .expect("inside the fixture tree")
            .to_string_lossy()
            .replace('\\', "/");
        assert!(
            listed.contains(&relative),
            "{relative} is not in scenarios.toml"
        );
    }
}

fn is_sensitive(name: &str) -> bool {
    matches!(
        name,
        "authorization" | "proxy-authorization" | "x-api-key" | "api-key" | "cookie" | "set-cookie"
    )
}

fn json_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                found.push(path);
            }
        }
    }
    found
}

/// Whichever cassette the manifest lists, replay serves the recorded bytes and
/// the decoder reaches a terminal event. Listing a new cassette is enough to
/// make CI exercise it.
#[tokio::test]
async fn every_manifest_scenario_replays_and_decodes() {
    for scenario in manifest().scenario {
        let cassette = Cassette::load(fixtures().join(&scenario.file))
            .unwrap_or_else(|error| panic!("{}: {error}", scenario.file));
        let server = cassette.start().await;

        for interaction in &cassette.interactions {
            let (status, body) = raw_post(
                server.url(),
                &interaction.request.path,
                &interaction.request.headers,
                &interaction.request.body,
            )
            .await;

            assert_eq!(
                status, interaction.response.status,
                "{}: replayed status",
                scenario.name
            );
            assert_eq!(
                body, interaction.response.body,
                "{}: the player must return the recorded body verbatim",
                scenario.name
            );

            let is_sse =
                interaction.response.headers.iter().any(|(name, value)| {
                    name == "content-type" && value.contains("text/event-stream")
                });
            if is_sse {
                let events = decode_sse(&scenario.protocol, &body);
                assert!(
                    matches!(events.last(), Some(LlmEvent::Done { .. })),
                    "{}: the replayed stream must end with Done, last event was {:?}",
                    scenario.name,
                    events.last()
                );
            }
        }

        assert_eq!(server.hits().len(), cassette.interactions.len());
        server.assert_no_misses();
        server.assert_all_consumed();
    }
}

/// A raw POST straight to the player: the generic replay must not depend on a
/// test's hand-built `Request` matching the recorded one.
async fn raw_post(
    url: &str,
    path: &str,
    headers: &[(String, String)],
    body: &str,
) -> (u16, String) {
    let address = url.trim_start_matches("http://").to_owned();
    let mut socket = TcpStream::connect(&address)
        .await
        .expect("connect to the cassette player");
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    socket
        .write_all(request.as_bytes())
        .await
        .expect("write the recorded request");
    socket.flush().await.expect("flush");

    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), socket.read_to_end(&mut response))
        .await
        .expect("the player answered")
        .expect("read the response");
    let response = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("a complete HTTP response");
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status code");
    (status, body.to_owned())
}

fn decode_sse(protocol: &str, body: &str) -> Vec<LlmEvent> {
    let mut decoder = match protocol {
        "openai-chat" => OpenAiChat::new(Compat::for_protocol(ProtocolId::OpenAiChat)).decoder(),
        "openai-responses" => {
            OpenAiResponses::new(Compat::for_protocol(ProtocolId::OpenAiResponses)).decoder()
        }
        "anthropic-messages" => {
            AnthropicMessages::new(Compat::for_protocol(ProtocolId::AnthropicMessages)).decoder()
        }
        other => panic!("no decoder for protocol {other:?}"),
    };
    let mut sse = SseDecoder::new();
    let mut events = Vec::new();
    for frame in sse.push(body.as_bytes()) {
        events.extend(decoder.frame(&frame.data).expect("the frame decodes"));
    }
    for frame in sse.finish() {
        events.extend(decoder.frame(&frame.data).expect("the frame decodes"));
    }
    events.extend(decoder.finish().expect("the stream finishes"));
    events
}
