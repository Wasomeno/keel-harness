//! Compaction policy: the checkpoint prompt, the replacement view, and the
//! budgets that keep both the summary request and the next request inside the
//! model's context window.
//!
//! The session owns the side request and the log record. This module is pure so
//! the selection rules can be tested without a server.

use keel_llm_types::{Content, Message, Role, Usage};
use serde::{Deserialize, Serialize};

use crate::tokens::{estimate_message_tokens, estimate_messages_tokens};

/// The prompt the summarizer receives as the final user message.
pub const COMPACTION_PROMPT: &str = include_str!("prompts/compaction.md");

/// The fixed opening every handoff summary message carries.
pub const SUMMARY_PREFIX: &str = include_str!("prompts/compaction_prefix.md");

/// Codex keeps at most 20,000 tokens of recent user messages. The same cap
/// applies here, narrowed further for small context windows.
pub const COMPACT_USER_MESSAGE_MAX_TOKENS: u64 = 20_000;

/// Upper bound for the summary response. The request uses whatever room is
/// left, so this is a cap rather than a target.
pub const SUMMARY_MAX_OUTPUT_TOKENS: u32 = 4_096;

/// Below this, a summary is too short to be a useful handoff; fail instead of
/// sending a request that would leave no room to write one.
pub const SUMMARY_MIN_OUTPUT_TOKENS: u32 = 128;

/// Recent user messages get at most this fraction of the context window.
const USER_MESSAGE_WINDOW_FRACTION: u64 = 4;

/// The summary also gets at most this fraction of the window, so the
/// replacement view leaves room for the next request.
const SUMMARY_WINDOW_FRACTION: u64 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    /// The trigger rule fired before a normal request.
    Threshold,
    /// The provider reported a context overflow; compact and retry once.
    Overflow,
}

impl CompactionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Threshold => "threshold",
            Self::Overflow => "overflow",
        }
    }
}

/// What one compaction did, for the status event and tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionStats {
    pub reason: CompactionReason,
    pub before_messages: usize,
    pub after_messages: usize,
    pub kept_user_messages: usize,
    pub estimated_tokens_before: u64,
    pub estimated_tokens_after: u64,
    pub summary_output_tokens: u32,
    /// Usage reported by the summary request. Side-request usage is reported
    /// here instead of being added to the assistant message, so the trigger
    /// keeps reading the last main request's numbers.
    pub usage: Usage,
}

/// Room left for the summary response given the estimated summary request.
/// `None` means the summary request cannot hold even a minimal handoff.
pub fn summary_output_budget(context_window: u32, input_estimate: u64) -> Option<u32> {
    let available = u64::from(context_window).saturating_sub(input_estimate);
    let limit = available
        .min(u64::from(SUMMARY_MAX_OUTPUT_TOKENS))
        .min(u64::from(context_window) / SUMMARY_WINDOW_FRACTION);
    (limit >= u64::from(SUMMARY_MIN_OUTPUT_TOKENS)).then_some(limit as u32)
}

/// Token budget for verbatim recent user messages.
pub fn user_message_budget(context_window: u32) -> u64 {
    (u64::from(context_window) / USER_MESSAGE_WINDOW_FRACTION).min(COMPACT_USER_MESSAGE_MAX_TOKENS)
}

/// A user message that may be kept verbatim.
///
/// Only plain user turns qualify. Tool results are also `user` messages on the
/// wire, but a result cannot cross the cut without its call and assistant turns
/// are not kept, so a result-only message is dropped with the call it answered.
pub fn is_verbatim_user_message(message: &Message) -> bool {
    if message.role != Role::User {
        return false;
    }
    // A previous handoff summary is a user message on the wire, but it is
    // synthesized context, not a user instruction. Keep only the newest one.
    if message.text().starts_with(SUMMARY_PREFIX) {
        return false;
    }
    let mut has_content = false;
    for part in &message.parts {
        match &part.content {
            Content::ToolCall { .. } | Content::ToolResult { .. } => return false,
            Content::Text { text } if !text.trim().is_empty() => has_content = true,
            Content::Image { .. } => has_content = true,
            _ => {}
        }
    }
    has_content
}

/// The summary message: one user message with a fixed handoff prefix.
pub fn summary_message(summary: &str) -> Message {
    // The prefix file ends with a newline, so one more leaves a blank line
    // between "Summary:" and the text.
    Message::user(format!("{SUMMARY_PREFIX}\n{}", summary.trim()))
}

/// Builds the replacement view.
///
/// Newest user messages are kept first until the budget is exhausted, then put
/// back in their original order. The summary message follows them. No assistant
/// turn, tool call or tool result is copied, so the view needs no synthesized
/// results to be replay-safe.
pub fn compacted_view(
    history: &[Message],
    summary: &str,
    context_window: u32,
) -> (Vec<Message>, usize) {
    let budget = user_message_budget(context_window);
    let mut kept: Vec<Message> = Vec::new();
    let mut kept_tokens = 0u64;

    for message in history.iter().rev() {
        if !is_verbatim_user_message(message) {
            continue;
        }
        let tokens = estimate_message_tokens(message);
        if kept_tokens + tokens > budget {
            break;
        }
        kept_tokens += tokens;
        kept.push(message.clone());
    }

    kept.reverse();
    let kept_count = kept.len();
    kept.push(summary_message(summary));
    (kept, kept_count)
}

/// Estimated tokens of the view, excluding system text and tools.
pub fn view_tokens(view: &[Message]) -> u64 {
    estimate_messages_tokens(view)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(text: &str) -> Message {
        Message::assistant(vec![keel_llm_types::Part::text(text)])
    }

    #[test]
    fn the_summary_message_always_starts_with_the_handoff_prefix() {
        let message = summary_message("  progress: wrote the parser  ");
        assert_eq!(message.role, Role::User);
        let text = message.text();
        assert!(text.starts_with(SUMMARY_PREFIX), "{text}");
        assert!(text.ends_with("progress: wrote the parser"));
    }

    #[test]
    fn only_plain_user_messages_are_kept_verbatim() {
        let user = Message::user("keep me");
        let assistant = assistant("drop me");
        let result = Message::new(Role::User).with_part(keel_llm_types::Part::tool_result(
            "call_1".into(),
            "output",
            false,
        ));
        let call = Message::assistant(vec![keel_llm_types::Part::tool_call(
            "call_1".into(),
            "read",
            "{}",
            keel_llm_types::Source {
                protocol: keel_llm_types::ProtocolId::OpenAiChat,
                provider: "test".into(),
                model: "gpt-4.1".into(),
            },
        )]);
        let empty = Message::user("   ");
        let handoff = summary_message("old handoff");

        assert!(is_verbatim_user_message(&user));
        assert!(!is_verbatim_user_message(&assistant));
        assert!(!is_verbatim_user_message(&result));
        assert!(!is_verbatim_user_message(&call));
        assert!(!is_verbatim_user_message(&empty));
        assert!(
            !is_verbatim_user_message(&handoff),
            "an older handoff summary is not a user instruction"
        );
    }

    #[test]
    fn the_view_keeps_recent_user_messages_in_order_then_the_summary() {
        let history = vec![
            Message::user("first"),
            assistant("answer"),
            Message::user("second"),
            Message::user("third"),
        ];
        let (view, kept) = compacted_view(&history, "handoff", 100_000);
        assert_eq!(kept, 3);
        let texts: Vec<String> = view.iter().map(Message::text).collect();
        assert_eq!(texts[0], "first");
        assert_eq!(texts[1], "second");
        assert_eq!(texts[2], "third");
        assert!(texts[3].starts_with(SUMMARY_PREFIX));
        assert!(view.iter().all(|message| message.role == Role::User));
    }

    #[test]
    fn a_small_window_keeps_the_newest_messages_that_fit() {
        // The fraction gives 25 tokens here; each message costs about 9.
        let history = vec![
            Message::user("old message"),
            Message::user("middle message"),
            Message::user("new message"),
        ];
        let (view, kept) = compacted_view(&history, "handoff", 100);
        assert_eq!(kept, 2, "{view:?}");
        let texts: Vec<String> = view.iter().map(Message::text).collect();
        assert_eq!(texts[0], "middle message");
        assert_eq!(texts[1], "new message");
        assert!(texts[2].starts_with(SUMMARY_PREFIX));
    }

    #[test]
    fn a_tool_result_never_crosses_the_cut_without_its_call() {
        let history = vec![
            Message::user("before"),
            Message::assistant(vec![keel_llm_types::Part::tool_call(
                "call_1".into(),
                "read",
                "{\"path\":\"a\"}",
                keel_llm_types::Source {
                    protocol: keel_llm_types::ProtocolId::OpenAiChat,
                    provider: "test".into(),
                    model: "gpt-4.1".into(),
                },
            )]),
            Message::new(Role::User).with_part(keel_llm_types::Part::tool_result(
                "call_1".into(),
                "contents",
                false,
            )),
            Message::user("after"),
        ];
        let (view, kept) = compacted_view(&history, "handoff", 100_000);
        assert_eq!(kept, 2);
        assert!(view.iter().all(|message| !message.has_tool_calls()));
        assert!(
            view.iter()
                .all(|message| message.tool_results().next().is_none())
        );
    }

    #[test]
    fn the_summary_budget_uses_the_room_left_in_the_window() {
        assert_eq!(summary_output_budget(100_000, 10_000), Some(4_096));
        assert_eq!(
            summary_output_budget(10_000, 0),
            Some(2_500),
            "a small window also caps the summary at a quarter of it"
        );
        assert_eq!(summary_output_budget(10_000, 9_600), Some(400));
        assert_eq!(summary_output_budget(10_000, 9_900), None);
        assert_eq!(summary_output_budget(10_000, 20_000), None);
    }

    #[test]
    fn the_user_budget_is_capped_by_the_window_fraction_and_the_codex_limit() {
        assert_eq!(user_message_budget(10_000), 2_500);
        assert_eq!(
            user_message_budget(1_000_000),
            COMPACT_USER_MESSAGE_MAX_TOKENS
        );
        assert_eq!(user_message_budget(0), 0);
    }
}
