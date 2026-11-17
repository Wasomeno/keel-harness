use keel_llm_protocol::{Protocol, StreamDecoder, anthropic_messages::AnthropicMessages};
use keel_llm_types::{
    CachePolicy, Compat, LlmEvent, Message, Part, ProtocolId, Request, Source, StopReason,
    ToolChoice, ToolSpec,
};
use serde_json::json;

fn protocol() -> AnthropicMessages {
    AnthropicMessages::new(Compat::for_protocol(ProtocolId::AnthropicMessages))
}

fn source() -> Source {
    Source {
        protocol: ProtocolId::AnthropicMessages,
        provider: "anthropic".into(),
        model: "claude-sonnet-4-5".into(),
    }
}

fn collect(decoder: &mut Box<dyn StreamDecoder>, frames: &[&str]) -> Vec<LlmEvent> {
    let mut events = Vec::new();
    for frame in frames {
        events.extend(decoder.frame(frame).expect("frame decodes"));
    }
    events.extend(decoder.finish().expect("stream finishes"));
    events
}

#[test]
fn encodes_system_messages_tools_and_cache_breakpoints() {
    let request = Request::new("claude-sonnet-4-5")
        .system("be terse")
        .messages(vec![Message::user("hello")])
        .tools(vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "read a file".to_owned(),
            input_schema: json!({"type": "object"}),
        }])
        .tool_choice(ToolChoice::Required)
        .max_output_tokens(1024)
        .cache(CachePolicy::Auto);

    let encoded = protocol().encode(&request).unwrap();
    let body = &encoded.body;

    assert_eq!(body["max_tokens"], json!(1024));
    assert_eq!(body["system"][0]["text"], json!("be terse"));
    assert_eq!(
        body["system"][0]["cache_control"]["type"],
        json!("ephemeral")
    );
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["type"],
        json!("ephemeral")
    );
    assert_eq!(body["tools"][0]["name"], json!("read_file"));
    assert_eq!(body["tool_choice"], json!({"type": "any"}));
}

#[test]
fn assistant_reasoning_is_encoded_with_its_signature() {
    let assistant = Message::assistant(vec![
        Part::reasoning("thought", Some("sig-123".to_owned()), source()),
        Part::text("answer"),
    ]);
    let request = Request::new("claude-sonnet-4-5").messages(vec![Message::user("go"), assistant]);

    let encoded = protocol().encode(&request).unwrap();
    let content = encoded.body["messages"][1]["content"].as_array().unwrap();

    assert_eq!(content[0]["type"], json!("thinking"));
    assert_eq!(content[0]["thinking"], json!("thought"));
    assert_eq!(content[0]["signature"], json!("sig-123"));
    assert_eq!(content[1]["type"], json!("text"));
}

#[test]
fn unsigned_reasoning_is_dropped_with_a_warning() {
    let assistant = Message::assistant(vec![
        Part::reasoning("thought", None, source()),
        Part::text("answer"),
    ]);
    let request = Request::new("claude-sonnet-4-5").messages(vec![Message::user("go"), assistant]);

    let encoded = protocol().encode(&request).unwrap();
    let content = encoded.body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert!(!encoded.warnings.is_empty());
}

#[test]
fn tool_results_come_before_user_text() {
    let user = Message::new(keel_llm_types::Role::User)
        .with_part(Part::text("what did it say"))
        .with_part(Part::tool_result("call_1".into(), "contents", false));
    let request = Request::new("claude-sonnet-4-5").messages(vec![user]);

    let encoded = protocol().encode(&request).unwrap();
    let content = encoded.body["messages"][0]["content"].as_array().unwrap();

    assert_eq!(content[0]["type"], json!("tool_result"));
    assert_eq!(content[0]["tool_use_id"], json!("call_1"));
    assert_eq!(content[1]["type"], json!("text"));
}

#[test]
fn tool_arguments_are_parsed_into_an_object() {
    let assistant = Message::assistant(vec![Part::tool_call(
        "call_1".into(),
        "read_file",
        r#"{"path":"a"}"#,
        source(),
    )]);
    let request = Request::new("claude-sonnet-4-5").messages(vec![Message::user("go"), assistant]);

    let encoded = protocol().encode(&request).unwrap();
    let block = &encoded.body["messages"][1]["content"][0];
    assert_eq!(block["type"], json!("tool_use"));
    assert_eq!(block["input"]["path"], json!("a"));
}

#[test]
fn thinking_raises_max_tokens_to_fit_the_budget() {
    let request = Request::new("claude-sonnet-4-5")
        .messages(vec![Message::user("think hard")])
        .max_output_tokens(100)
        .reasoning(keel_llm_types::ReasoningEffort::Medium);

    let encoded = protocol().encode(&request).unwrap();
    assert_eq!(encoded.body["thinking"]["budget_tokens"], json!(8192));
    assert!(encoded.body["max_tokens"].as_u64().unwrap() > 8192);
}

#[test]
fn decodes_text_and_a_terminal_event() {
    let mut decoder = protocol().decoder();
    let events = collect(
        &mut decoder,
        &[
            r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-5","usage":{"input_tokens":11,"cache_read_input_tokens":4}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            r#"{"type":"message_stop"}"#,
        ],
    );

    assert!(matches!(
        &events[0],
        LlmEvent::Start { model } if model.as_str() == "claude-sonnet-4-5"
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        LlmEvent::TextEnd { text, .. } if text == "Hi"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        LlmEvent::Done {
            stop: StopReason::EndTurn
        }
    )));
    let usage = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::Usage(usage) => Some(*usage),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(usage.input_tokens, 11);
    assert_eq!(usage.cache_read_tokens, 4);
    assert_eq!(usage.output_tokens, 3);
}

#[test]
fn collects_thinking_signatures_across_deltas() {
    let mut decoder = protocol().decoder();
    let events = collect(
        &mut decoder,
        &[
            r#"{"type":"message_start","message":{"model":"claude-sonnet-4-5","usage":{"input_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"step "}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"one"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
            r#"{"type":"message_stop"}"#,
        ],
    );

    let end = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::ReasoningEnd {
                text, signature, ..
            } => Some((text, signature)),
            _ => None,
        })
        .expect("reasoning end");
    assert_eq!(end.0, "step one");
    assert_eq!(end.1.as_deref(), Some("sig"));
}

#[test]
fn accumulates_tool_arguments() {
    let mut decoder = protocol().decoder();
    let events = collect(
        &mut decoder,
        &[
            r#"{"type":"message_start","message":{"model":"claude-sonnet-4-5","usage":{"input_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"th\":\"a\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
            r#"{"type":"message_stop"}"#,
        ],
    );

    let call = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::ToolCall { call, .. } => Some(call),
            _ => None,
        })
        .expect("tool call");
    assert_eq!(call.id.as_str(), "toolu_1");
    assert_eq!(call.name, "read_file");
    assert_eq!(call.arguments, r#"{"path":"a"}"#);
    assert!(events.iter().any(|event| matches!(
        event,
        LlmEvent::Done {
            stop: StopReason::ToolUse
        }
    )));
}

#[test]
fn classifies_overloaded_errors() {
    let mut decoder = protocol().decoder();
    let error = decoder
        .frame(r#"{"type":"error","error":{"type":"overloaded_error","message":"try later"}}"#)
        .unwrap_err();
    assert_eq!(error.kind, keel_llm_types::ErrorKind::Overloaded);
    assert!(error.is_retryable());
}
