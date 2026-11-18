//! The protocol seam.
//!
//! A protocol owns two things: lowering a canonical `Request` into a wire body,
//! and turning wire frames into canonical `LlmEvent`s. It does not own HTTP,
//! credentials, retries or catalog data.

use serde_json::Value;

use keel_llm_types::{Compat, ErrorKind, LlmError, LlmEvent, ProtocolId, Request};

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

/// Maximum JSON nesting depth accepted in a request or a wire frame.
///
/// Real tool schemas are rarely deeper than ten containers; 64 leaves room for
/// elaborate `anyOf` trees while staying below serde_json's own 128-container
/// parser limit, so this guard always produces a typed error first. It also
/// keeps recursive `Value` walks (clone, drop, serialization) far inside an
/// 8 MiB stack. The limit applies to each caller-supplied value (tool schema,
/// tool-call argument JSON) and to a whole SSE frame.
pub(crate) const MAX_JSON_DEPTH: usize = 64;

/// Parses a frame after rejecting nesting our own walkers could not survive.
///
/// The scan is iterative because the input may be deeper than the stack can
/// hold; by the time serde_json sees the text, its depth is already bounded.
pub(crate) fn parse_frame(codec: &str, data: &str) -> Result<Value, LlmError> {
    let depth = raw_json_depth(data);
    if depth > MAX_JSON_DEPTH {
        return Err(LlmError::decode(format!(
            "{codec}: frame JSON nesting depth {depth} exceeds the limit of {MAX_JSON_DEPTH}"
        )));
    }
    serde_json::from_str(data).map_err(|error| {
        // serde_json reports its own recursion limit as an ordinary syntax
        // error. The pre-scan fires first at our lower limit; this mapping is
        // a backstop in case that parser limit ever changes.
        if error.to_string().contains("recursion limit exceeded") {
            LlmError::decode(format!(
                "{codec}: frame JSON exceeds the maximum nesting depth of {MAX_JSON_DEPTH}"
            ))
        } else {
            LlmError::decode(format!("{codec}: invalid frame: {error}"))
        }
    })
}

/// Counts the deepest `{`/`[` nesting outside JSON strings.
fn raw_json_depth(text: &str) -> usize {
    let mut depth = 0usize;
    let mut max = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in text.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                max = max.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

/// Rejects a caller-supplied `Value` that nests too deeply to walk safely.
pub(crate) fn check_value_depth(codec: &str, value: &Value, path: &str) -> Result<(), LlmError> {
    let mut stack = vec![(value, 0usize)];
    while let Some((value, depth)) = stack.pop() {
        match value {
            Value::Array(items) => {
                let depth = depth + 1;
                if depth > MAX_JSON_DEPTH {
                    return Err(depth_error(codec, path));
                }
                stack.extend(items.iter().map(|item| (item, depth)));
            }
            Value::Object(object) => {
                let depth = depth + 1;
                if depth > MAX_JSON_DEPTH {
                    return Err(depth_error(codec, path));
                }
                stack.extend(object.values().map(|item| (item, depth)));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Rejects raw JSON text that nests too deeply, before serde_json parses it.
pub(crate) fn check_json_text_depth(codec: &str, text: &str, path: &str) -> Result<(), LlmError> {
    if raw_json_depth(text) > MAX_JSON_DEPTH {
        return Err(depth_error(codec, path));
    }
    Ok(())
}

fn depth_error(codec: &str, path: &str) -> LlmError {
    LlmError::new(
        ErrorKind::InvalidRequest,
        format!("{codec}: {path} exceeds the maximum JSON nesting depth of {MAX_JSON_DEPTH}"),
    )
}
