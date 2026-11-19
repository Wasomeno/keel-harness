//! OpenAI Responses.
//!
//! Responses is event-sourced: every SSE frame is a typed event about an item in
//! the output list, and `response.completed` is the terminal one. There is no
//! `[DONE]` marker. Reasoning arrives as an opaque `encrypted_content` blob,
//! which is what makes a `store: false` transcript replayable.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use keel_llm_types::{
    Compat, Content, ErrorKind, LlmError, LlmEvent, ModelId, ProtocolId, Request, Role, StopReason,
    ToolCall, ToolCallId, ToolChoice, Usage,
};

use crate::protocol::{
    EncodedRequest, Protocol, StreamDecoder, check_json_text_depth, check_value_depth, parse_frame,
};

#[derive(Debug)]
pub struct OpenAiResponses {
    compat: Compat,
}

impl OpenAiResponses {
    pub fn new(compat: Compat) -> Self {
        Self { compat }
    }
}

impl Protocol for OpenAiResponses {
    fn id(&self) -> ProtocolId {
        ProtocolId::OpenAiResponses
    }

    fn compat(&self) -> Compat {
        self.compat
    }

    fn encode(&self, request: &Request) -> Result<EncodedRequest, LlmError> {
        if request.messages.is_empty() {
            return Err(invalid("openai-responses: request has no messages"));
        }

        let mut warnings = Vec::new();
        let mut instructions: Vec<String> = Vec::new();
        if let Some(system) = request.system.as_deref().filter(|s| !s.trim().is_empty()) {
            instructions.push(system.to_owned());
        }

        let mut input: Vec<Value> = Vec::new();

        for (message_index, message) in request.messages.iter().enumerate() {
            match message.role {
                Role::System => {
                    let text = message.text();
                    if !text.is_empty() {
                        instructions.push(text);
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
                                let output = if *is_error {
                                    format!("ERROR: {output}")
                                } else {
                                    output.clone()
                                };
                                input.push(json!({
                                    "type": "function_call_output",
                                    "call_id": call_id.as_str(),
                                    "output": output
                                }));
                            }
                            Content::Text { text } => {
                                blocks.push(json!({"type": "input_text", "text": text}));
                            }
                            Content::Image { mime, data } => {
                                if self.compat.supports_images {
                                    blocks.push(json!({
                                        "type": "input_image",
                                        "image_url": format!("data:{mime};base64,{data}")
                                    }));
                                } else {
                                    blocks.push(json!({
                                        "type": "input_text",
                                        "text": format!("[image omitted: {mime}]")
                                    }));
                                }
                            }
                            _ => {}
                        }
                    }
                    if !blocks.is_empty() {
                        input.push(json!({"role": "user", "content": blocks}));
                    }
                }
                Role::Assistant => {
                    let mut content: Vec<Value> = Vec::new();
                    for (part_index, part) in message.parts.iter().enumerate() {
                        match &part.content {
                            Content::Reasoning { text: _, signature } => {
                                flush_assistant(&mut input, &mut content);
                                match signature {
                                    Some(signature) => {
                                        input.push(reasoning_item(signature));
                                    }
                                    None => warnings.push(
                                        "openai-responses: dropped reasoning without encrypted content"
                                            .to_owned(),
                                    ),
                                }
                            }
                            Content::RedactedReasoning { data } => {
                                flush_assistant(&mut input, &mut content);
                                input.push(reasoning_item(data));
                            }
                            Content::Text { text } => {
                                content.push(json!({"type": "output_text", "text": text}));
                            }
                            Content::ToolCall {
                                id,
                                name,
                                arguments,
                            } => {
                                flush_assistant(&mut input, &mut content);
                                check_json_text_depth(
                                    "openai-responses",
                                    arguments,
                                    &format!(
                                        "messages[{message_index}].parts[{part_index}].arguments"
                                    ),
                                )?;
                                if serde_json::from_str::<Value>(arguments).is_err() {
                                    warnings.push(format!(
                                        "openai-responses: tool {} arguments were not valid JSON",
                                        id
                                    ));
                                }
                                input.push(json!({
                                    "type": "function_call",
                                    "call_id": id.as_str(),
                                    "name": name,
                                    "arguments": arguments
                                }));
                            }
                            Content::Image { .. } | Content::ToolResult { .. } => {}
                        }
                    }
                    flush_assistant(&mut input, &mut content);
                }
            }
        }

        if input.is_empty() {
            return Err(invalid(
                "openai-responses: request contains no encodable content",
            ));
        }

        let mut body = Map::new();
        body.insert("model".to_owned(), json!(request.model.as_str()));
        body.insert("input".to_owned(), Value::Array(input));
        body.insert("stream".to_owned(), json!(true));
        // keel keeps no server-side conversation state; every request carries
        // its own transcript.
        body.insert("store".to_owned(), json!(false));

        if !instructions.is_empty() {
            body.insert("instructions".to_owned(), json!(instructions.join("\n\n")));
        }

        if let Some(max) = request.max_output_tokens {
            body.insert("max_output_tokens".to_owned(), json!(max));
        }

        if let Some(temperature) = request.temperature {
            if self.compat.supports_temperature {
                body.insert("temperature".to_owned(), json!(temperature));
            } else {
                warnings.push(
                    "openai-responses: temperature is not supported by this model; ignored"
                        .to_owned(),
                );
            }
        }

        if let Some(effort) = request.reasoning {
            if self.compat.supports_reasoning {
                body.insert(
                    "reasoning".to_owned(),
                    json!({"effort": effort.as_str(), "summary": "auto"}),
                );
                // Without this the reasoning items cannot be replayed once
                // `store` is false.
                body.insert("include".to_owned(), json!(["reasoning.encrypted_content"]));
            } else {
                warnings.push(
                    "openai-responses: reasoning effort is not supported by this model; ignored"
                        .to_owned(),
                );
            }
        }

        if !request.tools.is_empty() {
            if self.compat.supports_tools {
                for (index, tool) in request.tools.iter().enumerate() {
                    check_value_depth(
                        "openai-responses",
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
                                    "type": "function",
                                    "name": tool.name,
                                    "description": tool.description,
                                    "parameters": tool.input_schema,
                                    "strict": false
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
                            "openai-responses: forced tool choice is not supported; using auto"
                                .to_owned(),
                        );
                        Some(json!("auto"))
                    }
                    ToolChoice::Named(name) => Some(json!({"type": "function", "name": name})),
                };
                if let Some(choice) = choice {
                    body.insert("tool_choice".to_owned(), choice);
                }
            } else {
                warnings.push("openai-responses: model does not support tools".to_owned());
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
        Box::new(OpenAiResponsesDecoder::new())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotKind {
    Message,
    Reasoning,
    FunctionCall,
}

#[derive(Clone, Debug)]
struct Slot {
    item_id: String,
    index: usize,
    kind: SlotKind,
    text: String,
    call_id: Option<ToolCallId>,
    name: String,
    args: String,
    text_done: bool,
    reasoning_done: bool,
    tool_done: bool,
}

impl Slot {
    fn new(item_id: &str, index: usize) -> Self {
        Self {
            item_id: item_id.to_owned(),
            index,
            kind: SlotKind::Message,
            text: String::new(),
            call_id: None,
            name: String::new(),
            args: String::new(),
            text_done: false,
            reasoning_done: false,
            tool_done: false,
        }
    }
}

struct OpenAiResponsesDecoder {
    slots: BTreeMap<String, Slot>,
    next_slot: usize,
    started: bool,
    usage: Usage,
    saw_usage: bool,
    any_tool: bool,
    done: bool,
    warned_signature: bool,
}

impl OpenAiResponsesDecoder {
    fn new() -> Self {
        Self {
            slots: BTreeMap::new(),
            next_slot: 0,
            started: false,
            usage: Usage::default(),
            saw_usage: false,
            any_tool: false,
            done: false,
            warned_signature: false,
        }
    }

    fn ensure(&mut self, item_id: &str) -> usize {
        if let Some(slot) = self.slots.get(item_id) {
            return slot.index;
        }
        let index = self.next_slot;
        self.next_slot += 1;
        self.slots
            .insert(item_id.to_owned(), Slot::new(item_id, index));
        index
    }

    fn slot(&mut self, item_id: &str) -> &mut Slot {
        self.ensure(item_id);
        self.slots.get_mut(item_id).expect("slot was just inserted")
    }

    fn begin_item(&mut self, item: &Value) {
        let item_id = item.get("id").and_then(Value::as_str).unwrap_or("item");
        let kind = match item.get("type").and_then(Value::as_str) {
            Some("function_call") => SlotKind::FunctionCall,
            Some("reasoning") => SlotKind::Reasoning,
            _ => SlotKind::Message,
        };
        let slot = self.slot(item_id);
        slot.kind = kind;
        if kind == SlotKind::FunctionCall {
            if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                slot.call_id = Some(ToolCallId::new(call_id));
            }
            if let Some(name) = item.get("name").and_then(Value::as_str) {
                slot.name = name.to_owned();
            }
        }
    }

    fn finish_item(&mut self, item: &Value, events: &mut Vec<LlmEvent>) {
        let item_id = item.get("id").and_then(Value::as_str).unwrap_or("item");
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => self.finish_tool(item_id, item, events),
            Some("reasoning") => self.finish_reasoning(item_id, item, events),
            Some("message") => self.finish_text(item_id, item, events),
            _ => {}
        }
    }

    fn finish_text(&mut self, item_id: &str, item: &Value, events: &mut Vec<LlmEvent>) {
        let (index, done) = {
            let slot = self.slot(item_id);
            if !slot.text.is_empty() {
                // The done event's content is authoritative when present.
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    let joined: String = content
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect();
                    if !joined.is_empty() {
                        slot.text = joined;
                    }
                }
            }
            (slot.index, slot.text_done)
        };
        if done {
            return;
        }
        let text = {
            let slot = self.slots.get_mut(item_id).expect("slot exists");
            slot.text_done = true;
            slot.text.clone()
        };
        if !text.is_empty() {
            events.push(LlmEvent::TextEnd { index, text });
        }
    }

    fn finish_reasoning(&mut self, item_id: &str, item: &Value, events: &mut Vec<LlmEvent>) {
        let (index, text, encrypted, done) = {
            let slot = self.slot(item_id);
            if slot.text.is_empty() {
                if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                    slot.text = summary
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect();
                }
            }
            (
                slot.index,
                slot.text.clone(),
                item.get("encrypted_content")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                slot.reasoning_done,
            )
        };
        if done {
            return;
        }
        if let Some(data) = encrypted {
            self.slots
                .get_mut(item_id)
                .expect("slot exists")
                .reasoning_done = true;
            // The signature carries the item id as well, so a replay can send
            // the reasoning item back exactly as it arrived.
            let signature = json!({"id": item_id, "encrypted_content": data}).to_string();
            events.push(LlmEvent::ReasoningEnd {
                index,
                text,
                signature: Some(signature),
            });
        } else if !text.is_empty() {
            self.slots
                .get_mut(item_id)
                .expect("slot exists")
                .reasoning_done = true;
            events.push(LlmEvent::ReasoningEnd {
                index,
                text,
                signature: None,
            });
            // The turn keeps its reasoning text but has nothing to replay. That
            // is a property of the gateway, not an error, and one warning per
            // stream is enough to make it visible.
            if !self.warned_signature {
                self.warned_signature = true;
                events.push(LlmEvent::Warning(
                    "openai-responses: a reasoning item arrived without encrypted_content, \
                     so this turn cannot be replayed statelessly; the reasoning text is kept \
                     but carries no signature"
                        .to_owned(),
                ));
            }
        }
    }

    fn finish_tool(&mut self, item_id: &str, item: &Value, events: &mut Vec<LlmEvent>) {
        let (index, call_id, name, arguments, done) = {
            let slot = self.slot(item_id);
            if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                slot.args = arguments.to_owned();
            }
            if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                slot.call_id = Some(ToolCallId::new(call_id));
            }
            if let Some(name) = item.get("name").and_then(Value::as_str) {
                slot.name = name.to_owned();
            }
            (
                slot.index,
                slot.call_id.clone(),
                slot.name.clone(),
                slot.args.clone(),
                slot.tool_done,
            )
        };
        if done {
            return;
        }
        self.slots.get_mut(item_id).expect("slot exists").tool_done = true;
        self.any_tool = true;

        let id = call_id.unwrap_or_else(|| ToolCallId::new(item_id));
        events.push(LlmEvent::ToolInputEnd {
            index,
            call_id: id.clone(),
            arguments: arguments.clone(),
        });
        events.push(LlmEvent::ToolCall {
            index,
            call: ToolCall {
                id,
                name,
                arguments,
            },
        });
    }

    fn completed(&mut self, value: &Value, events: &mut Vec<LlmEvent>) {
        if let Some(usage) = value.pointer("/response/usage") {
            self.usage = parse_usage(usage);
            self.saw_usage = true;
            events.push(LlmEvent::Usage(self.usage));
        }
        let reason = value
            .pointer("/response/incomplete_details/reason")
            .and_then(Value::as_str);
        let stop = if self.any_tool {
            StopReason::ToolUse
        } else if reason == Some("max_output_tokens") {
            StopReason::MaxTokens
        } else {
            StopReason::EndTurn
        };
        self.done = true;
        events.push(LlmEvent::Done { stop });
    }
}

impl StreamDecoder for OpenAiResponsesDecoder {
    fn frame(&mut self, data: &str) -> Result<Vec<LlmEvent>, LlmError> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }

        let value = parse_frame("openai-responses", data)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let mut events = Vec::new();

        if !self.started {
            self.started = true;
            let model = value
                .pointer("/response/model")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            events.push(LlmEvent::Start {
                model: ModelId::new(model),
            });
        }

        match kind {
            "response.created" | "response.in_progress" => {}
            "response.output_item.added" => {
                if let Some(item) = value.get("item") {
                    self.begin_item(item);
                }
            }
            "response.output_item.done" => {
                if let Some(item) = value.get("item") {
                    self.finish_item(item, &mut events);
                }
            }
            "response.output_text.delta" => {
                let item_id = value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or("text");
                let delta = value
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !delta.is_empty() {
                    let index = {
                        let slot = self.slot(item_id);
                        slot.text.push_str(delta);
                        slot.index
                    };
                    events.push(LlmEvent::TextDelta {
                        index,
                        delta: delta.to_owned(),
                    });
                }
            }
            "response.output_text.done" => {
                let item_id = value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or("text");
                let text = value
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let (index, done) = {
                    let slot = self.slot(item_id);
                    (slot.index, slot.text_done)
                };
                if !done {
                    let slot = self.slots.get_mut(item_id).expect("slot was just inserted");
                    slot.text = text.to_owned();
                    slot.text_done = true;
                    events.push(LlmEvent::TextEnd {
                        index,
                        text: text.to_owned(),
                    });
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let item_id = value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or("reasoning");
                let delta = value
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !delta.is_empty() {
                    let index = {
                        let slot = self.slot(item_id);
                        slot.kind = SlotKind::Reasoning;
                        slot.text.push_str(delta);
                        slot.index
                    };
                    events.push(LlmEvent::ReasoningDelta {
                        index,
                        delta: delta.to_owned(),
                    });
                }
            }
            "response.function_call_arguments.delta" => {
                let item_id = value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or("call");
                let delta = value
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !delta.is_empty() {
                    let (index, call_id) = {
                        let slot = self.slot(item_id);
                        slot.kind = SlotKind::FunctionCall;
                        slot.args.push_str(delta);
                        (
                            slot.index,
                            slot.call_id
                                .clone()
                                .unwrap_or_else(|| ToolCallId::new(item_id)),
                        )
                    };
                    events.push(LlmEvent::ToolInputDelta {
                        index,
                        call_id,
                        delta: delta.to_owned(),
                    });
                }
            }
            "response.function_call_arguments.done" => {
                let item_id = value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or("call");
                if let Some(arguments) = value.get("arguments").and_then(Value::as_str) {
                    // The end event is authoritative, but `output_item.done`
                    // carries the call id and name, so the single `ToolInputEnd`
                    // and `ToolCall` are emitted there.
                    self.slot(item_id).args = arguments.to_owned();
                }
            }
            "response.completed" | "response.incomplete" => self.completed(&value, &mut events),
            "response.failed" => {
                let message = value
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("openai-responses: response failed");
                let mut error = LlmError::new(ErrorKind::ServerError, message);
                if let Some(code) = value
                    .pointer("/response/error/code")
                    .and_then(Value::as_str)
                {
                    error = error.with_provider_code(code);
                }
                return Err(error);
            }
            "error" => {
                let message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("openai-responses: stream error");
                let mut error = LlmError::new(ErrorKind::ServerError, message);
                if let Some(code) = value.get("code").and_then(Value::as_str) {
                    error = error.with_provider_code(code);
                }
                return Err(error);
            }
            "response.cancelled" => {
                self.done = true;
                events.push(LlmEvent::Done {
                    stop: StopReason::Aborted,
                });
            }
            _ => {}
        }

        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        if self.done {
            return Ok(Vec::new());
        }

        let mut events = Vec::new();
        let mut slots: Vec<Slot> = self.slots.values().cloned().collect();
        slots.sort_by_key(|slot| slot.index);

        for slot in slots {
            match slot.kind {
                SlotKind::Message if !slot.text_done && !slot.text.is_empty() => {
                    events.push(LlmEvent::TextEnd {
                        index: slot.index,
                        text: slot.text,
                    });
                }
                SlotKind::Reasoning if !slot.reasoning_done && !slot.text.is_empty() => {
                    events.push(LlmEvent::ReasoningEnd {
                        index: slot.index,
                        text: slot.text,
                        signature: None,
                    });
                }
                SlotKind::FunctionCall if !slot.tool_done => {
                    let id = slot
                        .call_id
                        .clone()
                        .unwrap_or_else(|| ToolCallId::new(&slot.item_id));
                    events.push(LlmEvent::ToolInputEnd {
                        index: slot.index,
                        call_id: id.clone(),
                        arguments: slot.args.clone(),
                    });
                    events.push(LlmEvent::ToolCall {
                        index: slot.index,
                        call: ToolCall {
                            id,
                            name: slot.name,
                            arguments: slot.args,
                        },
                    });
                    self.any_tool = true;
                }
                _ => {}
            }
        }

        if !self.saw_usage {
            events.push(LlmEvent::Usage(self.usage));
        }
        events.push(LlmEvent::Warning(
            "openai-responses: stream ended without response.completed".to_owned(),
        ));
        events.push(LlmEvent::Done {
            stop: if self.any_tool {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
        });
        self.done = true;
        Ok(events)
    }
}

/// Rebuilds a reasoning input item from a stored signature.
///
/// Responses hands back an opaque `encrypted_content` blob plus an item id.
/// Both are kept in the canonical `signature` as JSON so a `store: false`
/// transcript can be replayed without server-side state.
fn reasoning_item(signature: &str) -> Value {
    if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(signature) {
        if let Some(encrypted) = object.get("encrypted_content").and_then(Value::as_str) {
            let mut item = json!({
                "type": "reasoning",
                "encrypted_content": encrypted
            });
            if let Some(id) = object.get("id").and_then(Value::as_str) {
                item["id"] = json!(id);
            }
            return item;
        }
    }
    json!({"type": "reasoning", "encrypted_content": signature})
}

fn parse_usage(value: &Value) -> Usage {
    Usage {
        input_tokens: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: value
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write_tokens: value
            .pointer("/input_tokens_details/cache_write_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning_tokens: value
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

fn invalid(message: impl Into<String>) -> LlmError {
    LlmError::new(ErrorKind::InvalidRequest, message)
}

fn flush_assistant(input: &mut Vec<Value>, blocks: &mut Vec<Value>) {
    if !blocks.is_empty() {
        input.push(json!({
            "role": "assistant",
            "content": Value::Array(std::mem::take(blocks))
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_llm_types::{Message, Part, ProviderId, Source, ToolSpec};

    use crate::protocol::MAX_JSON_DEPTH;

    /// Far past any sensible limit, and past serde_json's own parser limit.
    const DEEP: usize = 10_000;

    fn responses() -> OpenAiResponses {
        OpenAiResponses::new(Compat::for_protocol(ProtocolId::OpenAiResponses))
    }

    fn request_with(messages: Vec<Message>) -> Request {
        Request::new("test-model").messages(messages)
    }

    fn nested_value(depth: usize) -> Value {
        let mut value = Value::Null;
        for _ in 0..depth {
            let mut object = Map::new();
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
            protocol: ProtocolId::OpenAiResponses,
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

        let error = responses()
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

        let error = responses()
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

        let encoded = responses()
            .encode(&request)
            .expect("nesting at the limit is allowed");
        assert!(encoded.body["tools"][0]["parameters"].is_object());
    }

    #[test]
    fn decode_rejects_deeply_nested_frame() {
        let frame = format!(
            r#"{{"type":"response.created","data":{}}}"#,
            nested_json(DEEP)
        );

        let error = responses()
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
            r#"{{"type":"response.created","data":{}}}"#,
            nested_json(MAX_JSON_DEPTH - 1)
        );

        let events = responses()
            .decoder()
            .frame(&frame)
            .expect("nesting at the limit is allowed");
        assert!(matches!(events.first(), Some(LlmEvent::Start { .. })));
    }

    #[test]
    fn a_reasoning_item_without_a_blob_warns_once() {
        let mut decoder = responses().decoder();
        let done = |id: &str| {
            format!(
                r#"{{"type":"response.output_item.done","item":{{"type":"reasoning","id":"{id}","summary":[{{"type":"summary_text","text":"thinking"}}]}}}}"#
            )
        };

        let mut events = Vec::new();
        for id in ["rs_1", "rs_2"] {
            events.extend(decoder.frame(&done(id)).expect("the frame decodes"));
        }

        let warnings: Vec<&String> = events
            .iter()
            .filter_map(|event| match event {
                LlmEvent::Warning(message) => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(warnings.len(), 1, "{events:?}");
        assert!(
            warnings[0].contains("cannot be replayed statelessly"),
            "{}",
            warnings[0]
        );

        let signatures: Vec<Option<String>> = events
            .iter()
            .filter_map(|event| match event {
                LlmEvent::ReasoningEnd { signature, .. } => Some(signature.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(signatures.len(), 2, "{events:?}");
        assert!(signatures.iter().all(Option::is_none));
    }
}
