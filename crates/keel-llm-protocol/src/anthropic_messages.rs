//! Anthropic Messages.
//!
//! This module owns thinking signatures, cache breakpoints and tool use. It is
//! the fidelity path for Anthropic and for CommandCode's Claude models; the
//! OpenAI-compatible shim some providers expose is not equivalent.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use keel_llm_types::{
    CachePolicy, Compat, Content, ErrorKind, LlmError, LlmEvent, ModelId, ProtocolId, Request,
    Role, StopReason, ToolCall, ToolCallId, ToolChoice, Usage,
};

use crate::protocol::{
    EncodedRequest, Protocol, StreamDecoder, check_json_text_depth, check_value_depth, parse_frame,
};

#[derive(Debug)]
pub struct AnthropicMessages {
    compat: Compat,
}

impl AnthropicMessages {
    pub fn new(compat: Compat) -> Self {
        Self { compat }
    }
}

impl Protocol for AnthropicMessages {
    fn id(&self) -> ProtocolId {
        ProtocolId::AnthropicMessages
    }

    fn compat(&self) -> Compat {
        self.compat
    }

    fn encode(&self, request: &Request) -> Result<EncodedRequest, LlmError> {
        if request.messages.is_empty() {
            return Err(LlmError::new(
                ErrorKind::InvalidRequest,
                "anthropic-messages: request has no messages",
            ));
        }

        let mut warnings = Vec::new();
        let mut system_blocks: Vec<Value> = Vec::new();

        if let Some(system) = request.system.as_deref().filter(|s| !s.trim().is_empty()) {
            system_blocks.push(json!({"type": "text", "text": system}));
        }

        let mut messages: Vec<Value> = Vec::new();

        for (message_index, message) in request.messages.iter().enumerate() {
            match message.role {
                Role::System => {
                    let text = message.text();
                    if !text.is_empty() {
                        system_blocks.push(json!({"type": "text", "text": text}));
                    }
                }
                Role::User => {
                    // Anthropic requires tool_result blocks at the front of the
                    // user turn.
                    let mut results = Vec::new();
                    let mut content = Vec::new();
                    for part in &message.parts {
                        match &part.content {
                            Content::ToolResult {
                                call_id,
                                output,
                                is_error,
                            } => results.push(json!({
                                "type": "tool_result",
                                "tool_use_id": call_id.as_str(),
                                "content": output,
                                "is_error": *is_error
                            })),
                            Content::Text { text } => {
                                content.push(json!({"type": "text", "text": text}));
                            }
                            Content::Image { mime, data } => {
                                if self.compat.supports_images {
                                    content.push(json!({
                                        "type": "image",
                                        "source": {
                                            "type": "base64",
                                            "media_type": mime,
                                            "data": data
                                        }
                                    }));
                                } else {
                                    content.push(json!({
                                        "type": "text",
                                        "text": format!("[image omitted: {mime}]")
                                    }));
                                }
                            }
                            _ => {}
                        }
                    }

                    let mut blocks = results;
                    blocks.extend(content);
                    if !blocks.is_empty() {
                        messages.push(json!({"role": "user", "content": blocks}));
                    }
                }
                Role::Assistant => {
                    let mut blocks: Vec<Value> = Vec::new();
                    for (part_index, part) in message.parts.iter().enumerate() {
                        match &part.content {
                            Content::Text { text } => {
                                blocks.push(json!({"type": "text", "text": text}));
                            }
                            Content::Reasoning { text, signature } => {
                                if let Some(signature) = signature {
                                    blocks.push(json!({
                                        "type": "thinking",
                                        "thinking": text,
                                        "signature": signature
                                    }));
                                } else {
                                    warnings.push(
                                        "anthropic-messages: dropped an unsigned reasoning block"
                                            .to_owned(),
                                    );
                                }
                            }
                            Content::RedactedReasoning { data } => {
                                blocks.push(json!({"type": "redacted_thinking", "data": data}));
                            }
                            Content::ToolCall {
                                id,
                                name,
                                arguments,
                            } => {
                                check_json_text_depth(
                                    "anthropic-messages",
                                    arguments,
                                    &format!(
                                        "messages[{message_index}].parts[{part_index}].arguments"
                                    ),
                                )?;
                                let input: Value = match serde_json::from_str::<Value>(arguments) {
                                    Ok(input) if input.is_object() => input,
                                    Ok(input) => {
                                        warnings.push(format!(
                                            "anthropic-messages: tool {} arguments were not an object; wrapped",
                                            id
                                        ));
                                        json!({"value": input})
                                    }
                                    Err(error) => {
                                        warnings.push(format!(
                                            "anthropic-messages: tool {} arguments were invalid JSON ({error}); sent as an empty object",
                                            id
                                        ));
                                        json!({})
                                    }
                                };
                                blocks.push(json!({
                                    "type": "tool_use",
                                    "id": id.as_str(),
                                    "name": name,
                                    "input": input
                                }));
                            }
                            _ => {}
                        }
                    }

                    // Anthropic rejects assistant turns where thinking is not
                    // first.
                    blocks.sort_by_key(|block| match block.get("type").and_then(Value::as_str) {
                        Some("thinking") | Some("redacted_thinking") => 0u8,
                        Some("text") => 1,
                        _ => 2,
                    });

                    if !blocks.is_empty() {
                        messages.push(json!({"role": "assistant", "content": blocks}));
                    }
                }
            }
        }

        if messages.is_empty() {
            return Err(LlmError::new(
                ErrorKind::InvalidRequest,
                "anthropic-messages: request contains no encodable content",
            ));
        }

        let mut max_tokens = request.max_output_tokens.unwrap_or(4096);
        let mut thinking_budget = None;
        if let Some(effort) = request.reasoning {
            if self.compat.supports_reasoning {
                let budget = match effort {
                    keel_llm_types::ReasoningEffort::Minimal => 1024,
                    keel_llm_types::ReasoningEffort::Low => 2048,
                    keel_llm_types::ReasoningEffort::Medium => 8192,
                    keel_llm_types::ReasoningEffort::High => 16384,
                };
                if max_tokens <= budget {
                    max_tokens = budget + 1024;
                    warnings.push(format!(
                        "anthropic-messages: raised max_tokens to {max_tokens} to fit the thinking budget"
                    ));
                }
                thinking_budget = Some(budget);
                if request.temperature.is_some() {
                    warnings.push(
                        "anthropic-messages: temperature is ignored when thinking is enabled"
                            .to_owned(),
                    );
                }
            } else {
                warnings.push(
                    "anthropic-messages: reasoning is not supported by this model; ignored"
                        .to_owned(),
                );
            }
        }

        let mut body = serde_json::Map::new();
        body.insert("model".to_owned(), json!(request.model.as_str()));
        body.insert("stream".to_owned(), json!(true));
        body.insert("max_tokens".to_owned(), json!(max_tokens));

        if let Some(budget) = thinking_budget {
            body.insert(
                "thinking".to_owned(),
                json!({"type": "enabled", "budget_tokens": budget}),
            );
        } else if let Some(temperature) = request.temperature {
            if self.compat.supports_temperature {
                if temperature > 1.0 {
                    warnings
                        .push("anthropic-messages: temperature above 1.0 was clamped".to_owned());
                }
                body.insert("temperature".to_owned(), json!(temperature.clamp(0.0, 1.0)));
            }
        }

        if !system_blocks.is_empty() {
            if matches!(request.cache, CachePolicy::Auto) {
                if let Some(last) = system_blocks.last_mut() {
                    last["cache_control"] = json!({"type": "ephemeral"});
                }
            }
            body.insert("system".to_owned(), Value::Array(system_blocks));
        }

        if !request.tools.is_empty() {
            if self.compat.supports_tools {
                for (index, tool) in request.tools.iter().enumerate() {
                    check_value_depth(
                        "anthropic-messages",
                        &tool.input_schema,
                        &format!("tools[{index}].input_schema"),
                    )?;
                }
                body.insert(
                    "tools".to_owned(),
                    Value::Array(
                        request
                            .tools
                            .iter()
                            .map(|tool| {
                                json!({
                                    "name": tool.name,
                                    "description": tool.description,
                                    "input_schema": tool.input_schema
                                })
                            })
                            .collect(),
                    ),
                );
                let choice = match &request.tool_choice {
                    ToolChoice::Auto => Some(json!({"type": "auto"})),
                    ToolChoice::None => Some(json!({"type": "none"})),
                    ToolChoice::Required => Some(json!({"type": "any"})),
                    ToolChoice::Named(name) => Some(json!({"type": "tool", "name": name})),
                };
                if let Some(choice) = choice {
                    body.insert("tool_choice".to_owned(), choice);
                }
            } else {
                warnings.push("anthropic-messages: model does not support tools".to_owned());
            }
        }

        // One explicit cache breakpoint on the newest turn, on top of the
        // system breakpoint. Anthropic allows four; we use at most two.
        if matches!(request.cache, CachePolicy::Auto) {
            if let Some(last_message) = messages.last_mut() {
                if let Some(blocks) = last_message
                    .get_mut("content")
                    .and_then(Value::as_array_mut)
                {
                    if let Some(block) = blocks.last_mut() {
                        block["cache_control"] = json!({"type": "ephemeral"});
                    }
                }
            }
        }

        body.insert("messages".to_owned(), Value::Array(messages));

        Ok(EncodedRequest {
            body: Value::Object(body),
            headers: vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("accept".to_owned(), "text/event-stream".to_owned()),
                ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
            ],
            warnings,
        })
    }

    fn decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(AnthropicDecoder::new())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    Redacted,
    ToolUse,
}

struct Block {
    slot: usize,
    kind: BlockKind,
    text: String,
    signature: String,
    call_id: ToolCallId,
    name: String,
    args: String,
}

struct AnthropicDecoder {
    started: bool,
    next_slot: usize,
    blocks: BTreeMap<usize, Block>,
    usage: Usage,
    stop: Option<StopReason>,
    had_tools: bool,
}

impl AnthropicDecoder {
    fn new() -> Self {
        Self {
            started: false,
            next_slot: 0,
            blocks: BTreeMap::new(),
            usage: Usage::default(),
            stop: None,
            had_tools: false,
        }
    }
}

impl StreamDecoder for AnthropicDecoder {
    fn frame(&mut self, data: &str) -> Result<Vec<LlmEvent>, LlmError> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }

        let value = parse_frame("anthropic-messages", data)?;

        let mut events = Vec::new();
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");

        match event_type {
            "message_start" => {
                if !self.started {
                    self.started = true;
                    let model = value
                        .pointer("/message/model")
                        .and_then(Value::as_str)
                        .map(ModelId::new)
                        .unwrap_or_else(|| ModelId::new("unknown"));
                    events.push(LlmEvent::Start { model });
                }
                if let Some(usage) = value.pointer("/message/usage") {
                    self.usage.input_tokens = usage
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    self.usage.cache_read_tokens = usage
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    self.usage.cache_write_tokens = usage
                        .get("cache_creation_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    events.push(LlmEvent::Usage(self.usage));
                }
            }
            "content_block_start" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let block = value.get("content_block").cloned().unwrap_or(Value::Null);
                let wire_type = block.get("type").and_then(Value::as_str).unwrap_or("text");
                let slot = self.next_slot;
                self.next_slot += 1;

                let mut state = Block {
                    slot,
                    kind: BlockKind::Text,
                    text: String::new(),
                    signature: String::new(),
                    call_id: ToolCallId::new(format!("tool_{index}")),
                    name: String::new(),
                    args: String::new(),
                };

                match wire_type {
                    "text" => {
                        state.text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                    }
                    "thinking" => {
                        state.kind = BlockKind::Thinking;
                        state.text = block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        state.signature = block
                            .get("signature")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                    }
                    "redacted_thinking" => {
                        state.kind = BlockKind::Redacted;
                        state.args = block
                            .get("data")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                    }
                    "tool_use" => {
                        state.kind = BlockKind::ToolUse;
                        self.had_tools = true;
                        state.call_id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .map(ToolCallId::new)
                            .unwrap_or_else(|| ToolCallId::new(format!("tool_{index}")));
                        state.name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        // Streaming sends an empty object here; the deltas carry
                        // the real arguments.
                        if let Some(input) = block.get("input") {
                            if input.is_object()
                                && input.as_object().map(|o| !o.is_empty()).unwrap_or(false)
                            {
                                state.args = input.to_string();
                            }
                        }
                    }
                    _ => {}
                }

                self.blocks.insert(index, state);
            }
            "content_block_delta" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = value.get("delta").cloned().unwrap_or(Value::Null);
                let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");

                if let Some(block) = self.blocks.get_mut(&index) {
                    match delta_type {
                        "text_delta" => {
                            let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                            if !text.is_empty() {
                                block.text.push_str(text);
                                events.push(LlmEvent::TextDelta {
                                    index: block.slot,
                                    delta: text.to_owned(),
                                });
                            }
                        }
                        "thinking_delta" => {
                            let text = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                            if !text.is_empty() {
                                block.text.push_str(text);
                                events.push(LlmEvent::ReasoningDelta {
                                    index: block.slot,
                                    delta: text.to_owned(),
                                });
                            }
                        }
                        "signature_delta" => {
                            if let Some(signature) = delta.get("signature").and_then(Value::as_str)
                            {
                                block.signature.push_str(signature);
                            }
                        }
                        "input_json_delta" => {
                            let partial = delta
                                .get("partial_json")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            if !partial.is_empty() {
                                block.args.push_str(partial);
                                events.push(LlmEvent::ToolInputDelta {
                                    index: block.slot,
                                    call_id: block.call_id.clone(),
                                    delta: partial.to_owned(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(block) = self.blocks.remove(&index) {
                    events.extend(close_block(block));
                }
            }
            "message_delta" => {
                if let Some(usage) = value.get("usage") {
                    if let Some(output) = usage.get("output_tokens").and_then(Value::as_u64) {
                        self.usage.output_tokens = output;
                        events.push(LlmEvent::Usage(self.usage));
                    }
                }
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = Some(map_stop(reason));
                }
            }
            "message_stop" => {}
            "error" => {
                let message = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("provider error");
                let kind = match value.pointer("/error/type").and_then(Value::as_str) {
                    Some("rate_limit_error") => ErrorKind::RateLimit,
                    Some("authentication_error") => ErrorKind::Auth,
                    Some("permission_error") => ErrorKind::Permission,
                    Some("not_found_error") => ErrorKind::NotFound,
                    Some("request_too_large") => ErrorKind::ContextOverflow,
                    Some("overloaded_error") => ErrorKind::Overloaded,
                    _ => ErrorKind::ServerError,
                };
                return Err(LlmError::new(kind, message));
            }
            "ping" => {}
            _ => {}
        }

        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        let mut events = Vec::new();
        for (_, block) in std::mem::take(&mut self.blocks) {
            events.extend(close_block(block));
        }
        events.push(LlmEvent::Usage(self.usage));
        let stop = self.stop.unwrap_or(if self.had_tools {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        });
        events.push(LlmEvent::Done { stop });
        Ok(events)
    }
}

fn close_block(block: Block) -> Vec<LlmEvent> {
    match block.kind {
        BlockKind::Text => {
            if block.text.is_empty() {
                Vec::new()
            } else {
                vec![LlmEvent::TextEnd {
                    index: block.slot,
                    text: block.text,
                }]
            }
        }
        BlockKind::Thinking => vec![LlmEvent::ReasoningEnd {
            index: block.slot,
            text: block.text,
            signature: if block.signature.is_empty() {
                None
            } else {
                Some(block.signature)
            },
        }],
        BlockKind::Redacted => vec![LlmEvent::RedactedReasoning {
            index: block.slot,
            data: block.args,
        }],
        BlockKind::ToolUse => vec![
            LlmEvent::ToolInputEnd {
                index: block.slot,
                call_id: block.call_id.clone(),
                arguments: block.args.clone(),
            },
            LlmEvent::ToolCall {
                index: block.slot,
                call: ToolCall {
                    id: block.call_id,
                    name: block.name,
                    arguments: block.args,
                },
            },
        ],
    }
}

fn map_stop(reason: &str) -> StopReason {
    match reason {
        "max_tokens" => StopReason::MaxTokens,
        "tool_use" => StopReason::ToolUse,
        "stop_sequence" => StopReason::StopSequence,
        "refusal" => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_llm_types::{Message, Part, ProviderId, Source, ToolSpec};

    use crate::protocol::MAX_JSON_DEPTH;

    /// Far past any sensible limit, and past serde_json's own parser limit.
    const DEEP: usize = 10_000;

    fn anthropic() -> AnthropicMessages {
        AnthropicMessages::new(Compat::for_protocol(ProtocolId::AnthropicMessages))
    }

    fn request_with(messages: Vec<Message>) -> Request {
        Request::new("test-model").messages(messages)
    }

    fn nested_value(depth: usize) -> Value {
        let mut value = Value::Null;
        for _ in 0..depth {
            let mut object = serde_json::Map::new();
            object.insert("a".to_owned(), value);
            value = Value::Object(object);
        }
        value
    }

    fn nested_json(depth: usize) -> String {
        let mut out = String::with_capacity(depth * 6 + 4);
        out.push_str(&"{\"a\":".repeat(depth));
        out.push_str("null");
        out.push_str(&"}".repeat(depth));
        out
    }

    fn source() -> Source {
        Source {
            protocol: ProtocolId::AnthropicMessages,
            provider: ProviderId::from("test"),
            model: ModelId::new("test-model"),
        }
    }

    #[test]
    fn encode_rejects_deeply_nested_tool_schema() {
        let mut request = request_with(vec![Message::user("hi")]);
        request.tools = vec![ToolSpec {
            name: "deep".to_owned(),
            description: String::new(),
            input_schema: nested_value(DEEP),
        }];

        let error = anthropic()
            .encode(&request)
            .expect_err("a tool schema nested deeper than the limit must be rejected");
        // Dropping a `Value` this deep is itself recursive; leak it so the
        // test process does not overflow while cleaning up.
        std::mem::forget(request);
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert!(
            error.message.contains("tools[0].input_schema"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains(&MAX_JSON_DEPTH.to_string()),
            "{}",
            error.message
        );
    }

    #[test]
    fn encode_rejects_deeply_nested_tool_arguments() {
        let message = Message::new(Role::Assistant).with_part(Part::tool_call(
            ToolCallId::new("call_1"),
            "deep",
            nested_json(DEEP),
            source(),
        ));

        let error = anthropic()
            .encode(&request_with(vec![message]))
            .expect_err("tool arguments nested deeper than the limit must be rejected");
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert!(
            error.message.contains("messages[0].parts[0].arguments"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains(&MAX_JSON_DEPTH.to_string()),
            "{}",
            error.message
        );
    }

    #[test]
    fn encode_accepts_nesting_at_the_limit() {
        let mut request = request_with(vec![Message::user("hi")]);
        request.tools = vec![ToolSpec {
            name: "deep".to_owned(),
            description: String::new(),
            input_schema: nested_value(MAX_JSON_DEPTH),
        }];

        let encoded = anthropic()
            .encode(&request)
            .expect("nesting at the limit is allowed");
        assert!(encoded.body["tools"][0]["input_schema"].is_object());
    }

    #[test]
    fn decode_rejects_deeply_nested_frame() {
        let frame = format!(r#"{{"type":"ping","data":{}}}"#, nested_json(DEEP));

        let error = anthropic()
            .decoder()
            .frame(&frame)
            .expect_err("a frame nested deeper than the limit must be rejected");
        assert_eq!(error.kind, ErrorKind::Decode);
        assert!(
            error.message.contains(&MAX_JSON_DEPTH.to_string()),
            "{}",
            error.message
        );
        assert!(error.message.contains("nesting depth"), "{}", error.message);
    }

    #[test]
    fn decode_accepts_nesting_at_the_limit() {
        // The frame's own object consumes one level of the budget.
        let frame = format!(
            r#"{{"type":"message_start","data":{},"message":{{"model":"test-model"}}}}"#,
            nested_json(MAX_JSON_DEPTH - 1)
        );

        let events = anthropic()
            .decoder()
            .frame(&frame)
            .expect("nesting at the limit is allowed");
        assert!(matches!(events.first(), Some(LlmEvent::Start { .. })));
    }
}
