//! OpenAI Chat Completions.
//!
//! This module is the compatibility path for OpenAI, Grok, CommandCode's open
//! models, OpenCode's translated routes and any OpenAI-compatible endpoint.
//! Vendor differences belong in `Compat`, not in a copy of this file.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use keel_llm_types::{
    Compat, Content, ErrorKind, LlmError, LlmEvent, MaxTokensField, ModelId, ProtocolId, Request,
    Role, StopReason, ToolCall, ToolCallId, ToolChoice, Usage,
};

use crate::protocol::{EncodedRequest, Protocol, StreamDecoder};

#[derive(Debug)]
pub struct OpenAiChat {
    compat: Compat,
}

impl OpenAiChat {
    pub fn new(compat: Compat) -> Self {
        Self { compat }
    }
}

impl Protocol for OpenAiChat {
    fn id(&self) -> ProtocolId {
        ProtocolId::OpenAiChat
    }

    fn compat(&self) -> Compat {
        self.compat
    }

    fn encode(&self, request: &Request) -> Result<EncodedRequest, LlmError> {
        if request.messages.is_empty() {
            return Err(LlmError::new(
                ErrorKind::InvalidRequest,
                "openai-chat: request has no messages",
            ));
        }

        let mut warnings = Vec::new();
        let mut messages: Vec<Value> = Vec::new();

        if let Some(system) = request.system.as_deref().filter(|s| !s.trim().is_empty()) {
            if self.compat.system_in_messages {
                messages.push(json!({"role": "system", "content": system}));
            } else {
                warnings.push(
                    "openai-chat: this model takes a top-level system field; it was sent as a message"
                        .to_owned(),
                );
                messages.push(json!({"role": "system", "content": system}));
            }
        }

        for message in &request.messages {
            match message.role {
                Role::System => {
                    let text = message.text();
                    if !text.is_empty() {
                        messages.push(json!({"role": "system", "content": text}));
                    }
                }
                Role::User => {
                    let mut blocks: Vec<Value> = Vec::new();
                    for part in &message.parts {
                        match &part.content {
                            Content::ToolResult {
                                call_id,
                                output,
                                is_error,
                            } => {
                                let content = if *is_error {
                                    format!("ERROR: {output}")
                                } else {
                                    output.clone()
                                };
                                messages.push(json!({
                                    "role": "tool",
                                    "tool_call_id": call_id.as_str(),
                                    "content": content
                                }));
                            }
                            Content::Text { text } => {
                                blocks.push(json!({"type": "text", "text": text}));
                            }
                            Content::Image { mime, data } => {
                                if self.compat.supports_images {
                                    blocks.push(json!({
                                        "type": "image_url",
                                        "image_url": {"url": format!("data:{mime};base64,{data}")}
                                    }));
                                } else {
                                    blocks.push(json!({
                                        "type": "text",
                                        "text": format!("[image omitted: {mime}]")
                                    }));
                                }
                            }
                            Content::Reasoning { .. }
                            | Content::RedactedReasoning { .. }
                            | Content::ToolCall { .. } => {}
                        }
                    }

                    if blocks.is_empty() {
                        continue;
                    }

                    // A single text block as a plain string is the most widely
                    // accepted form across OpenAI-compatible servers.
                    let content = if blocks.len() == 1
                        && blocks[0].get("type").and_then(Value::as_str) == Some("text")
                    {
                        blocks[0]
                            .get("text")
                            .cloned()
                            .unwrap_or(Value::String(String::new()))
                    } else {
                        Value::Array(blocks)
                    };
                    messages.push(json!({"role": "user", "content": content}));
                }
                Role::Assistant => {
                    let mut text = String::new();
                    let mut calls = Vec::new();
                    for part in &message.parts {
                        match &part.content {
                            Content::Text { text: value } => text.push_str(value),
                            Content::ToolCall {
                                id,
                                name,
                                arguments,
                            } => calls.push(json!({
                                "id": id.as_str(),
                                "type": "function",
                                "function": {"name": name, "arguments": arguments}
                            })),
                            _ => {}
                        }
                    }

                    if text.is_empty() && calls.is_empty() {
                        continue;
                    }

                    let mut object = serde_json::Map::new();
                    object.insert("role".to_owned(), json!("assistant"));
                    object.insert(
                        "content".to_owned(),
                        if text.is_empty() {
                            Value::Null
                        } else {
                            Value::String(text)
                        },
                    );
                    if !calls.is_empty() {
                        if self.compat.supports_tools {
                            object.insert("tool_calls".to_owned(), Value::Array(calls));
                        } else {
                            warnings.push(
                                "openai-chat: model does not support tools; tool calls dropped"
                                    .to_owned(),
                            );
                        }
                    }
                    messages.push(Value::Object(object));
                }
            }
        }

        if messages.is_empty() {
            return Err(LlmError::new(
                ErrorKind::InvalidRequest,
                "openai-chat: request contains no encodable content",
            ));
        }

        let mut body = serde_json::Map::new();
        body.insert("model".to_owned(), json!(request.model.as_str()));
        body.insert("messages".to_owned(), Value::Array(messages));
        body.insert("stream".to_owned(), json!(true));
        if self.compat.supports_stream_options {
            body.insert("stream_options".to_owned(), json!({"include_usage": true}));
        }

        if let Some(max) = request.max_output_tokens {
            let field = match self.compat.max_tokens_field {
                MaxTokensField::MaxTokens => "max_tokens",
                MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
            };
            body.insert(field.to_owned(), json!(max));
        } else if matches!(
            self.compat.max_tokens_field,
            MaxTokensField::MaxCompletionTokens
        ) {
            warnings.push(
                "openai-chat: this model requires max_completion_tokens; none was set".to_owned(),
            );
        }

        if let Some(temperature) = request.temperature {
            if self.compat.supports_temperature {
                body.insert("temperature".to_owned(), json!(temperature));
            } else {
                warnings.push(
                    "openai-chat: temperature is not supported by this model; ignored".to_owned(),
                );
            }
        }

        if let Some(effort) = request.reasoning {
            if self.compat.supports_reasoning {
                body.insert("reasoning_effort".to_owned(), json!(effort.as_str()));
            } else {
                warnings.push(
                    "openai-chat: reasoning effort is not supported by this model; ignored"
                        .to_owned(),
                );
            }
        }

        if !request.tools.is_empty() {
            if self.compat.supports_tools {
                body.insert(
                    "tools".to_owned(),
                    Value::Array(
                        request
                            .tools
                            .iter()
                            .map(|tool| {
                                json!({
                                    "type": "function",
                                    "function": {
                                        "name": tool.name,
                                        "description": tool.description,
                                        "parameters": tool.input_schema
                                    }
                                })
                            })
                            .collect(),
                    ),
                );
                let choice = match &request.tool_choice {
                    ToolChoice::Auto => Some(json!("auto")),
                    ToolChoice::None => Some(json!("none")),
                    ToolChoice::Required if self.compat.supports_tool_choice_required => {
                        Some(json!("required"))
                    }
                    ToolChoice::Required => {
                        warnings.push(
                            "openai-chat: forced tool choice is not supported; using auto"
                                .to_owned(),
                        );
                        Some(json!("auto"))
                    }
                    ToolChoice::Named(name) => Some(json!({
                        "type": "function",
                        "function": {"name": name}
                    })),
                };
                if let Some(choice) = choice {
                    body.insert("tool_choice".to_owned(), choice);
                }
            } else {
                warnings.push("openai-chat: model does not support tools".to_owned());
            }
        }

        Ok(EncodedRequest {
            body: Value::Object(body),
            headers: vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("accept".to_owned(), "text/event-stream".to_owned()),
            ],
            warnings,
        })
    }

    fn decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(OpenAiChatDecoder::new())
    }
}

struct TextBuf {
    slot: usize,
    text: String,
}

struct ToolBuf {
    slot: usize,
    id: ToolCallId,
    name: String,
    args: String,
}

struct OpenAiChatDecoder {
    started: bool,
    next_slot: usize,
    text: Option<TextBuf>,
    reasoning: Option<TextBuf>,
    tools: BTreeMap<String, ToolBuf>,
    usage: Usage,
    stop: Option<StopReason>,
    saw_stop: bool,
}

impl OpenAiChatDecoder {
    fn new() -> Self {
        Self {
            started: false,
            next_slot: 0,
            text: None,
            reasoning: None,
            tools: BTreeMap::new(),
            usage: Usage::default(),
            stop: None,
            saw_stop: false,
        }
    }

    fn text_slot(&mut self) -> usize {
        if self.text.is_none() {
            let slot = self.next_slot;
            self.next_slot += 1;
            self.text = Some(TextBuf {
                slot,
                text: String::new(),
            });
        }
        self.text.as_ref().map(|buf| buf.slot).unwrap_or(0)
    }

    fn reasoning_slot(&mut self) -> usize {
        if self.reasoning.is_none() {
            let slot = self.next_slot;
            self.next_slot += 1;
            self.reasoning = Some(TextBuf {
                slot,
                text: String::new(),
            });
        }
        self.reasoning.as_ref().map(|buf| buf.slot).unwrap_or(0)
    }
}

impl StreamDecoder for OpenAiChatDecoder {
    fn frame(&mut self, data: &str) -> Result<Vec<LlmEvent>, LlmError> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        if data == "[DONE]" {
            return Ok(Vec::new());
        }

        let value: Value = serde_json::from_str(data)
            .map_err(|error| LlmError::decode(format!("openai-chat: invalid frame: {error}")))?;

        let mut events = Vec::new();

        if !self.started {
            self.started = true;
            let model = value
                .get("model")
                .and_then(Value::as_str)
                .map(ModelId::new)
                .unwrap_or_else(|| ModelId::new("unknown"));
            events.push(LlmEvent::Start { model });
        }

        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider error");
            let mut llm_error = LlmError::new(ErrorKind::ServerError, message);
            if let Some(code) = error.get("code").and_then(|code| {
                code.as_str()
                    .map(str::to_owned)
                    .or_else(|| code.as_u64().map(|n| n.to_string()))
            }) {
                llm_error = llm_error.with_provider_code(code);
            }
            return Err(llm_error);
        }

        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            self.usage = parse_usage(usage);
            events.push(LlmEvent::Usage(self.usage));
        }

        if let Some(choices) = value.get("choices").and_then(Value::as_array) {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    if let Some(content) = delta.get("content").and_then(Value::as_str) {
                        if !content.is_empty() {
                            let slot = self.text_slot();
                            if let Some(buf) = self.text.as_mut() {
                                buf.text.push_str(content);
                            }
                            events.push(LlmEvent::TextDelta {
                                index: slot,
                                delta: content.to_owned(),
                            });
                        }
                    }

                    let reasoning = delta
                        .get("reasoning_content")
                        .or_else(|| delta.get("reasoning"))
                        .and_then(Value::as_str);
                    if let Some(reasoning) = reasoning.filter(|text| !text.is_empty()) {
                        let slot = self.reasoning_slot();
                        if let Some(buf) = self.reasoning.as_mut() {
                            buf.text.push_str(reasoning);
                        }
                        events.push(LlmEvent::ReasoningDelta {
                            index: slot,
                            delta: reasoning.to_owned(),
                        });
                    }

                    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                        for call in calls {
                            // OpenAI keys a call by `index`; some compatible
                            // servers omit it and send every delta under its
                            // own id instead.
                            let key = call
                                .get("index")
                                .and_then(Value::as_u64)
                                .map(|index| format!("index:{index}"))
                                .or_else(|| {
                                    call.get("id")
                                        .and_then(Value::as_str)
                                        .filter(|id| !id.is_empty())
                                        .map(|id| format!("id:{id}"))
                                })
                                .unwrap_or_else(|| "index:0".to_owned());
                            let slot = match self.tools.get(&key) {
                                Some(tool) => tool.slot,
                                None => {
                                    let slot = self.next_slot;
                                    self.next_slot += 1;
                                    slot
                                }
                            };
                            let tool = self.tools.entry(key).or_insert_with(|| ToolBuf {
                                slot,
                                id: ToolCallId::new(format!("call_{slot}")),
                                name: String::new(),
                                args: String::new(),
                            });

                            if let Some(id) = call.get("id").and_then(Value::as_str) {
                                if !id.is_empty() {
                                    tool.id = ToolCallId::new(id);
                                }
                            }

                            if let Some(function) = call.get("function") {
                                if let Some(name) = function.get("name").and_then(Value::as_str) {
                                    if !name.is_empty() {
                                        tool.name = name.to_owned();
                                    }
                                }
                                if let Some(arguments) =
                                    function.get("arguments").and_then(Value::as_str)
                                {
                                    if !arguments.is_empty() {
                                        tool.args.push_str(arguments);
                                        events.push(LlmEvent::ToolInputDelta {
                                            index: slot,
                                            call_id: tool.id.clone(),
                                            delta: arguments.to_owned(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }

                if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                    self.stop = Some(map_stop(reason));
                    self.saw_stop = true;
                }
            }
        }

        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        let mut events = Vec::new();

        if let Some(buf) = self.text.take() {
            if !buf.text.is_empty() {
                events.push(LlmEvent::TextEnd {
                    index: buf.slot,
                    text: buf.text,
                });
            }
        }

        if let Some(buf) = self.reasoning.take() {
            if !buf.text.is_empty() {
                events.push(LlmEvent::ReasoningEnd {
                    index: buf.slot,
                    text: buf.text,
                    signature: None,
                });
            }
        }

        let mut had_tools = false;
        let mut tools: Vec<ToolBuf> = std::mem::take(&mut self.tools).into_values().collect();
        tools.sort_by_key(|tool| tool.slot);
        for tool in tools {
            had_tools = true;
            events.push(LlmEvent::ToolInputEnd {
                index: tool.slot,
                call_id: tool.id.clone(),
                arguments: tool.args.clone(),
            });
            events.push(LlmEvent::ToolCall {
                index: tool.slot,
                call: ToolCall {
                    id: tool.id,
                    name: tool.name,
                    arguments: tool.args,
                },
            });
        }

        events.push(LlmEvent::Usage(self.usage));
        if !self.saw_stop {
            events.push(LlmEvent::Warning(
                "openai-chat: stream ended without a finish reason; treated as a completed turn"
                    .to_owned(),
            ));
        }
        let mut stop = self.stop.unwrap_or(if had_tools {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        });
        // A turn that produced tool calls is a tool-use turn even when the
        // provider labelled it `stop`.
        if had_tools && stop == StopReason::EndTurn {
            stop = StopReason::ToolUse;
        }
        events.push(LlmEvent::Done { stop });
        Ok(events)
    }
}

fn parse_usage(value: &Value) -> Usage {
    Usage {
        input_tokens: value
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: value
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write_tokens: 0,
        reasoning_tokens: value
            .get("completion_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

fn map_stop(reason: &str) -> StopReason {
    match reason {
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::ContentFilter,
        _ => StopReason::EndTurn,
    }
}
