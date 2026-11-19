//! Cache policy and the provider-neutral vocabulary it is expressed in.
//!
//! A request carries one [`CachePolicy`]. `Auto` is the protocol's own best
//! placement, `None` removes every cache field keel would add, and `Object`
//! names explicit breakpoint positions and a TTL. Protocols that have no
//! explicit breakpoints report the dropped positions as [`CacheWarning`]s
//! instead of pretending the policy was applied.
//!
//! Positions are canonical, not wire indices: `Message(3)` is the fourth
//! message in [`Request::messages`](crate::Request::messages), and `Part`
//! addresses one canonical part. The protocol module resolves each position
//! against the blocks it actually emits.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::ProtocolId;
use crate::request::ToolSpec;

/// Anthropic Messages accepts at most four cache breakpoints per request.
pub const ANTHROPIC_MAX_BREAKPOINTS: usize = 4;

/// Prefix of every deterministic OpenAI prompt cache key.
pub const PROMPT_CACHE_KEY_VERSION: &str = "keel-v1";

/// What a request asks the provider to cache.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePolicy {
    /// Place breakpoints where they pay off, up to the protocol's cap, and
    /// send a deterministic `prompt_cache_key` on OpenAI protocols.
    #[default]
    Auto,
    /// Strip every cache field keel would add.
    None,
    /// Explicit positions and TTL. The protocol drops what it cannot express
    /// and reports each drop as a warning.
    Object(CacheObject),
}

impl CachePolicy {
    pub const fn is_auto(&self) -> bool {
        matches!(self, Self::Auto)
    }

    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    pub const fn object(&self) -> Option<&CacheObject> {
        match self {
            Self::Object(object) => Some(object),
            _ => None,
        }
    }

    /// Builds an `Object` policy from a breakpoint list.
    pub fn object_with(breakpoints: impl IntoIterator<Item = CacheBreakpoint>) -> Self {
        Self::Object(CacheObject {
            breakpoints: breakpoints.into_iter().collect(),
            ttl: CacheTtl::FiveMinutes,
        })
    }
}

/// Explicit cache controls: where to place breakpoints and how long to keep
/// the entries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheObject {
    /// Positions in request order. The order is priority order: if more than
    /// the protocol's cap resolve to distinct blocks, the first ones win.
    #[serde(default)]
    pub breakpoints: Vec<CacheBreakpoint>,
    #[serde(default)]
    pub ttl: CacheTtl,
}

impl CacheObject {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn breakpoint(mut self, breakpoint: CacheBreakpoint) -> Self {
        self.breakpoints.push(breakpoint);
        self
    }

    pub fn ttl(mut self, ttl: CacheTtl) -> Self {
        self.ttl = ttl;
        self
    }
}

/// A cache lifetime a policy can ask for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheTtl {
    /// The provider's default five-minute lifetime. Serialized as `5m`; on
    /// Anthropic the wire field is omitted because it is the API default.
    #[default]
    #[serde(rename = "5m")]
    FiveMinutes,
    /// The extended one-hour lifetime (`ttl: "1h"`).
    #[serde(rename = "1h")]
    OneHour,
}

impl CacheTtl {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }
}

impl fmt::Display for CacheTtl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One canonical position a breakpoint can mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheBreakpoint {
    /// The last system text block, including a system-role message.
    System,
    /// The tool definition at this index in [`Request::tools`](crate::Request::tools).
    /// Caching a tool caches every tool before and including it.
    Tool(usize),
    /// The last cacheable block produced by the message at this index in
    /// [`Request::messages`](crate::Request::messages).
    Message(usize),
    /// The block produced by one canonical message part. A system-role message
    /// is flattened to one system text block, so only part 0 addresses it.
    Part { message: usize, part: usize },
    /// The last cacheable block of the last message actually emitted.
    LastMessage,
}

impl fmt::Display for CacheBreakpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::System => f.write_str("system"),
            Self::Tool(index) => write!(f, "tool[{index}]"),
            Self::Message(index) => write!(f, "message[{index}]"),
            Self::Part { message, part } => write!(f, "part[{message}:{part}]"),
            Self::LastMessage => f.write_str("last message"),
        }
    }
}

/// A typed report about a policy the encoder could not apply exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheWarning {
    pub protocol: ProtocolId,
    pub kind: CacheWarningKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheWarningKind {
    /// One requested position was dropped.
    Dropped {
        position: CacheBreakpoint,
        reason: CacheDropReason,
    },
    /// More positions resolved than the protocol accepts; the tail was
    /// dropped. The retained positions are the first `cap` in request order.
    CapReached {
        cap: usize,
        dropped: Vec<CacheBreakpoint>,
    },
    /// The protocol has no way to express part of the policy at all.
    Unsupported { feature: CacheFeature },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheDropReason {
    /// The canonical position does not correspond to a block in this request.
    NoSuchPosition,
    /// The block exists but the protocol cannot attach `cache_control` to it
    /// (for example an Anthropic thinking block).
    NotCacheable,
    /// Another breakpoint already marks the same wire block.
    Duplicate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheFeature {
    ExplicitBreakpoints { requested: usize },
    Ttl(CacheTtl),
}

impl CacheWarning {
    pub fn dropped(
        protocol: ProtocolId,
        position: CacheBreakpoint,
        reason: CacheDropReason,
    ) -> Self {
        Self {
            protocol,
            kind: CacheWarningKind::Dropped { position, reason },
        }
    }

    pub fn cap_reached(protocol: ProtocolId, cap: usize, dropped: Vec<CacheBreakpoint>) -> Self {
        Self {
            protocol,
            kind: CacheWarningKind::CapReached { cap, dropped },
        }
    }

    pub fn unsupported(protocol: ProtocolId, feature: CacheFeature) -> Self {
        Self {
            protocol,
            kind: CacheWarningKind::Unsupported { feature },
        }
    }
}

impl fmt::Display for CacheDropReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchPosition => {
                f.write_str("the position does not exist in the encoded request")
            }
            Self::NotCacheable => f.write_str("the block cannot carry a cache breakpoint"),
            Self::Duplicate => f.write_str("another breakpoint already marks the same block"),
        }
    }
}

impl fmt::Display for CacheFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExplicitBreakpoints { requested } => write!(
                f,
                "{requested} explicit cache breakpoint(s); this protocol has no explicit breakpoints"
            ),
            Self::Ttl(ttl) => write!(f, "cache TTL {ttl}; this protocol does not support a TTL"),
        }
    }
}

impl fmt::Display for CacheWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            CacheWarningKind::Dropped { position, reason } => {
                write!(
                    f,
                    "{}: dropped cache breakpoint {position}: {reason}",
                    self.protocol
                )
            }
            CacheWarningKind::CapReached { cap, dropped } => {
                let positions = dropped
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "{}: dropped {} cache breakpoint(s) over the cap of {cap}: {positions}",
                    self.protocol,
                    dropped.len()
                )
            }
            CacheWarningKind::Unsupported { feature } => {
                write!(f, "{}: dropped {feature}", self.protocol)
            }
        }
    }
}

/// A deterministic `prompt_cache_key` for OpenAI Chat and Responses.
///
/// Identical prefixes produce the same key across processes and runs. The key
/// hashes the protocol, model, system prompt and every tool's name, description
/// and schema. It deliberately does not include the provider: protocol modules
/// are provider-blind (D-002/D-004), and both OpenAI protocols scope caches per
/// account anyway, so the same key on two providers cannot share an entry.
///
/// The hash is FNV-1a 64-bit, chosen so the value is stable across Rust
/// releases and needs no dependency. A collision only merges two cache-routing
/// groups; it cannot change the prompt or the response.
pub fn prompt_cache_key(
    protocol: ProtocolId,
    model: &str,
    system: Option<&str>,
    tools: &[ToolSpec],
) -> String {
    let mut hash = Fnv1a::new();
    hash.field(PROMPT_CACHE_KEY_VERSION.as_bytes());
    hash.field(protocol.as_str().as_bytes());
    hash.field(model.as_bytes());
    match system.filter(|text| !text.trim().is_empty()) {
        Some(system) => {
            hash.field(b"system");
            hash.field(system.as_bytes());
        }
        None => hash.field(b"no-system"),
    }
    hash.field(&(tools.len() as u64).to_le_bytes());
    for tool in tools {
        hash.field(tool.name.as_bytes());
        hash.field(tool.description.as_bytes());
        // `Value::to_string` is compact JSON with a deterministic key order
        // because serde_json's default map is a BTreeMap.
        hash.field(tool.input_schema.to_string().as_bytes());
    }
    format!("{PROMPT_CACHE_KEY_VERSION}-{:016x}", hash.finish())
}

struct Fnv1a(u64);

impl Fnv1a {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn field(&mut self, bytes: &[u8]) {
        self.write(&(bytes.len() as u64).to_le_bytes());
        self.write(bytes);
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn finish(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: "does a thing".to_owned(),
            input_schema: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
        }
    }

    #[test]
    fn a_prompt_cache_key_is_stable_and_bounded() {
        let first = prompt_cache_key(
            ProtocolId::OpenAiChat,
            "gpt-4.1",
            Some("be terse"),
            &[tool("read")],
        );
        let second = prompt_cache_key(
            ProtocolId::OpenAiChat,
            "gpt-4.1",
            Some("be terse"),
            &[tool("read")],
        );
        assert_eq!(first, second);
        assert!(first.starts_with("keel-v1-"));
        assert!(first.len() <= 64, "{first}");
    }

    #[test]
    fn a_prompt_cache_key_separates_inputs_that_change_the_prefix() {
        let base = prompt_cache_key(
            ProtocolId::OpenAiChat,
            "gpt-4.1",
            Some("be terse"),
            &[tool("read")],
        );
        assert_ne!(
            base,
            prompt_cache_key(
                ProtocolId::OpenAiChat,
                "gpt-4.1",
                Some("be verbose"),
                &[tool("read")],
            )
        );
        assert_ne!(
            base,
            prompt_cache_key(
                ProtocolId::OpenAiChat,
                "gpt-4.1",
                Some("be terse"),
                &[tool("write")],
            )
        );
        assert_ne!(
            base,
            prompt_cache_key(
                ProtocolId::OpenAiResponses,
                "gpt-4.1",
                Some("be terse"),
                &[tool("read")],
            )
        );
    }

    #[test]
    fn a_prompt_cache_key_ignores_blank_system_text() {
        assert_eq!(
            prompt_cache_key(ProtocolId::OpenAiChat, "gpt-4.1", None, &[]),
            prompt_cache_key(ProtocolId::OpenAiChat, "gpt-4.1", Some("  "), &[]),
        );
    }

    #[test]
    fn object_policies_serialize_with_their_positions_and_ttl() {
        let policy = CachePolicy::Object(
            CacheObject::new()
                .breakpoint(CacheBreakpoint::System)
                .breakpoint(CacheBreakpoint::Tool(1))
                .breakpoint(CacheBreakpoint::Part {
                    message: 2,
                    part: 3,
                })
                .ttl(CacheTtl::OneHour),
        );
        let text = serde_json::to_string(&policy).expect("policy serializes");
        assert!(text.contains("\"object\""));
        assert!(text.contains("\"tool\":1"));
        assert!(text.contains("\"ttl\":\"1h\""));
        let back: CachePolicy = serde_json::from_str(&text).expect("policy round-trips");
        assert_eq!(back, policy);
    }
}
