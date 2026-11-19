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
//! (`https://api.neuralwatt.com`), `KEEL_RECORD_PROTOCOL` (`openai-chat`,
//! `openai-responses` or `anthropic-messages`), `KEEL_RECORD_MODEL`
//! (`nw-small`), `KEEL_RECORD_API_KEY_ENV` (`NEURALWATT_API_KEY`),
//! `KEEL_RECORD_PROMPT`, `KEEL_RECORD_REASONING` (`minimal`, `low`, `medium`
//! or `high`) and `KEEL_RECORD_TOOL` to send one tool definition and require a
//! call back.
//!
//! The cassette lands under `tests/fixtures/cassettes/<scenario>.json`. List it
//! in `scenarios.toml` and the manifest test replays it like any other.

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use keel_llm_protocol::Protocol;
use keel_llm_protocol::anthropic_messages::AnthropicMessages;
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_protocol::openai_responses::OpenAiResponses;
use keel_llm_route::{Auth, Client, CredentialRef, EventStream, Framing, Route, SecretResolver};
use keel_llm_types::{
    Compat, LlmEvent, Message, ProtocolId, ReasoningEffort, Request, ToolCall, ToolSpec,
};
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
        "anthropic-messages" => (
            ProtocolId::AnthropicMessages,
            Arc::new(AnthropicMessages::new(Compat::for_protocol(
                ProtocolId::AnthropicMessages,
            ))),
            "/v1/messages",
            "anthropic-messages",
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
    let tool = env_or("KEEL_RECORD_TOOL", "");
    let reasoning = match env_or("KEEL_RECORD_REASONING", "").as_str() {
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "" => None,
        other => panic!("unknown reasoning effort {other:?}"),
    };

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
        context_window: 0,
        max_output_tokens: 0,
    };

    let mut request = Request::new(model.clone());
    request.system = Some("You are terse.".to_owned());
    let budget = if tool.is_empty() && reasoning.is_none() {
        64
    } else {
        512
    };
    request.max_output_tokens = Some(budget);
    request.reasoning = reasoning;
    request.messages = vec![Message::user(prompt.as_str())];
    if !tool.is_empty() {
        request.tools = vec![ToolSpec {
            name: tool.clone(),
            description: "Report the weather for one city.".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        }];
    }

    let stream = client
        .stream(&route, &request)
        .await
        .expect("the provider answers the request");
    let (text, done, calls, warnings) = finish(stream).await;
    assert!(done, "the recorded stream must reach a terminal event");
    if !tool.is_empty() {
        assert!(
            !calls.is_empty(),
            "the tool turn must return a call, not prose: {text:?}"
        );
    }

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
    for call in &calls {
        eprintln!("  called {}({})", call.name, call.arguments);
    }
    for warning in &warnings {
        eprintln!("  warning: {warning}");
    }
    eprintln!(
        "\nadd to scenarios.toml:\n\n[[scenario]]\nname = \"{scenario}\"\nfile = \"{scenario}.json\"\nprotocol = \"{protocol_label}\"\nprovider = \"{}\"\ncovers = \"\"\n",
        cassette.provider
    );
}

async fn finish(mut stream: EventStream) -> (String, bool, Vec<ToolCall>, Vec<String>) {
    let mut text = String::new();
    let mut done = false;
    let mut calls = Vec::new();
    let mut warnings = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event.expect("the recorded stream decodes");
        match event {
            LlmEvent::TextDelta { delta, .. } => text.push_str(&delta),
            LlmEvent::ToolCall { call, .. } => calls.push(call),
            LlmEvent::Warning(warning) => warnings.push(warning),
            LlmEvent::Done { .. } => done = true,
            LlmEvent::Error(error) => panic!("the provider returned an error: {error}"),
            _ => {}
        }
    }
    (text, done, calls, warnings)
}
