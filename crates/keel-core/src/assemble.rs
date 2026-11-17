//! Assemble canonical events into one assistant message.
//!
//! The assembler owns ordering and the authoritative end values. Protocols emit
//! deltas for the UI and one end event per block; this is the only place that
//! decides what a part finally contains.

use std::collections::BTreeMap;

use keel_llm_types::{
    Content, LlmError, LlmEvent, Message, Part, Source, StopReason, ToolCallId, Usage,
};

#[derive(Clone, Debug)]
pub struct Turn {
    pub parts: Vec<Part>,
    pub usage: Usage,
    pub stop: StopReason,
    pub error: Option<LlmError>,
    pub warnings: Vec<String>,
}

impl Turn {
    /// Attach provenance to every model-produced part.
    pub fn into_message(mut self, source: &Source) -> Message {
        for part in &mut self.parts {
            if matches!(
                &part.content,
                Content::Text { .. }
                    | Content::Reasoning { .. }
                    | Content::RedactedReasoning { .. }
                    | Content::ToolCall { .. }
            ) {
                part.source = Some(source.clone());
            }
        }

        let mut message = Message::assistant(self.parts);
        message.stop = Some(self.stop);
        message.usage = self.usage;
        message.error = self.error.map(|error| error.to_string());
        message
    }
}

enum Slot {
    Text(String),
    Reasoning {
        text: String,
        signature: Option<String>,
    },
    Redacted(String),
    Tool {
        call_id: ToolCallId,
        name: String,
        args: String,
    },
}

#[derive(Default)]
pub struct Assembler {
    slots: BTreeMap<usize, Slot>,
    usage: Usage,
    stop: Option<StopReason>,
    error: Option<LlmError>,
    warnings: Vec<String>,
}

impl Assembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, event: LlmEvent) {
        match event {
            LlmEvent::Start { .. } => {}
            LlmEvent::TextDelta { index, delta } => {
                self.text(index).push_str(&delta);
            }
            LlmEvent::TextEnd { index, text } => {
                self.slots.insert(index, Slot::Text(text));
            }
            LlmEvent::ReasoningDelta { index, delta } => {
                if let Slot::Reasoning { text, .. } = self.reasoning(index) {
                    text.push_str(&delta);
                }
            }
            LlmEvent::ReasoningEnd {
                index,
                text,
                signature,
            } => {
                self.slots
                    .insert(index, Slot::Reasoning { text, signature });
            }
            LlmEvent::RedactedReasoning { index, data } => {
                self.slots.insert(index, Slot::Redacted(data));
            }
            LlmEvent::ToolInputDelta {
                index,
                call_id,
                delta,
            } => {
                if let Slot::Tool {
                    call_id: id, args, ..
                } = self.tool(index, &call_id)
                {
                    *id = call_id;
                    args.push_str(&delta);
                }
            }
            LlmEvent::ToolInputEnd {
                index,
                call_id,
                arguments,
            } => {
                if let Slot::Tool {
                    call_id: id, args, ..
                } = self.tool(index, &call_id)
                {
                    *id = call_id;
                    *args = arguments;
                }
            }
            LlmEvent::ToolCall { index, call } => {
                if let Slot::Tool {
                    call_id,
                    name,
                    args,
                } = self.tool(index, &call.id)
                {
                    *call_id = call.id;
                    *name = call.name;
                    *args = call.arguments;
                }
            }
            LlmEvent::Usage(usage) => {
                // Protocols send cumulative usage, so assignment (not addition)
                // is correct.
                self.usage = usage;
            }
            LlmEvent::Warning(message) => self.warnings.push(message),
            LlmEvent::Done { stop } => self.stop = Some(stop),
            LlmEvent::Error(error) => self.error = Some(error),
        }
    }

    pub fn fail(&mut self, error: LlmError) {
        self.error = Some(error);
    }

    pub fn finish(self) -> Turn {
        let mut parts = Vec::new();
        let mut has_tool = false;

        for slot in self.slots.into_values() {
            match slot {
                Slot::Text(text) => {
                    if !text.is_empty() {
                        parts.push(Part::text(text));
                    }
                }
                Slot::Reasoning { text, signature } => {
                    parts.push(Part {
                        content: Content::Reasoning { text, signature },
                        source: None,
                    });
                }
                Slot::Redacted(data) => {
                    parts.push(Part {
                        content: Content::RedactedReasoning { data },
                        source: None,
                    });
                }
                Slot::Tool {
                    call_id,
                    name,
                    args,
                } => {
                    has_tool = true;
                    parts.push(Part {
                        content: Content::ToolCall {
                            id: call_id,
                            name,
                            arguments: args,
                        },
                        source: None,
                    });
                }
            }
        }

        let stop = self.stop.unwrap_or(if has_tool {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        });

        Turn {
            parts,
            usage: self.usage,
            stop,
            error: self.error,
            warnings: self.warnings,
        }
    }

    fn text(&mut self, index: usize) -> &mut String {
        if !matches!(self.slots.get(&index), Some(Slot::Text(_))) {
            self.slots.insert(index, Slot::Text(String::new()));
        }
        match self.slots.get_mut(&index) {
            Some(Slot::Text(text)) => text,
            _ => unreachable!("slot was just inserted as text"),
        }
    }

    fn reasoning(&mut self, index: usize) -> &mut Slot {
        if !matches!(self.slots.get(&index), Some(Slot::Reasoning { .. })) {
            self.slots.insert(
                index,
                Slot::Reasoning {
                    text: String::new(),
                    signature: None,
                },
            );
        }
        self.slots
            .get_mut(&index)
            .expect("reasoning slot was just inserted")
    }

    fn tool(&mut self, index: usize, call_id: &ToolCallId) -> &mut Slot {
        let replace = match self.slots.get(&index) {
            Some(Slot::Tool {
                call_id: existing, ..
            }) => existing != call_id,
            _ => true,
        };
        if replace {
            self.slots.insert(
                index,
                Slot::Tool {
                    call_id: call_id.clone(),
                    name: String::new(),
                    args: String::new(),
                },
            );
        }
        self.slots.get_mut(&index).expect("tool slot exists")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_are_overridden_by_authoritative_end_events() {
        let mut assembler = Assembler::new();
        assembler.apply(LlmEvent::TextDelta {
            index: 0,
            delta: "hel".to_owned(),
        });
        assembler.apply(LlmEvent::TextEnd {
            index: 0,
            text: "hello".to_owned(),
        });
        let turn = assembler.finish();
        assert_eq!(turn.parts.len(), 1);
        assert!(matches!(
            &turn.parts[0].content,
            Content::Text { text } if text == "hello"
        ));
    }

    #[test]
    fn tool_arguments_are_authoritative_at_the_end() {
        let mut assembler = Assembler::new();
        assembler.apply(LlmEvent::ToolInputDelta {
            index: 0,
            call_id: ToolCallId::new("call_1"),
            delta: "{\"pa".to_owned(),
        });
        assembler.apply(LlmEvent::ToolInputEnd {
            index: 0,
            call_id: ToolCallId::new("call_1"),
            arguments: "{\"path\":\"a\"}".to_owned(),
        });
        let call = assembler
            .finish()
            .parts
            .into_iter()
            .find_map(|part| match part.content {
                Content::ToolCall { arguments, .. } => Some(arguments),
                _ => None,
            })
            .unwrap();
        assert_eq!(call, "{\"path\":\"a\"}");
    }

    #[test]
    fn an_error_keeps_partial_content() {
        let mut assembler = Assembler::new();
        assembler.apply(LlmEvent::TextEnd {
            index: 0,
            text: "partial".to_owned(),
        });
        assembler.apply(LlmEvent::Error(LlmError::decode("cut off")));
        let turn = assembler.finish();
        assert_eq!(turn.parts.len(), 1);
        assert!(turn.error.is_some());
    }

    #[test]
    fn provenance_is_attached_to_model_parts_only() {
        let mut assembler = Assembler::new();
        assembler.apply(LlmEvent::TextEnd {
            index: 0,
            text: "answer".to_owned(),
        });
        assembler.apply(LlmEvent::ToolCall {
            index: 1,
            call: keel_llm_types::ToolCall {
                id: ToolCallId::new("call_1"),
                name: "read_file".to_owned(),
                arguments: "{}".to_owned(),
            },
        });
        let source = Source {
            protocol: keel_llm_types::ProtocolId::OpenAiChat,
            provider: "test".into(),
            model: "gpt-4.1".into(),
        };
        let message = assembler.finish().into_message(&source);

        assert!(message.parts.iter().all(|part| part.source.is_some()));
        assert_eq!(
            message.parts[0].source.as_ref().unwrap().model.as_str(),
            "gpt-4.1"
        );
    }
}
