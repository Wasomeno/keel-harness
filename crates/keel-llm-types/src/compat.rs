//! Per-model wire compatibility.
//!
//! Quirks live in data, not in a bespoke provider client. A catalog entry starts
//! from `Compat::for_protocol` and overrides only what the model does
//! differently.

use serde::{Deserialize, Serialize};

use crate::ids::ProtocolId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Compat {
    /// Some OpenAI reasoning models reject `max_tokens` and require
    /// `max_completion_tokens`.
    pub max_tokens_field: MaxTokensField,
    pub supports_images: bool,
    pub supports_tools: bool,
    pub supports_reasoning: bool,
    /// OpenAI sends usage on a final chunk only when asked.
    pub supports_stream_options: bool,
    pub supports_temperature: bool,
    pub supports_tool_choice_required: bool,
    /// OpenAI-compatible chat puts the system prompt in the message list;
    /// Anthropic takes a top-level `system` field.
    pub system_in_messages: bool,
}

impl Compat {
    pub const fn for_protocol(protocol: ProtocolId) -> Self {
        match protocol {
            ProtocolId::OpenAiChat => Self {
                max_tokens_field: MaxTokensField::MaxTokens,
                supports_images: true,
                supports_tools: true,
                supports_reasoning: false,
                supports_stream_options: true,
                supports_temperature: true,
                supports_tool_choice_required: true,
                system_in_messages: true,
            },
            ProtocolId::OpenAiResponses => Self {
                max_tokens_field: MaxTokensField::MaxCompletionTokens,
                supports_images: true,
                supports_tools: true,
                supports_reasoning: true,
                supports_stream_options: false,
                supports_temperature: false,
                supports_tool_choice_required: true,
                system_in_messages: false,
            },
            ProtocolId::AnthropicMessages => Self {
                max_tokens_field: MaxTokensField::MaxTokens,
                supports_images: true,
                supports_tools: true,
                supports_reasoning: true,
                supports_stream_options: false,
                supports_temperature: true,
                supports_tool_choice_required: true,
                system_in_messages: false,
            },
            ProtocolId::GoogleGenerative => Self {
                max_tokens_field: MaxTokensField::MaxTokens,
                supports_images: true,
                supports_tools: true,
                supports_reasoning: true,
                supports_stream_options: false,
                supports_temperature: true,
                supports_tool_choice_required: false,
                system_in_messages: false,
            },
        }
    }

    pub fn images(mut self, enabled: bool) -> Self {
        self.supports_images = enabled;
        self
    }

    pub fn tools(mut self, enabled: bool) -> Self {
        self.supports_tools = enabled;
        self
    }

    pub fn reasoning(mut self, enabled: bool) -> Self {
        self.supports_reasoning = enabled;
        self
    }

    pub fn temperature(mut self, enabled: bool) -> Self {
        self.supports_temperature = enabled;
        self
    }

    pub fn max_tokens_field(mut self, field: MaxTokensField) -> Self {
        self.max_tokens_field = field;
        self
    }
}

impl Default for Compat {
    fn default() -> Self {
        Self::for_protocol(ProtocolId::OpenAiChat)
    }
}
