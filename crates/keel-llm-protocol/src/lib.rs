//! First-party wire protocols.
//!
//! Phase 1 ships OpenAI Chat Completions and Anthropic Messages. Responses and
//! Google/Antigravity are catalogued but not implemented yet; `for_protocol`
//! returns `None` so route resolution fails with a typed error instead of
//! silently building a broken route.

pub mod anthropic_messages;
pub mod openai_chat;
mod protocol;

pub use protocol::{EncodedRequest, Protocol, StreamDecoder};

use keel_llm_types::{Compat, ProtocolId};

pub fn for_protocol(id: ProtocolId, compat: Compat) -> Option<Box<dyn Protocol>> {
    match id {
        ProtocolId::OpenAiChat => Some(Box::new(openai_chat::OpenAiChat::new(compat))),
        ProtocolId::AnthropicMessages => {
            Some(Box::new(anthropic_messages::AnthropicMessages::new(compat)))
        }
        ProtocolId::OpenAiResponses | ProtocolId::GoogleGenerative => None,
    }
}

pub fn is_implemented(id: ProtocolId) -> bool {
    matches!(id, ProtocolId::OpenAiChat | ProtocolId::AnthropicMessages)
}
