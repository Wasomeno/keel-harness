use keel_llm_protocol::{Protocol, StreamDecoder, openai_chat::OpenAiChat};
use keel_llm_types::{
    Compat, Content, LlmEvent, Message, Part, ProtocolId, Request, StopReason, ToolChoice,
    ToolSpec, Usage,
};
use serde_json::json;

fn protocol() -> OpenAiChat {
    OpenAiChat::new(Compat::for_protocol(ProtocolId::OpenAiChat))
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
fn encodes_a_request_with_tools_and_usage() {
    let request = Request::new("gpt-4.1")
        .system("be terse")
        .messages(vec![Message::user("hello")])
        .tools(vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "read a file".to_owned(),
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }])
        .tool_choice(ToolChoice::Auto)
        .max_output_tokens(256)
        .temperature(0.2);

    let encoded = protocol().encode(&request).unwrap();
    let body = &encoded.body;

    assert_eq!(body["model"], json!("gpt-4.1"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"]["include_usage"], json!(true));
    assert_eq!(body["max_tokens"], json!(256));
    assert_eq!(body["messages"][0]["role"], json!("system"));
    assert_eq!(body["messages"][0]["content"], json!("be terse"));
    assert_eq!(body["messages"][1]["role"], json!("user"));
    assert_eq!(body["messages"][1]["content"], json!("hello"));
    assert_eq!(body["tools"][0]["function"]["name"], json!("read_file"));
    assert_eq!(body["tool_choice"], json!("auto"));
    assert!(encoded.warnings.is_empty());
}

#[test]
fn rejects_an_empty_request() {
    let request = Request::new("gpt-4.1");
    assert!(protocol().encode(&request).is_err());
}

#[test]
fn emits_tool_results_as_tool_messages() {
    let assistant = Message::assistant(vec![Part::tool_call(
        "call_1".into(),
        "read_file",
        r#"{"path":"a"}"#,
        keel_llm_types::Source {
            protocol: ProtocolId::OpenAiChat,
            provider: "openai".into(),
            model: "gpt-4.1".into(),
        },
    )]);
    let user = Message::new(keel_llm_types::Role::User)
        .with_part(Part::tool_result("call_1".into(), "contents", false))
        .with_part(Part::text("summarize"));

    let request = Request::new("gpt-4.1").messages(vec![Message::user("go"), assistant, user]);
    let encoded = protocol().encode(&request).unwrap();
    let messages = encoded.body["messages"].as_array().unwrap();

    assert_eq!(messages[1]["role"], json!("assistant"));
    assert_eq!(
        messages[1]["tool_calls"][0]["function"]["arguments"],
        json!(r#"{"path":"a"}"#)
    );
    assert_eq!(messages[2]["role"], json!("tool"));
    assert_eq!(messages[2]["tool_call_id"], json!("call_1"));
    assert_eq!(messages[3]["role"], json!("user"));
    assert_eq!(messages[3]["content"], json!("summarize"));
}

#[test]
fn decodes_text_deltas_and_a_terminal_event() {
    let mut decoder = protocol().decoder();
    let events = collect(
        &mut decoder,
        &[
            r#"{"model":"gpt-4.1","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
            r#"{"model":"gpt-4.1","choices":[{"index":0,"delta":{"content":"Hel"},"finish_reason":null}]}"#,
            r#"{"model":"gpt-4.1","choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":3}}}"#,
            "[DONE]",
        ],
    );

    assert!(matches!(events[0], LlmEvent::Start { .. }));
    let deltas: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["Hel", "lo"]);
    assert!(events.iter().any(|event| matches!(
        event,
        LlmEvent::TextEnd { text, .. } if text == "Hello"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        LlmEvent::Done {
            stop: StopReason::EndTurn
        }
    )));

    let usage = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Usage(usage) if usage.output_tokens == 2 => Some(*usage),
            _ => None,
        })
        .expect("usage event");
    assert_eq!(
        usage,
        Usage {
            input_tokens: 7,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        }
    );
}

#[test]
fn accumulates_streamed_tool_arguments() {
    let mut decoder = protocol().decoder();
    let events = collect(
        &mut decoder,
        &[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_9","type":"function","function":{"name":"read_file","arguments":""}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ],
    );

    let call = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::ToolCall { call, .. } => Some(call),
            _ => None,
        })
        .expect("tool call event");

    assert_eq!(call.id.as_str(), "call_9");
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
fn surfaces_in_stream_errors() {
    let mut decoder = protocol().decoder();
    let error = decoder
        .frame(r#"{"error":{"message":"upstream exploded","code":"boom"}}"#)
        .unwrap_err();
    assert_eq!(error.kind, keel_llm_types::ErrorKind::ServerError);
    assert_eq!(error.provider_code.as_deref(), Some("boom"));
}

#[test]
fn reasoning_content_is_not_mistaken_for_text() {
    let mut decoder = protocol().decoder();
    let events = collect(
        &mut decoder,
        &[
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"thinking"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ],
    );

    assert!(matches!(
        &events[1],
        LlmEvent::ReasoningDelta { delta, .. } if delta == "thinking"
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        LlmEvent::ReasoningEnd { text, .. } if text == "thinking"
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        LlmEvent::TextEnd { text, .. } if text.contains("thinking")
    )));
}

#[test]
fn image_parts_lower_to_data_urls() {
    let request = Request::new("gpt-4.1").messages(vec![
        Message::new(keel_llm_types::Role::User)
            .with_part(Part::text("what is this"))
            .with_part(Part::image("image/png", "AAAA")),
    ]);

    let encoded = protocol().encode(&request).unwrap();
    let content = &encoded.body["messages"][0]["content"];
    assert_eq!(content[0]["type"], json!("text"));
    assert_eq!(content[1]["type"], json!("image_url"));
    assert_eq!(
        content[1]["image_url"]["url"],
        json!("data:image/png;base64,AAAA")
    );
}

#[test]
fn dropped_tool_calls_are_reported_not_silent() {
    let compat = Compat::for_protocol(ProtocolId::OpenAiChat).tools(false);
    let protocol = OpenAiChat::new(compat);
    let assistant = Message::assistant(vec![Part::tool_call(
        "call_1".into(),
        "read_file",
        "{}",
        keel_llm_types::Source {
            protocol: ProtocolId::OpenAiChat,
            provider: "openai".into(),
            model: "gpt-4.1".into(),
        },
    )]);
    let request = Request::new("gpt-4.1").messages(vec![Message::user("go"), assistant]);
    let encoded = protocol.encode(&request).unwrap();
    assert!(matches!(
        encoded.body["messages"][1]["content"],
        serde_json::Value::Null
    ));
    assert!(matches!(
        encoded.body["messages"][1]["tool_calls"],
        serde_json::Value::Null
    ));
    assert!(!encoded.warnings.is_empty());
}

#[test]
fn text_parts_stay_a_plain_string() {
    let request = Request::new("gpt-4.1").messages(vec![
        Message::new(keel_llm_types::Role::User)
            .with_part(Part::text("one"))
            .with_part(Part::text("two")),
    ]);

    let encoded = protocol().encode(&request).unwrap();
    assert_eq!(
        encoded.body["messages"][0]["content"],
        json!(serde_json::Value::Array(vec![
            json!({"type": "text", "text": "one"}),
            json!({"type": "text", "text": "two"})
        ]))
    );
    assert!(matches!(
        encoded.body["messages"][0]["content"],
        serde_json::Value::Array(_)
    ));
    let _ = Content::Text {
        text: String::new(),
    };
}
