use keel_llm_protocol::{Protocol, StreamDecoder, anthropic_messages::AnthropicMessages};
use keel_llm_types::{
    ANTHROPIC_MAX_BREAKPOINTS, CacheBreakpoint, CacheDropReason, CacheObject, CachePolicy,
    CacheTtl, CacheWarningKind, Compat, LlmEvent, Message, Part, ProtocolId, Request, Source,
    StopReason, ToolChoice, ToolSpec,
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

fn object_policy(breakpoints: Vec<CacheBreakpoint>) -> CachePolicy {
    CachePolicy::Object(CacheObject {
        breakpoints,
        ttl: CacheTtl::FiveMinutes,
    })
}

fn cache_control_at<'a>(
    value: &'a serde_json::Value,
    pointer: &str,
) -> Option<&'a serde_json::Value> {
    value
        .pointer(pointer)
        .and_then(|value| value.get("cache_control"))
}

fn cache_control_type_at<'a>(value: &'a serde_json::Value, pointer: &str) -> &'a str {
    cache_control_at(value, pointer)
        .and_then(|value| value.get("type"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("no cache_control at {pointer}"))
}

#[test]
fn object_policy_places_breakpoints_on_system_tools_and_exact_parts() {
    let assistant = Message::assistant(vec![
        Part::reasoning("thought", Some("sig".to_owned()), source()),
        Part::text("answer"),
        Part::tool_call("call_1".into(), "read_file", "{}", source()),
    ]);
    let request = Request::new("claude-sonnet-4-5")
        .system("be terse")
        .messages(vec![
            Message::user("first"),
            assistant,
            Message::with_id("msg_3".into(), keel_llm_types::Role::User)
                .with_part(Part::tool_result("call_1".into(), "ok", false))
                .with_part(Part::text("summarize")),
        ])
        .tools(vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "read a file".to_owned(),
            input_schema: json!({"type": "object"}),
        }])
        .cache(object_policy(vec![
            CacheBreakpoint::System,
            CacheBreakpoint::Tool(0),
            CacheBreakpoint::Part {
                message: 2,
                part: 0,
            },
            CacheBreakpoint::Part {
                message: 2,
                part: 1,
            },
        ]));

    let encoded = protocol().encode(&request).unwrap();
    let body = &encoded.body;

    assert_eq!(cache_control_type_at(body, "/system/0"), "ephemeral");
    assert_eq!(cache_control_type_at(body, "/tools/0"), "ephemeral");
    assert!(cache_control_at(body, "/messages/1/content/0").is_none());
    assert!(cache_control_at(body, "/messages/1/content/1").is_none());
    assert!(cache_control_at(body, "/messages/1/content/2").is_none());
    // The tool result is the first block of the last user turn and the text
    // is the second, so both exact part addresses land where asked.
    assert_eq!(
        cache_control_type_at(body, "/messages/2/content/0"),
        "ephemeral"
    );
    assert_eq!(
        cache_control_type_at(body, "/messages/2/content/1"),
        "ephemeral"
    );
    assert!(
        encoded.cache_warnings.is_empty(),
        "{:?}",
        encoded.cache_warnings
    );
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[test]
fn object_policy_one_hour_ttl_requires_the_extended_cache_beta_header() {
    let request = Request::new("claude-sonnet-4-5")
        .system("be terse")
        .messages(vec![Message::user("hello")])
        .cache(CachePolicy::Object(
            CacheObject::new()
                .breakpoint(CacheBreakpoint::System)
                .ttl(CacheTtl::OneHour),
        ));

    let encoded = protocol().encode(&request).unwrap();
    assert_eq!(
        encoded.body.pointer("/system/0/cache_control"),
        Some(&json!({"type": "ephemeral", "ttl": "1h"}))
    );
    assert_eq!(
        header(&encoded.headers, "anthropic-beta"),
        Some("extended-cache-ttl-2025-04-11"),
        "the 2023-06-01 API version keel sends needs the beta header for 1h"
    );
    assert_eq!(
        header(&encoded.headers, "anthropic-version"),
        Some("2023-06-01")
    );
    assert!(encoded.cache_warnings.is_empty());
}

#[test]
fn object_policy_one_hour_ttl_without_a_kept_breakpoint_omits_the_beta_header() {
    let request = Request::new("claude-sonnet-4-5")
        .messages(vec![Message::user("hello")])
        .cache(CachePolicy::Object(
            CacheObject::new()
                .breakpoint(CacheBreakpoint::System)
                .ttl(CacheTtl::OneHour),
        ));

    let encoded = protocol().encode(&request).unwrap();
    assert!(encoded.body.get("system").is_none());
    assert_eq!(header(&encoded.headers, "anthropic-beta"), None);
    assert_eq!(encoded.cache_warnings.len(), 1);
}

#[test]
fn object_policy_default_ttl_omits_the_ttl_field() {
    let request = Request::new("claude-sonnet-4-5")
        .messages(vec![Message::user("hello")])
        .cache(object_policy(vec![CacheBreakpoint::LastMessage]));

    let encoded = protocol().encode(&request).unwrap();
    assert_eq!(
        encoded.body.pointer("/messages/0/content/0/cache_control"),
        Some(&json!({"type": "ephemeral"}))
    );
    assert_eq!(header(&encoded.headers, "anthropic-beta"), None);
}

#[test]
fn object_policy_drops_a_missing_position_and_keeps_the_rest() {
    let request = Request::new("claude-sonnet-4-5")
        .system("be terse")
        .messages(vec![Message::user("hello")])
        .cache(object_policy(vec![
            CacheBreakpoint::System,
            CacheBreakpoint::Message(9),
            CacheBreakpoint::Tool(3),
            CacheBreakpoint::Part {
                message: 0,
                part: 7,
            },
        ]));

    let encoded = protocol().encode(&request).unwrap();
    assert_eq!(
        cache_control_type_at(&encoded.body, "/system/0"),
        "ephemeral"
    );
    assert!(cache_control_at(&encoded.body, "/messages/0/content/0").is_none());
    let dropped = encoded
        .cache_warnings
        .iter()
        .filter(|warning| {
            matches!(
                warning.kind,
                CacheWarningKind::Dropped {
                    reason: CacheDropReason::NoSuchPosition,
                    ..
                }
            )
        })
        .count();
    assert_eq!(dropped, 3, "{:?}", encoded.cache_warnings);
}

#[test]
fn object_policy_drops_a_duplicate_breakpoint() {
    let request = Request::new("claude-sonnet-4-5")
        .messages(vec![Message::user("hello")])
        .cache(object_policy(vec![
            CacheBreakpoint::LastMessage,
            CacheBreakpoint::Message(0),
        ]));

    let encoded = protocol().encode(&request).unwrap();
    assert_eq!(
        cache_control_type_at(&encoded.body, "/messages/0/content/0"),
        "ephemeral"
    );
    assert!(encoded.cache_warnings.iter().any(|warning| matches!(
        warning.kind,
        CacheWarningKind::Dropped {
            position: CacheBreakpoint::Message(0),
            reason: CacheDropReason::Duplicate,
        }
    )));
}

#[test]
fn object_policy_caps_at_the_protocol_limit_and_names_what_it_dropped() {
    let messages = (0..6)
        .map(|index| Message::with_id(format!("msg_{index}").into(), keel_llm_types::Role::User))
        .map(|message| message.with_part(Part::text("hello")))
        .collect::<Vec<_>>();
    let breakpoints = (0..6).map(CacheBreakpoint::Message).collect::<Vec<_>>();
    let request = Request::new("claude-sonnet-4-5")
        .messages(messages)
        .cache(object_policy(breakpoints));

    let encoded = protocol().encode(&request).unwrap();
    let controls = encoded
        .body
        .pointer("/messages")
        .and_then(serde_json::Value::as_array)
        .unwrap()
        .iter()
        .filter(|message| message.pointer("/content/0/cache_control").is_some())
        .count();
    assert_eq!(controls, ANTHROPIC_MAX_BREAKPOINTS);
    let cap = encoded
        .cache_warnings
        .iter()
        .find_map(|warning| match &warning.kind {
            CacheWarningKind::CapReached { cap, dropped } => Some((*cap, dropped.clone())),
            _ => None,
        })
        .expect("cap warning");
    assert_eq!(cap.0, ANTHROPIC_MAX_BREAKPOINTS);
    assert_eq!(
        cap.1,
        vec![CacheBreakpoint::Message(4), CacheBreakpoint::Message(5)]
    );
}

#[test]
fn object_policy_keeps_the_earliest_wire_positions_over_the_cap() {
    let messages = (0..6)
        .map(|index| Message::with_id(format!("msg_{index}").into(), keel_llm_types::Role::User))
        .map(|message| message.with_part(Part::text("hello")))
        .collect::<Vec<_>>();
    let request = Request::new("claude-sonnet-4-5")
        .messages(messages)
        .cache(object_policy(vec![
            CacheBreakpoint::Message(5),
            CacheBreakpoint::Message(0),
            CacheBreakpoint::Message(1),
            CacheBreakpoint::Message(2),
            CacheBreakpoint::Message(3),
        ]));

    let encoded = protocol().encode(&request).unwrap();
    for kept in 0..4 {
        assert_eq!(
            cache_control_type_at(&encoded.body, &format!("/messages/{kept}/content/0")),
            "ephemeral",
            "message {kept} is inside the earliest prefix"
        );
    }
    assert!(
        cache_control_at(&encoded.body, "/messages/5/content/0").is_none(),
        "listing message 5 first must not displace the earlier prefix"
    );
    let cap = encoded
        .cache_warnings
        .iter()
        .find_map(|warning| match &warning.kind {
            CacheWarningKind::CapReached { dropped, .. } => Some(dropped.clone()),
            _ => None,
        })
        .expect("cap warning");
    assert_eq!(cap, vec![CacheBreakpoint::Message(5)]);
}

#[test]
fn object_policy_handles_extreme_indices_without_panicking() {
    let request = Request::new("claude-sonnet-4-5")
        .messages(vec![Message::user("hello")])
        .cache(object_policy(vec![
            CacheBreakpoint::Message(usize::MAX),
            CacheBreakpoint::Tool(usize::MAX),
            CacheBreakpoint::Part {
                message: usize::MAX,
                part: usize::MAX,
            },
        ]));

    let encoded = protocol().encode(&request).unwrap();
    assert_eq!(encoded.cache_warnings.len(), 3);
    assert!(cache_control_at(&encoded.body, "/messages/0/content/0").is_none());
}

#[test]
fn object_policy_warns_when_a_thinking_block_is_addressed() {
    let assistant = Message::assistant(vec![Part::reasoning(
        "thought",
        Some("sig".to_owned()),
        source(),
    )]);
    let request = Request::new("claude-sonnet-4-5")
        .messages(vec![Message::user("go"), assistant])
        .cache(object_policy(vec![CacheBreakpoint::Message(1)]));

    let encoded = protocol().encode(&request).unwrap();
    assert!(encoded.cache_warnings.iter().any(|warning| matches!(
        warning.kind,
        CacheWarningKind::Dropped {
            position: CacheBreakpoint::Message(1),
            reason: CacheDropReason::NotCacheable,
        }
    )));
}

#[test]
fn object_policy_marks_an_extra_part_of_a_flattened_system_message_not_cacheable() {
    let system = Message::new(keel_llm_types::Role::System)
        .with_part(Part::text("first"))
        .with_part(Part::text("second"));
    let request = Request::new("claude-sonnet-4-5")
        .messages(vec![system, Message::user("hello")])
        .cache(object_policy(vec![
            CacheBreakpoint::Part {
                message: 0,
                part: 0,
            },
            CacheBreakpoint::Part {
                message: 0,
                part: 1,
            },
        ]));

    let encoded = protocol().encode(&request).unwrap();
    assert_eq!(
        cache_control_type_at(&encoded.body, "/system/0"),
        "ephemeral"
    );
    assert!(encoded.cache_warnings.iter().any(|warning| matches!(
        warning.kind,
        CacheWarningKind::Dropped {
            position: CacheBreakpoint::Part {
                message: 0,
                part: 1,
            },
            reason: CacheDropReason::NotCacheable,
        }
    )));
}

#[test]
fn none_strips_every_cache_control_from_an_anthropic_request() {
    let request = Request::new("claude-sonnet-4-5")
        .system("be terse")
        .messages(vec![Message::user("hello")])
        .tools(vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "read a file".to_owned(),
            input_schema: json!({"type": "object"}),
        }])
        .cache(CachePolicy::None);

    let encoded = protocol().encode(&request).unwrap();
    let text = serde_json::to_string(&encoded.body).unwrap();
    assert!(!text.contains("cache_control"), "{text}");
    assert!(encoded.cache_warnings.is_empty());
}
