//! First-party wire protocols.
//!
//! OpenAI Chat Completions, OpenAI Responses and Anthropic Messages are
//! implemented. Google Generative is catalogued but not implemented yet;
//! `for_protocol` returns `None` so route resolution fails with a typed error
//! instead of silently building a broken route.

pub mod anthropic_messages;
mod cache;
pub mod openai_chat;
pub mod openai_responses;
mod protocol;

pub use protocol::{EncodedRequest, Protocol, StreamDecoder};

use keel_llm_types::{Compat, ProtocolId};

pub fn for_protocol(id: ProtocolId, compat: Compat) -> Option<Box<dyn Protocol>> {
    match id {
        ProtocolId::OpenAiChat => Some(Box::new(openai_chat::OpenAiChat::new(compat))),
        ProtocolId::OpenAiResponses => {
            Some(Box::new(openai_responses::OpenAiResponses::new(compat)))
        }
        ProtocolId::AnthropicMessages => {
            Some(Box::new(anthropic_messages::AnthropicMessages::new(compat)))
        }
        ProtocolId::GoogleGenerative => None,
    }
}

pub fn is_implemented(id: ProtocolId) -> bool {
    matches!(
        id,
        ProtocolId::OpenAiChat | ProtocolId::OpenAiResponses | ProtocolId::AnthropicMessages
    )
}
