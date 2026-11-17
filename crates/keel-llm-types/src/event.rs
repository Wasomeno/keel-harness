//! The streaming event protocol.
//!
//! Events are owned values, not snapshots of a live message. A protocol decoder
//! emits deltas as they arrive and one authoritative end event per block; the
//! session assembler is the only place that builds parts.
//!
//! `Done` is required. A stream that ends without it is a decode error.

use crate::content::ToolCall;
use crate::error::LlmError;
use crate::ids::{ModelId, ToolCallId};
use crate::message::StopReason;
use crate::usage::Usage;

#[derive(Clone, Debug, PartialEq)]
pub enum LlmEvent {
    Start {
        model: ModelId,
    },
    TextDelta {
        index: usize,
        delta: String,
    },
    TextEnd {
        index: usize,
        text: String,
    },
    ReasoningDelta {
        index: usize,
        delta: String,
    },
    ReasoningEnd {
        index: usize,
        text: String,
        signature: Option<String>,
    },
    RedactedReasoning {
        index: usize,
        data: String,
    },
    /// Raw tool-argument text. Not valid JSON until `ToolInputEnd`.
    ToolInputDelta {
        index: usize,
        call_id: ToolCallId,
        delta: String,
    },
    ToolInputEnd {
        index: usize,
        call_id: ToolCallId,
        arguments: String,
    },
    /// The authoritative tool call: id, name and complete raw arguments.
    ToolCall {
        index: usize,
        call: ToolCall,
    },
    Usage(Usage),
    Warning(String),
    Done {
        stop: StopReason,
    },
    Error(LlmError),
}
