//! Records one live provider exchange into a cassette.
//!
//! Ignored by default because it makes a real network call and spends money.
//! The recorder refuses a non-loopback origin unless `KEEL_CASSETTE_RECORD=1`
//! is set, and refuses outright under `CI`.
//!
//! ```text
//! KEEL_CASSETTE_RECORD=1 NEURALWATT_API_KEY=sk-... \
//!   cargo test -p keel-llm-route --test record_live -- --ignored --nocapture
//! ```
//!
//! The key can also come from a file, so it never appears in a shell history:
//! `KEEL_RECORD_KEY_FILE=~/.config/neuralwatt/key`. Everything is overridable:
//! `KEEL_RECORD_SCENARIO` (`neuralwatt/chat-text`), `KEEL_RECORD_UPSTREAM`
//! (`https://api.neuralwatt.com`), `KEEL_RECORD_PROTOCOL` (`openai-chat`),
//! `KEEL_RECORD_MODEL` (`nw-small`), `KEEL_RECORD_API_KEY_ENV`
//! (`NEURALWATT_API_KEY`) and `KEEL_RECORD_PROMPT`.
//!
//! The cassette lands under `tests/fixtures/cassettes/<scenario>.json`. List it
//! in `scenarios.toml` and the manifest test replays it like any other.

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use keel_llm_protocol::Protocol;
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_protocol::openai_responses::OpenAiResponses;
use keel_llm_route::{Auth, Client, CredentialRef, EventStream, Framing, Route, SecretResolver};
use keel_llm_types::{Compat, LlmEvent, Message, ProtocolId, Request};
use keel_testkit::Recorder;

/// Resolves the recording key from a file when one is given, then from the
/// environment. Keeps the secret out of the command line.
struct RecordingKey {
    file: Option<PathBuf>,
}

impl SecretResolver for RecordingKey {
    fn resolve(&self, reference: &CredentialRef) -> Option<String> {
        if let Some(path) = &self.file {
            let value = std::fs::read_to_string(path).ok()?;
            let value = value.trim().to_owned();
            if !value.is_empty() {
                return Some(value);
            }
        }
        reference
            .env
            .iter()
            .find_map(|name| std::env::var(name).ok())
            .filter(|value| !value.trim().is_empty())
    }
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_owned())
}

fn protocol(id: &str) -> (ProtocolId, Arc<dyn Protocol>, &'static str, &'static str) {
    match id {
        "openai-chat" => (
            ProtocolId::OpenAiChat,
            Arc::new(OpenAiChat::new(Compat::for_protocol(
                ProtocolId::OpenAiChat,
            ))),
            "/v1/chat/completions",
            "openai-chat",
        ),
        "openai-responses" => (
            ProtocolId::OpenAiResponses,
            Arc::new(OpenAiResponses::new(Compat::for_protocol(
                ProtocolId::OpenAiResponses,
            ))),
            "/v1/responses",
            "openai-responses",
        ),
        other => panic!("no recording path for protocol {other:?}"),
    }
}

#[tokio::test]
#[ignore = "makes a live provider call; needs KEEL_CASSETTE_RECORD=1 and a key"]
async fn record_one_live_turn() {
    assert_eq!(
        std::env::var("KEEL_CASSETTE_RECORD").as_deref(),
        Ok("1"),
        "recording a real endpoint needs KEEL_CASSETTE_RECORD=1"
    );

    let scenario = env_or("KEEL_RECORD_SCENARIO", "neuralwatt/chat-text");
    let upstream = env_or("KEEL_RECORD_UPSTREAM", "https://api.neuralwatt.com");
    let protocol_name = env_or("KEEL_RECORD_PROTOCOL", "openai-chat");
    let model = env_or("KEEL_RECORD_MODEL", "nw-small");
    let key_env = env_or("KEEL_RECORD_API_KEY_ENV", "NEURALWATT_API_KEY");
    let provider_id = env_or("KEEL_RECORD_PROVIDER", "neuralwatt");
    let prompt = env_or("KEEL_RECORD_PROMPT", "Reply with exactly one word: pong");
    let key_file = std::env::var_os("KEEL_RECORD_KEY_FILE").map(PathBuf::from);

    let (protocol_id, protocol, path, protocol_label) = protocol(&protocol_name);
    let recorder = Recorder::start(&scenario, &upstream)
        .await
        .expect("the recorder allows this origin");
    recorder.provider(provider_id.clone());
    recorder.protocol(protocol_label);
    recorder.model(&model);
    recorder.source("live capture through keel's recorder");

    let client = Client::with_resolver(Arc::new(RecordingKey { file: key_file }))
        .expect("the client builds");
    let route = Route {
        provider: provider_id.into(),
        model: model.clone().into(),
        endpoint: format!("{}{path}", recorder.url()),
        protocol,
        auth: Auth::bearer(CredentialRef::from_env(key_env)),
        framing: Framing::Sse,
        headers: Vec::new(),
    };

    let mut request = Request::new(model.clone());
    request.system = Some("You are terse.".to_owned());
    request.max_output_tokens = Some(64);
    request.messages = vec![Message::user(prompt.as_str())];

    let stream = client
        .stream(&route, &request)
        .await
        .expect("the provider answers the request");
    let (text, done) = finish(stream).await;
    assert!(done, "the recorded stream must reach a terminal event");

    assert_eq!(
        recorder.interaction_count(),
        1,
        "one request, one recorded interaction"
    );
    let cassette = recorder.cassette();
    let response = &cassette.interactions[0].response;
    assert_eq!(response.status, 200, "a successful turn is recorded");
    assert!(
        response.body.contains("data:"),
        "the response is a stream, not a buffered JSON body"
    );

    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cassettes");
    let file = directory.join(format!("{scenario}.json"));
    cassette.save(&file).expect("the cassette is written");

    eprintln!("\nrecorded {} ({protocol_id:?})", file.display());
    eprintln!(
        "  request: {} bytes, response: {} bytes, redactions: {:?}",
        cassette.interactions[0].request.body.len(),
        response.body.len(),
        cassette.redactions
    );
    eprintln!("  model said: {text}");
    eprintln!(
        "\nadd to scenarios.toml:\n\n[[scenario]]\nname = \"{scenario}\"\nfile = \"{scenario}.json\"\nprotocol = \"{protocol_label}\"\nprovider = \"{}\"\ncovers = \"\"\n",
        cassette.provider
    );
}

async fn finish(mut stream: EventStream) -> (String, bool) {
    let mut text = String::new();
    let mut done = false;
    while let Some(event) = stream.next().await {
        let event = event.expect("the recorded stream decodes");
        match event {
            LlmEvent::TextDelta { delta, .. } => text.push_str(&delta),
            LlmEvent::Done { .. } => done = true,
            LlmEvent::Error(error) => panic!("the provider returned an error: {error}"),
            _ => {}
        }
    }
    (text, done)
}
