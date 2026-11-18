//! Golden request bodies.
//!
//! These catch the regression that matters most in a harness: the request we
//! send changed. A diff here is a prompt or a wire change, not a flaky decode.
//!
//! Regenerate with `UPDATE_GOLDEN=1 cargo test -p keel-llm-protocol --test requests`
//! and review the diff before committing it.

use std::fs;
use std::path::PathBuf;

use serde_json::json;

use keel_llm_protocol::Protocol;
use keel_llm_protocol::anthropic_messages::AnthropicMessages;
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_types::{
    CachePolicy, Compat, Message, MessageId, Part, ProtocolId, ProviderId, ReasoningEffort,
    Request, Role, Source, ToolChoice, ToolSpec,
};

fn source(protocol: ProtocolId) -> Source {
    Source {
        protocol,
        provider: ProviderId::from("test"),
        model: "test-model".into(),
    }
}

/// One request that exercises the parts every protocol has to place: a system
/// prompt, text, an image, signed reasoning, a tool call and its result.
fn canonical_request() -> Request {
    let mut request = Request::new("test-model");
    request.system = Some("You are terse.".to_owned());
    request.max_output_tokens = Some(1024);
    request.temperature = Some(0.25);
    request.reasoning = Some(ReasoningEffort::Medium);
    request.cache = CachePolicy::Auto;
    request.tool_choice = ToolChoice::Auto;
    request.tools = vec![ToolSpec {
        name: "read".to_owned(),
        description: "Read a file".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        }),
    }];

    let user = Message::with_id(MessageId::from("msg_1"), Role::User)
        .with_part(Part::text("read hello.txt"))
        .with_part(Part::image("image/png", "iVBORw0KGgo="));

    let assistant = Message::with_id(MessageId::from("msg_2"), Role::Assistant)
        .with_part(Part::reasoning(
            "I should read the file.",
            Some("sig_abc123".to_owned()),
            source(ProtocolId::AnthropicMessages),
        ))
        .with_part(Part::text("Reading it now."))
        .with_part(Part::tool_call(
            "call_1".into(),
            "read",
            r#"{"path":"hello.txt"}"#,
            source(ProtocolId::AnthropicMessages),
        ));

    let tool_turn = Message::with_id(MessageId::from("msg_3"), Role::User)
        .with_part(Part::tool_result("call_1".into(), "hello", false))
        .with_part(Part::text("summarize it"));

    request.messages = vec![user, assistant, tool_turn];
    request
}

fn encode(protocol: ProtocolId) -> (serde_json::Value, Vec<(String, String)>) {
    let request = canonical_request();
    let encoded = match protocol {
        ProtocolId::OpenAiChat => {
            OpenAiChat::new(Compat::for_protocol(protocol).reasoning(true)).encode(&request)
        }
        ProtocolId::AnthropicMessages => {
            AnthropicMessages::new(Compat::for_protocol(protocol)).encode(&request)
        }
        other => panic!("no golden request for {other:?}"),
    }
    .expect("the canonical request encodes");
    (encoded.body, encoded.headers)
}

#[test]
fn openai_chat_matches_the_golden_body() {
    check(ProtocolId::OpenAiChat, "openai_chat");
}

#[test]
fn anthropic_messages_matches_the_golden_body() {
    check(ProtocolId::AnthropicMessages, "anthropic_messages");
}

fn check(protocol: ProtocolId, name: &str) {
    let (body, headers) = encode(protocol);
    let rendered = serde_json::to_string_pretty(&body).expect("the body serializes") + "\n";
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/requests")
        .join(format!("{name}.json"));

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::create_dir_all(path.parent().expect("fixtures directory")).expect("create fixtures");
        fs::write(&path, &rendered).expect("write golden");
    }

    let expected = fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing golden file {}; run with UPDATE_GOLDEN=1",
            path.display()
        )
    });
    assert_eq!(
        rendered,
        expected,
        "the {protocol:?} request drifted from {}",
        path.display()
    );
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "content-type" && value == "application/json"),
        "the request must declare a JSON body"
    );
}
