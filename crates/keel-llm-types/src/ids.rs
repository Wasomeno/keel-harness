//! Identifiers used across the provider seam.
//!
//! Identifiers are plain newtypes so a provider id, a model id and a tool call
//! id cannot be swapped by accident. `ProtocolId` is a closed set: keel owns
//! the protocol modules, so an unknown protocol is a compile-time concern, not
//! a runtime string.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(ProviderId);
string_id!(ModelId);
string_id!(MessageId);
string_id!(ToolCallId);

/// The wire protocol a model speaks.
///
/// This is a property of the model entry, not of the provider: one provider can
/// serve several protocols from one base URL (OpenCode, CommandCode).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ProtocolId {
    #[serde(rename = "openai-chat")]
    OpenAiChat,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "google-generative")]
    GoogleGenerative,
}

impl ProtocolId {
    pub const ALL: [Self; 4] = [
        Self::OpenAiChat,
        Self::OpenAiResponses,
        Self::AnthropicMessages,
        Self::GoogleGenerative,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai-chat",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GoogleGenerative => "google-generative",
        }
    }

    /// Character and length rules a tool call id must satisfy on the wire.
    ///
    /// Conservative on purpose: a caller that normalizes to the tightest rule
    /// can replay the same transcript to any of keel's protocols.
    pub const fn tool_id_format(self) -> ToolIdFormat {
        match self {
            Self::OpenAiChat | Self::OpenAiResponses | Self::AnthropicMessages => ToolIdFormat {
                max_len: 64,
                allowed_extra: "_-",
            },
            Self::GoogleGenerative => ToolIdFormat {
                max_len: 128,
                allowed_extra: "_-.:",
            },
        }
    }
}

impl fmt::Display for ProtocolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProtocolId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|protocol| protocol.as_str() == value)
            .ok_or_else(|| format!("unknown protocol: {value}"))
    }
}

/// Constraints on a tool call id for one protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolIdFormat {
    pub max_len: usize,
    pub allowed_extra: &'static str,
}

impl ToolIdFormat {
    pub fn allows(self, c: char) -> bool {
        c.is_ascii_alphanumeric() || self.allowed_extra.contains(c)
    }

    /// Rewrite `raw` into a valid, unused id.
    ///
    /// The result is deterministic for a given transcript: disallowed bytes are
    /// replaced with `_`, the id is truncated to `max_len`, and collisions get a
    /// numeric suffix.
    pub fn normalize(self, raw: &str, taken: &mut HashSet<String>) -> String {
        let mut base: String = raw
            .chars()
            .map(|c| if self.allows(c) { c } else { '_' })
            .collect();
        if base.is_empty() {
            base.push_str("call");
        }
        base.truncate(self.max_len);
        if taken.insert(base.clone()) {
            return base;
        }

        let mut n = 2u32;
        loop {
            let suffix = format!("_{n}");
            let keep = self.max_len.saturating_sub(suffix.len());
            let mut candidate: String = base.chars().take(keep).collect();
            candidate.push_str(&suffix);
            if taken.insert(candidate.clone()) {
                return candidate;
            }
            n += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_to_allowed_charset() {
        let mut taken = HashSet::new();
        let id = ProtocolId::AnthropicMessages
            .tool_id_format()
            .normalize("call/abc def", &mut taken);
        assert_eq!(id, "call_abc_def");
    }

    #[test]
    fn truncates_and_avoids_collisions() {
        let mut taken = HashSet::new();
        let format = ToolIdFormat {
            max_len: 8,
            allowed_extra: "_-",
        };
        let long = "a".repeat(32);
        let first = format.normalize(&long, &mut taken);
        let second = format.normalize(&long, &mut taken);
        assert_eq!(first.len(), 8);
        assert_eq!(second.len(), 8);
        assert_ne!(first, second);
    }
}
