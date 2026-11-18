//! Conformance against recorded provider traffic.
//!
//! The `.sse` files under `fixtures/recorded` are real response bodies, not
//! hand-written frames (see `PROVENANCE.md`). Replaying them does not prove the
//! encoder is right, but it does prove the decoder survives what providers
//! actually send.

use keel_llm_protocol::Protocol;
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::SseDecoder;
use keel_llm_types::{Compat, LlmEvent, ProtocolId, StopReason, ToolCall, Usage};

const AZURE_TEXT: &str = include_str!("fixtures/recorded/openai-chat/azure-text.sse");
const GROQ_TOOLS: &str = include_str!("fixtures/recorded/openai-chat/groq-parallel-tool-calls.sse");
const GEMINI_TOOLS: &str =
    include_str!("fixtures/recorded/openai-chat/gemini-parallel-tool-calls.sse");

#[derive(Debug, Default)]
struct Decoded {
    text: String,
    reasoning: String,
    calls: Vec<ToolCall>,
    stop: Option<StopReason>,
    usage: Usage,
    warnings: Vec<String>,
}

fn decode(body: &str) -> Decoded {
    let protocol = OpenAiChat::new(Compat::for_protocol(ProtocolId::OpenAiChat));
    let mut decoder = protocol.decoder();
    let mut sse = SseDecoder::new();
    let mut events = Vec::new();

    for frame in sse.push(body.as_bytes()) {
        events.extend(
            decoder
                .frame(&frame.data)
                .expect("every recorded frame decodes"),
        );
    }
    for frame in sse.finish() {
        events.extend(
            decoder
                .frame(&frame.data)
                .expect("every recorded frame decodes"),
        );
    }
    events.extend(decoder.finish().expect("the stream finishes"));

    let mut decoded = Decoded::default();
    for event in events {
        match event {
            LlmEvent::TextDelta { delta, .. } => decoded.text.push_str(&delta),
            LlmEvent::ReasoningDelta { delta, .. } => decoded.reasoning.push_str(&delta),
            LlmEvent::ToolCall { call, .. } => decoded.calls.push(call),
            LlmEvent::Done { stop } => decoded.stop = Some(stop),
            LlmEvent::Usage(usage) => decoded.usage = usage,
            LlmEvent::Warning(warning) => decoded.warnings.push(warning),
            _ => {}
        }
    }
    decoded
}

#[test]
fn azure_text_stream_decodes_to_one_turn() {
    let decoded = decode(AZURE_TEXT);

    assert_eq!(decoded.text, "hello");
    assert!(decoded.calls.is_empty());
    assert_eq!(decoded.stop, Some(StopReason::EndTurn));
    assert_eq!(decoded.usage.input_tokens, 13);
    assert_eq!(decoded.usage.output_tokens, 5);
    assert!(decoded.warnings.is_empty());
}

#[test]
fn groq_parallel_tool_calls_stay_separate() {
    let decoded = decode(GROQ_TOOLS);

    assert!(decoded.text.is_empty());
    assert_eq!(decoded.calls.len(), 2);

    let cities: Vec<_> = decoded
        .calls
        .iter()
        .map(|call| {
            assert_eq!(call.name, "lookup_weather");
            let arguments: serde_json::Value =
                serde_json::from_str(&call.arguments).expect("arguments are JSON");
            arguments["city"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(cities, ["Paris", "London"]);
    assert_eq!(decoded.stop, Some(StopReason::ToolUse));
    assert_eq!(decoded.usage.input_tokens, 326);
    assert_eq!(decoded.usage.output_tokens, 54);
}

/// Gemini's OpenAI-compatible endpoint omits `index` on tool calls and sends
/// one call per chunk. Keying by index alone used to merge them into a single
/// malformed call.
#[test]
fn compatible_tool_calls_without_an_index_do_not_merge() {
    let decoded = decode(GEMINI_TOOLS);

    assert!(decoded.text.is_empty());
    assert_eq!(decoded.calls.len(), 2);
    let ids: Vec<_> = decoded.calls.iter().map(|call| call.id.as_str()).collect();
    assert_eq!(ids, ["call_723181", "call_723184"]);

    let cities: Vec<_> = decoded
        .calls
        .iter()
        .map(|call| {
            assert_eq!(call.name, "get_weather");
            let arguments: serde_json::Value =
                serde_json::from_str(&call.arguments).expect("arguments are JSON");
            arguments["city"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(cities, ["Paris", "Tokyo"]);

    // The provider says `stop`, but a turn with tool calls is a tool-use turn.
    assert_eq!(decoded.stop, Some(StopReason::ToolUse));
    assert!(decoded.warnings.is_empty());
}
