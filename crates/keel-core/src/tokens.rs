//! Conservative token estimates for compaction decisions.
//!
//! keel does not ship a tokenizer. The estimate therefore only has to be
//! *conservative*: a request the estimate says fits should fit. One token per
//! three bytes overestimates English prose (which is closer to four bytes per
//! token) and stays honest for code and JSON, where tokens are denser. Every
//! message and part adds a small fixed overhead for role markers, delimiters
//! and protocol framing. Images carry a flat estimate instead of their base64
//! payload, which is not sent as text.
//!
//! These numbers decide when compaction runs and whether the summary request
//! fits. They are covered by unit tests and documented in
//! `docs/architecture.md`; changing them changes the trigger boundary.

use keel_llm_types::{Content, Message, ToolSpec};

/// Bytes charged per token. Lower is more conservative.
const BYTES_PER_TOKEN: u64 = 3;

/// Per-message framing: role markers and separators.
const MESSAGE_OVERHEAD: u64 = 4;

/// Per-part framing inside a message.
const PART_OVERHEAD: u64 = 2;

/// Per-tool framing in the request.
const TOOL_OVERHEAD: u64 = 8;

/// An image is not text; charge a flat, deliberately high estimate.
const IMAGE_TOKENS: u64 = 2_000;

/// Tokens for `text` at the conservative bytes-per-token rate.
pub fn estimate_text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(BYTES_PER_TOKEN)
}

/// Tokens for one canonical message, including its fixed overhead.
pub fn estimate_message_tokens(message: &Message) -> u64 {
    let parts: u64 = message
        .parts
        .iter()
        .map(|part| match &part.content {
            Content::Text { text } => estimate_text_tokens(text) + PART_OVERHEAD,
            Content::Reasoning { text, .. } => estimate_text_tokens(text) + PART_OVERHEAD,
            Content::RedactedReasoning { data } => estimate_text_tokens(data) + PART_OVERHEAD,
            Content::ToolCall {
                id,
                name,
                arguments,
            } => {
                estimate_text_tokens(id.as_str())
                    + estimate_text_tokens(name)
                    + estimate_text_tokens(arguments)
                    + PART_OVERHEAD
            }
            Content::ToolResult {
                call_id, output, ..
            } => {
                estimate_text_tokens(call_id.as_str())
                    + estimate_text_tokens(output)
                    + PART_OVERHEAD
            }
            Content::Image { .. } => IMAGE_TOKENS,
        })
        .sum();
    MESSAGE_OVERHEAD + parts
}

/// Tokens for a whole message list.
pub fn estimate_messages_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_message_tokens).sum()
}

/// Tokens for the tool definitions a request advertises.
pub fn estimate_tools_tokens(tools: &[ToolSpec]) -> u64 {
    tools
        .iter()
        .map(|tool| {
            TOOL_OVERHEAD
                + estimate_text_tokens(&tool.name)
                + estimate_text_tokens(&tool.description)
                + estimate_text_tokens(&tool.input_schema.to_string())
        })
        .sum()
}

/// The input half of a request: system text, messages and tool definitions.
pub fn estimate_request_input_tokens(
    system: Option<&str>,
    messages: &[Message],
    tools: &[ToolSpec],
) -> u64 {
    let system = system.map_or(0, |text| estimate_text_tokens(text) + MESSAGE_OVERHEAD);
    system + estimate_messages_tokens(messages) + estimate_tools_tokens(tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_llm_types::{Content, Message, Part};

    #[test]
    fn empty_text_costs_nothing() {
        assert_eq!(estimate_text_tokens(""), 0);
    }

    #[test]
    fn the_text_rate_is_one_token_per_three_bytes_rounded_up() {
        assert_eq!(estimate_text_tokens("ab"), 1);
        assert_eq!(estimate_text_tokens("abc"), 1);
        assert_eq!(estimate_text_tokens("abcd"), 2);
        // CJK is three bytes per character in UTF-8, so one token per character
        // is a safe upper bound here.
        assert_eq!(estimate_text_tokens("界界"), 2);
    }

    #[test]
    fn a_message_carries_a_fixed_overhead() {
        let message = Message::user("");
        assert_eq!(
            estimate_message_tokens(&message),
            MESSAGE_OVERHEAD + PART_OVERHEAD
        );
        let two_parts = Message::new(keel_llm_types::Role::User)
            .with_part(Part::text("abc"))
            .with_part(Part::text("def"));
        assert_eq!(
            estimate_message_tokens(&two_parts),
            MESSAGE_OVERHEAD + 2 * (PART_OVERHEAD + 1)
        );
    }

    #[test]
    fn tool_calls_and_results_include_ids_and_payloads() {
        let call = Message::new(keel_llm_types::Role::Assistant).with_part(Part::tool_call(
            "call_1".into(),
            "read",
            "{\"path\":\"a\"}",
            keel_llm_types::Source {
                protocol: keel_llm_types::ProtocolId::OpenAiChat,
                provider: "test".into(),
                model: "gpt-4.1".into(),
            },
        ));
        let call_tokens = estimate_message_tokens(&call);
        assert!(call_tokens > MESSAGE_OVERHEAD);

        let result = Message::new(keel_llm_types::Role::User).with_part(Part::tool_result(
            "call_1".into(),
            "file contents",
            false,
        ));
        assert!(estimate_message_tokens(&result) > MESSAGE_OVERHEAD);
    }

    #[test]
    fn an_image_is_charged_flat_not_by_its_base64_length() {
        let message = Message::new(keel_llm_types::Role::User)
            .with_part(Part::image("image/png", "A".repeat(30_000)));
        assert_eq!(
            estimate_message_tokens(&message),
            MESSAGE_OVERHEAD + IMAGE_TOKENS
        );
    }

    #[test]
    fn the_request_estimate_adds_system_and_tools() {
        let messages = vec![Message::user("hello")];
        let tools = vec![ToolSpec {
            name: "read".to_owned(),
            description: "read a file".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let without = estimate_request_input_tokens(None, &messages, &[]);
        let with_system = estimate_request_input_tokens(Some("be terse"), &messages, &[]);
        let with_tools = estimate_request_input_tokens(None, &messages, &tools);
        assert!(with_system > without);
        assert!(with_tools > without);
    }

    #[test]
    fn estimates_grow_with_content() {
        let short = Message::user("a".repeat(30));
        let long = Message::user("a".repeat(300));
        assert!(estimate_message_tokens(&long) > estimate_message_tokens(&short));
        // The estimate is never below a naive four-bytes-per-token count.
        let text = "x".repeat(401);
        assert!(estimate_text_tokens(&text) >= text.len().div_ceil(4) as u64);
    }

    #[test]
    fn content_kind_does_not_change_the_text_rate() {
        let text = "same bytes";
        let as_text = Message::new(keel_llm_types::Role::Assistant).with_part(Part::text(text));
        let as_reasoning = Message::new(keel_llm_types::Role::Assistant).with_part(Part {
            content: Content::Reasoning {
                text: text.to_owned(),
                signature: None,
            },
            source: None,
        });
        assert_eq!(
            estimate_message_tokens(&as_text),
            estimate_message_tokens(&as_reasoning)
        );
    }
}
