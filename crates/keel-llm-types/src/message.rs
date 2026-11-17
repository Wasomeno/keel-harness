//! Canonical conversation messages.
//!
//! A message is an ordered list of parts plus the outcome of the turn that
//! produced it. Tool calls and their results are parts, not a parallel
//! structure, so transcript repair can reason about one sequence.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::content::{Content, Part};
use crate::ids::{MessageId, ToolCallId};
use crate::usage::Usage;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
    Refusal,
    ContentFilter,
    /// The caller cancelled the turn.
    Aborted,
    /// The provider or transport failed.
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub role: Role,
    pub parts: Vec<Part>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Usage::is_zero")]
    pub usage: Usage,
    /// Set when the turn failed. The partial parts are still present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Message {
    pub fn new(role: Role) -> Self {
        Self::with_id(MessageId::new(fresh_id()), role)
    }

    /// Construct with a caller-chosen id. Used where determinism matters, such
    /// as synthesized turns during transcript repair.
    pub fn with_id(id: MessageId, role: Role) -> Self {
        Self {
            id,
            role,
            parts: Vec::new(),
            stop: None,
            usage: Usage::default(),
            error: None,
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self::new(Role::System).with_part(Part::text(text))
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::new(Role::User).with_part(Part::text(text))
    }

    pub fn assistant(parts: Vec<Part>) -> Self {
        let mut message = Self::new(Role::Assistant);
        message.parts = parts;
        message
    }

    pub fn with_part(mut self, part: Part) -> Self {
        self.parts.push(part);
        self
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        for part in &self.parts {
            if let Content::Text { text } = &part.content {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }

    pub fn has_tool_calls(&self) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part.content, Content::ToolCall { .. }))
    }

    pub fn tool_call_ids(&self) -> Vec<ToolCallId> {
        self.parts
            .iter()
            .filter_map(|part| match &part.content {
                Content::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn tool_results(&self) -> impl Iterator<Item = (&ToolCallId, &str, bool)> {
        self.parts.iter().filter_map(|part| match &part.content {
            Content::ToolResult {
                call_id,
                output,
                is_error,
            } => Some((call_id, output.as_str(), *is_error)),
            _ => None,
        })
    }

    /// A turn that must not be replayed: the provider errored, or the caller
    /// aborted it after partial output.
    pub fn is_failed(&self) -> bool {
        self.error.is_some() || matches!(self.stop, Some(StopReason::Aborted | StopReason::Error))
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

fn fresh_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("msg_{millis:x}_{n:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_stable() {
        let a = Message::user("a");
        let b = Message::user("b");
        assert_ne!(a.id, b.id);
        assert_eq!(a.id.as_str(), a.id.as_str());
    }

    #[test]
    fn text_joins_only_text_parts() {
        let message = Message::new(Role::User)
            .with_part(Part::text("first"))
            .with_part(Part::image("image/png", "AAAA"))
            .with_part(Part::text("second"));
        assert_eq!(message.text(), "first\nsecond");
    }
}
