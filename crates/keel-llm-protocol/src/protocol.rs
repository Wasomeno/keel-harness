//! The protocol seam.
//!
//! A protocol owns two things: lowering a canonical `Request` into a wire body,
//! and turning wire frames into canonical `LlmEvent`s. It does not own HTTP,
//! credentials, retries or catalog data.

use serde_json::Value;

use keel_llm_types::{Compat, LlmError, LlmEvent, ProtocolId, Request};

/// A wire body plus the headers this protocol requires.
#[derive(Clone, Debug)]
pub struct EncodedRequest {
    pub body: Value,
    pub headers: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

impl EncodedRequest {
    pub fn new(body: Value) -> Self {
        Self {
            body,
            headers: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn warn(mut self, message: impl Into<String>) -> Self {
        self.warnings.push(message.into());
        self
    }
}

/// Incremental decoder for one response stream.
///
/// Frames are already de-framed by the transport (SSE payloads), so a decoder
/// only understands its protocol's JSON.
pub trait StreamDecoder: Send {
    /// Consume one frame's data payload.
    fn frame(&mut self, data: &str) -> Result<Vec<LlmEvent>, LlmError>;

    /// Emit the terminal events once the wire stream ends.
    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError>;
}

pub trait Protocol: Send + Sync {
    fn id(&self) -> ProtocolId;

    fn compat(&self) -> Compat;

    fn encode(&self, request: &Request) -> Result<EncodedRequest, LlmError>;

    fn decoder(&self) -> Box<dyn StreamDecoder>;
}
