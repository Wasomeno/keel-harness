use keel_llm_types::{
    Content, Message, Part, ProtocolId, RepairNote, ReplayTarget, Role, Source, StopReason,
    ToolCallId, repair_transcript,
};

fn target(protocol: ProtocolId) -> ReplayTarget {
    ReplayTarget::new(protocol, "provider", "model")
}

fn source(protocol: ProtocolId, provider: &str, model: &str) -> Source {
    Source {
        protocol,
        provider: provider.into(),
        model: model.into(),
    }
}

fn tool_call(id: &str) -> Part {
    Part::tool_call(
        ToolCallId::new(id),
        "read_file",
        r#"{"path":"README.md"}"#,
        source(ProtocolId::AnthropicMessages, "anthropic", "claude"),
    )
}

#[test]
fn downgrades_an_image_to_a_named_placeholder_and_records_the_note() {
    let message = Message::with_id("msg_image".into(), Role::User)
        .with_part(Part::text("image from view_image: render.png"))
        .with_part(Part::image_with_detail(
            "image/png",
            "iVBORw0KGgo=",
            Some("high".to_owned()),
        ));
    let target = target(ProtocolId::OpenAiChat).with_images(false);

    let outcome = repair_transcript(vec![message], &target);

    assert_eq!(outcome.messages.len(), 1);
    assert_eq!(outcome.messages[0].parts.len(), 2);
    assert_eq!(
        outcome.messages[0].parts[0].as_text(),
        Some("image from view_image: render.png")
    );
    assert_eq!(
        outcome.messages[0].parts[1].as_text(),
        Some("[image omitted: image/png]")
    );
    assert!(outcome.notes.iter().any(|note| matches!(
        note,
        RepairNote::DowngradedImage { message, mime }
            if message.as_str() == "msg_image" && mime == "image/png"
    )));
}

#[test]
fn keeps_the_image_for_a_target_with_vision() {
    let message = Message::new(Role::User).with_part(Part::image("image/webp", "AAAA"));

    let outcome = repair_transcript(vec![message], &target(ProtocolId::OpenAiChat));

    assert!(outcome.is_clean());
    assert!(matches!(
        outcome.messages[0].parts[0].content,
        Content::Image { .. }
    ));
}

#[test]
fn drops_foreign_unsigned_reasoning() {
    let reasoning = Part::reasoning(
        "private thoughts",
        None,
        source(ProtocolId::OpenAiChat, "openai", "gpt-4.1"),
    );
    let messages = vec![Message::assistant(vec![
        reasoning,
        Part::text("answer").with_source(source(ProtocolId::OpenAiChat, "openai", "gpt-4.1")),
    ])];

    let outcome = repair_transcript(messages, &target(ProtocolId::AnthropicMessages));

    assert_eq!(outcome.messages.len(), 1);
    assert_eq!(outcome.messages[0].parts.len(), 1);
    assert!(matches!(
        outcome.messages[0].parts[0].content,
        Content::Text { .. }
    ));
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| matches!(note, RepairNote::DroppedReasoning { .. }))
    );
}

#[test]
fn keeps_signed_reasoning_from_another_model_of_the_same_protocol() {
    let reasoning = Part::reasoning(
        "private thoughts",
        Some("signature".to_owned()),
        source(ProtocolId::AnthropicMessages, "anthropic", "claude-opus"),
    );
    let messages = vec![Message::assistant(vec![reasoning, Part::text("answer")])];

    let outcome = repair_transcript(messages, &target(ProtocolId::AnthropicMessages));

    assert_eq!(outcome.messages[0].parts.len(), 2);
}

#[test]
fn keeps_reasoning_from_the_exact_same_model() {
    let reasoning = Part::reasoning(
        "private thoughts",
        None,
        source(ProtocolId::AnthropicMessages, "provider", "model"),
    );
    let messages = vec![Message::assistant(vec![reasoning, Part::text("answer")])];

    let outcome = repair_transcript(messages, &target(ProtocolId::AnthropicMessages));

    assert_eq!(outcome.messages[0].parts.len(), 2);
    assert!(outcome.is_clean());
}

#[test]
fn drops_errored_and_aborted_turns() {
    let mut errored = Message::assistant(vec![Part::text("partial")]);
    errored.error = Some("connection reset".to_owned());
    errored.stop = Some(StopReason::Error);

    let mut aborted = Message::assistant(vec![Part::text("partial")]);
    aborted.stop = Some(StopReason::Aborted);

    let messages = vec![Message::user("go"), errored, aborted];

    let outcome = repair_transcript(messages, &target(ProtocolId::OpenAiChat));

    assert_eq!(outcome.messages.len(), 1);
    assert_eq!(outcome.messages[0].role, keel_llm_types::Role::User);
}

#[test]
fn normalizes_tool_ids_on_calls_and_results() {
    let messages = vec![
        Message::user("go"),
        Message::assistant(vec![tool_call("call/1 with space")]),
        Message::new(keel_llm_types::Role::User).with_part(Part::tool_result(
            ToolCallId::new("call/1 with space"),
            "contents",
            false,
        )),
    ];

    let outcome = repair_transcript(messages, &target(ProtocolId::AnthropicMessages));

    let call_id = match &outcome.messages[1].parts[0].content {
        Content::ToolCall { id, .. } => id.clone(),
        other => panic!("expected a tool call, got {other:?}"),
    };
    assert_eq!(call_id.as_str(), "call_1_with_space");

    let result_id = match &outcome.messages[2].parts[0].content {
        Content::ToolResult { call_id, .. } => call_id.clone(),
        other => panic!("expected a tool result, got {other:?}"),
    };
    assert_eq!(result_id, call_id);
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| matches!(note, RepairNote::NormalizedToolId { .. }))
    );
}

#[test]
fn synthesizes_a_missing_tool_result_before_user_text() {
    let messages = vec![
        Message::user("go"),
        Message::assistant(vec![tool_call("call_1")]),
        Message::user("never mind, do something else"),
    ];

    let outcome = repair_transcript(messages, &target(ProtocolId::AnthropicMessages));

    let last = outcome.messages.last().unwrap();
    assert!(matches!(
        last.parts[0].content,
        Content::ToolResult { is_error: true, .. }
    ));
    assert!(matches!(last.parts[1].content, Content::Text { .. }));
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| matches!(note, RepairNote::SynthesizedToolResult { .. }))
    );
}

#[test]
fn synthesizes_a_result_when_the_call_is_the_last_message() {
    let messages = vec![
        Message::user("go"),
        Message::assistant(vec![tool_call("call_1")]),
    ];

    let outcome = repair_transcript(messages, &target(ProtocolId::AnthropicMessages));

    assert_eq!(outcome.messages.len(), 3);
    assert!(matches!(
        outcome.messages[2].parts[0].content,
        Content::ToolResult { .. }
    ));
}

#[test]
fn drops_tool_results_that_answer_no_call() {
    let messages = vec![
        Message::user("go"),
        Message::new(keel_llm_types::Role::User).with_part(Part::tool_result(
            ToolCallId::new("call_missing"),
            "stale",
            false,
        )),
    ];

    let outcome = repair_transcript(messages, &target(ProtocolId::AnthropicMessages));

    assert_eq!(outcome.messages.len(), 1);
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| matches!(note, RepairNote::DroppedToolResult { .. }))
    );
}

#[test]
fn hoists_system_messages_that_split_a_call_from_its_result() {
    let messages = vec![
        Message::user("go"),
        Message::assistant(vec![tool_call("call_1")]),
        Message::system("reminder injected by the host"),
        Message::new(keel_llm_types::Role::User).with_part(Part::tool_result(
            ToolCallId::new("call_1"),
            "ok",
            false,
        )),
    ];

    let outcome = repair_transcript(messages, &target(ProtocolId::AnthropicMessages));

    let roles: Vec<_> = outcome.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            keel_llm_types::Role::User,
            keel_llm_types::Role::System,
            keel_llm_types::Role::Assistant,
            keel_llm_types::Role::User,
        ]
    );
    assert!(
        outcome
            .notes
            .iter()
            .any(|note| matches!(note, RepairNote::HoistedSystemMessages { .. }))
    );
}

#[test]
fn downgrades_images_the_target_cannot_see() {
    let messages = vec![
        Message::new(keel_llm_types::Role::User)
            .with_part(Part::text("what is this?"))
            .with_part(Part::image("image/png", "AAAA")),
    ];

    let target = target(ProtocolId::OpenAiChat).with_images(false);
    let outcome = repair_transcript(messages, &target);

    assert_eq!(outcome.messages[0].parts.len(), 2);
    assert!(matches!(
        outcome.messages[0].parts[1].content,
        Content::Text { .. }
    ));
}

#[test]
fn drops_turns_with_no_replayable_content() {
    let reasoning = Part::reasoning(
        "thoughts",
        None,
        source(ProtocolId::OpenAiChat, "openai", "gpt-4.1"),
    );
    let messages = vec![Message::user("go"), Message::assistant(vec![reasoning])];

    let outcome = repair_transcript(messages, &target(ProtocolId::AnthropicMessages));

    assert_eq!(outcome.messages.len(), 1);
    assert_eq!(outcome.messages[0].role, keel_llm_types::Role::User);
}

#[test]
fn repair_is_deterministic_when_it_synthesizes_turns() {
    let messages = vec![
        Message::user("go"),
        Message::assistant(vec![tool_call("call_1")]),
    ];
    let target = target(ProtocolId::AnthropicMessages);

    let first = repair_transcript(messages.clone(), &target);
    let second = repair_transcript(messages, &target);

    assert_eq!(first.messages, second.messages);
    assert_eq!(first.notes, second.notes);
}

#[test]
fn messages_round_trip_through_json() {
    let message = Message::assistant(vec![
        Part::text("using a tool"),
        Part::tool_call(
            ToolCallId::new("call_1"),
            "read_file",
            "{}",
            source(ProtocolId::AnthropicMessages, "anthropic", "claude"),
        ),
    ]);
    let json = serde_json::to_string(&message).unwrap();
    let back: Message = serde_json::from_str(&json).unwrap();
    assert_eq!(back, message);
}
