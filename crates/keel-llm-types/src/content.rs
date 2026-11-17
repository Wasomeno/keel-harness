//! Canonical content parts and the provenance attached to them.

use serde::{Deserialize, Serialize};

use crate::ids::{ModelId, ProtocolId, ProviderId, ToolCallId};

/// Who produced a content part.
///
/// Replay validity is a function of origin, not of content: a thinking block is
/// only portable back to the model that signed it. The source travels on the
/// part so a later repair pass does not have to reconstruct it from the
/// destination's namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub protocol: ProtocolId,
    pub provider: ProviderId,
    pub model: ModelId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
    },
    /// Model-visible reasoning. `signature` is the provider's opaque proof that
    /// this text came from that model, when the protocol has one.
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Opaque reasoning the provider refuses to disclose (Anthropic
    /// `redacted_thinking`). Only replayed to its origin.
    RedactedReasoning {
        data: String,
    },
    ToolCall {
        id: ToolCallId,
        name: String,
        /// Raw JSON text. Never assume it parses until the turn is complete.
        arguments: String,
    },
    ToolResult {
        call_id: ToolCallId,
        output: String,
        #[serde(default)]
        is_error: bool,
    },
    Image {
        mime: String,
        /// Base64 payload, without the `data:` prefix.
        data: String,
    },
}

impl Content {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Reasoning { .. } => "reasoning",
            Self::RedactedReasoning { .. } => "redacted_reasoning",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::Image { .. } => "image",
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }

    pub const fn is_reasoning(&self) -> bool {
        matches!(
            self,
            Self::Reasoning { .. } | Self::RedactedReasoning { .. }
        )
    }
}

/// A complete tool call, used by the event stream once arguments are final.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    /// Raw JSON text. The session loop parses it once and reports a structured
    /// error instead of failing the turn.
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    pub content: Content,
    /// Present on every part produced by a model. Absent on user/system input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
}

impl Part {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: Content::Text { text: text.into() },
            source: None,
        }
    }

    pub fn reasoning(text: impl Into<String>, signature: Option<String>, source: Source) -> Self {
        Self {
            content: Content::Reasoning {
                text: text.into(),
                signature,
            },
            source: Some(source),
        }
    }

    pub fn redacted_reasoning(data: impl Into<String>, source: Source) -> Self {
        Self {
            content: Content::RedactedReasoning { data: data.into() },
            source: Some(source),
        }
    }

    pub fn tool_call(
        id: ToolCallId,
        name: impl Into<String>,
        arguments: impl Into<String>,
        source: Source,
    ) -> Self {
        Self {
            content: Content::ToolCall {
                id,
                name: name.into(),
                arguments: arguments.into(),
            },
            source: Some(source),
        }
    }

    pub fn tool_result(call_id: ToolCallId, output: impl Into<String>, is_error: bool) -> Self {
        Self {
            content: Content::ToolResult {
                call_id,
                output: output.into(),
                is_error,
            },
            source: None,
        }
    }

    pub fn image(mime: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            content: Content::Image {
                mime: mime.into(),
                data: data.into(),
            },
            source: None,
        }
    }

    pub fn with_source(mut self, source: Source) -> Self {
        self.source = Some(source);
        self
    }

    pub fn as_text(&self) -> Option<&str> {
        self.content.as_text()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_round_trips_as_tagged_json() {
        let part = Part::text("hello");
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["content"]["type"], "text");
        assert_eq!(json["content"]["text"], "hello");
        let back: Part = serde_json::from_value(json).unwrap();
        assert_eq!(back, part);
    }

    #[test]
    fn provenance_survives_a_round_trip() {
        let source = Source {
            protocol: ProtocolId::AnthropicMessages,
            provider: ProviderId::new("anthropic"),
            model: ModelId::new("claude-sonnet-4-5"),
        };
        let part = Part::reasoning("thinking", Some("sig".into()), source.clone());
        let json = serde_json::to_string(&part).unwrap();
        let back: Part = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, Some(source));
    }
}
