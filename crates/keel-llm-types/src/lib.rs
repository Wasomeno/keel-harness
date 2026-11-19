//! Canonical, provider-independent types for keel's provider seam.
//!
//! Dependency rule: this crate has no HTTP, no auth, no catalog and no session
//! loop. Everything above it may depend on it; it depends on nothing of keel's.

pub mod cache;
pub mod compat;
pub mod content;
pub mod error;
pub mod event;
pub mod ids;
pub mod message;
pub mod repair;
pub mod request;
pub mod usage;

pub use cache::{
    ANTHROPIC_MAX_BREAKPOINTS, CacheBreakpoint, CacheDropReason, CacheFeature, CacheObject,
    CachePolicy, CacheTtl, CacheWarning, CacheWarningKind, prompt_cache_key,
};
pub use compat::{Compat, MaxTokensField};
pub use content::{Content, Part, Source, ToolCall};
pub use error::{ErrorKind, LlmError};
pub use event::LlmEvent;
pub use ids::{MessageId, ModelId, ProtocolId, ProviderId, ToolCallId, ToolIdFormat};
pub use message::{Message, Role, StopReason};
pub use repair::{RepairNote, RepairOutcome, ReplayTarget, repair_transcript};
pub use request::{ReasoningEffort, Request, ToolChoice, ToolSpec};
pub use usage::Usage;
