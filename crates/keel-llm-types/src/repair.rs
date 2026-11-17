//! Pure transcript repair, run before every request.
//!
//! Replay validity is a property of the whole conversation against a target
//! model, not of individual messages. The rules implemented here are:
//!
//! 1. Drop errored or aborted assistant turns.
//! 2. Drop unsigned reasoning that came from a different model.
//! 3. Normalize tool call ids into the target's charset and length.
//! 4. Synthesize an error result for a tool call that never got one.
//! 5. Drop tool results that answer no open call.
//! 6. Move system messages that landed between a call and its result.
//! 7. Downgrade images the target cannot see to a text placeholder.
//!
//! The function is pure: same input, same output, no clock, no IO.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::compat::Compat;
use crate::content::{Content, Part, Source};
use crate::ids::{MessageId, ModelId, ProtocolId, ProviderId, ToolCallId, ToolIdFormat};
use crate::message::{Message, Role};

#[derive(Clone, Debug)]
pub struct ReplayTarget {
    pub protocol: ProtocolId,
    pub provider: ProviderId,
    pub model: ModelId,
    pub supports_images: bool,
    pub tool_id: ToolIdFormat,
}

impl ReplayTarget {
    pub fn new(
        protocol: ProtocolId,
        provider: impl Into<ProviderId>,
        model: impl Into<ModelId>,
    ) -> Self {
        Self {
            protocol,
            provider: provider.into(),
            model: model.into(),
            supports_images: Compat::for_protocol(protocol).supports_images,
            tool_id: protocol.tool_id_format(),
        }
    }

    pub fn with_images(mut self, supports_images: bool) -> Self {
        self.supports_images = supports_images;
        self
    }

    pub fn source(&self) -> Source {
        Source {
            protocol: self.protocol,
            provider: self.provider.clone(),
            model: self.model.clone(),
        }
    }

    fn same_origin(&self, source: &Source) -> bool {
        source.protocol == self.protocol
            && source.provider == self.provider
            && source.model == self.model
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepairNote {
    DroppedTurn {
        message: MessageId,
        reason: String,
    },
    DroppedReasoning {
        message: MessageId,
        reason: String,
    },
    DowngradedImage {
        message: MessageId,
        mime: String,
    },
    HoistedSystemMessages {
        count: usize,
        before: MessageId,
    },
    NormalizedToolId {
        from: String,
        to: String,
    },
    SynthesizedToolResult {
        call_id: ToolCallId,
        message: MessageId,
    },
    DroppedToolResult {
        call_id: ToolCallId,
    },
}

impl fmt::Display for RepairNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DroppedTurn { message, reason } => {
                write!(f, "dropped turn {message}: {reason}")
            }
            Self::DroppedReasoning { message, reason } => {
                write!(f, "dropped reasoning in {message}: {reason}")
            }
            Self::DowngradedImage { message, mime } => {
                write!(f, "downgraded {mime} image in {message} to text")
            }
            Self::HoistedSystemMessages { count, before } => {
                write!(f, "hoisted {count} system message(s) before {before}")
            }
            Self::NormalizedToolId { from, to } => {
                write!(f, "normalized tool id {from} -> {to}")
            }
            Self::SynthesizedToolResult { call_id, message } => {
                write!(f, "synthesized result for {call_id} in {message}")
            }
            Self::DroppedToolResult { call_id } => {
                write!(f, "dropped tool result for {call_id}: no open call")
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RepairOutcome {
    pub messages: Vec<Message>,
    pub notes: Vec<RepairNote>,
}

impl RepairOutcome {
    pub fn is_clean(&self) -> bool {
        self.notes.is_empty()
    }
}

pub fn repair_transcript(messages: Vec<Message>, target: &ReplayTarget) -> RepairOutcome {
    let mut notes = Vec::new();

    // 1. Per-part hygiene and failed-turn removal.
    let mut cleaned: Vec<Message> = Vec::with_capacity(messages.len());
    for message in messages {
        if message.role == Role::Assistant && message.is_failed() {
            notes.push(RepairNote::DroppedTurn {
                message: message.id.clone(),
                reason: "errored or aborted assistant turn".to_owned(),
            });
            continue;
        }

        let mut kept = Vec::with_capacity(message.parts.len());
        for mut part in message.parts {
            let drop_reason = match &part.content {
                Content::Reasoning { signature, .. } => {
                    if portable_reasoning(part.source.as_ref(), signature.is_some(), false, target)
                    {
                        None
                    } else {
                        Some("unsigned reasoning from another model")
                    }
                }
                Content::RedactedReasoning { .. } => {
                    if portable_reasoning(part.source.as_ref(), false, true, target) {
                        None
                    } else {
                        Some("redacted reasoning from another model")
                    }
                }
                _ => None,
            };
            if let Some(reason) = drop_reason {
                notes.push(RepairNote::DroppedReasoning {
                    message: message.id.clone(),
                    reason: reason.to_owned(),
                });
                continue;
            }

            let omitted_image = match &part.content {
                Content::Image { mime, .. } if !target.supports_images => Some(mime.clone()),
                _ => None,
            };
            if let Some(mime) = omitted_image {
                notes.push(RepairNote::DowngradedImage {
                    message: message.id.clone(),
                    mime: mime.clone(),
                });
                part = Part {
                    content: Content::Text {
                        text: format!("[image omitted: {mime}]"),
                    },
                    source: part.source.clone(),
                };
            }
            kept.push(part);
        }

        let Message {
            id: message_id,
            role,
            parts: _,
            stop,
            usage,
            error,
        } = message;
        let message = Message {
            id: message_id,
            role,
            parts: kept,
            stop,
            usage,
            error,
        };
        if message.parts.is_empty() {
            notes.push(RepairNote::DroppedTurn {
                message: message.id.clone(),
                reason: "no replayable content".to_owned(),
            });
            continue;
        }
        cleaned.push(message);
    }

    // 2. Hoist system messages that landed between a tool call and its result.
    let mut hoisted: Vec<Message> = Vec::with_capacity(cleaned.len());
    let mut cursor = 0;
    while cursor < cleaned.len() {
        let message = cleaned[cursor].clone();
        if message.role == Role::Assistant && message.has_tool_calls() {
            let mut next = cursor + 1;
            let mut systems = Vec::new();
            while next < cleaned.len() && cleaned[next].role == Role::System {
                systems.push(cleaned[next].clone());
                next += 1;
            }
            if !systems.is_empty() {
                notes.push(RepairNote::HoistedSystemMessages {
                    count: systems.len(),
                    before: message.id.clone(),
                });
                hoisted.extend(systems);
            }
            hoisted.push(message);
            cursor = next;
        } else {
            hoisted.push(message);
            cursor += 1;
        }
    }

    // 3. Normalize tool ids and make every call have exactly one result.
    let mut taken: HashSet<String> = HashSet::new();
    let mut id_map: HashMap<String, String> = HashMap::new();
    let mut out: Vec<Message> = Vec::with_capacity(hoisted.len());
    let mut pending: Vec<ToolCallId> = Vec::new();

    for mut message in hoisted {
        match message.role {
            Role::Assistant => {
                if !pending.is_empty() {
                    let mut synthesized = Message::with_id(
                        MessageId::new(format!("{}_tool_results", message.id)),
                        Role::User,
                    );
                    for id in std::mem::take(&mut pending) {
                        notes.push(RepairNote::SynthesizedToolResult {
                            call_id: id.clone(),
                            message: synthesized.id.clone(),
                        });
                        synthesized
                            .parts
                            .push(Part::tool_result(id, "No result provided", true));
                    }
                    out.push(synthesized);
                }

                for part in &mut message.parts {
                    if let Content::ToolCall { id, .. } = &mut part.content {
                        let old = id.as_str().to_owned();
                        let new = match id_map.get(&old) {
                            Some(mapped) => mapped.clone(),
                            None => {
                                let mapped = target.tool_id.normalize(&old, &mut taken);
                                id_map.insert(old.clone(), mapped.clone());
                                mapped
                            }
                        };
                        if old != new {
                            notes.push(RepairNote::NormalizedToolId {
                                from: old,
                                to: new.clone(),
                            });
                        }
                        *id = ToolCallId::new(new);
                    }
                }
                pending = message.tool_call_ids();
                out.push(message);
            }
            Role::User => {
                let mut parts = Vec::with_capacity(message.parts.len());
                for mut part in message.parts {
                    if let Content::ToolResult { call_id, .. } = &mut part.content {
                        let current = call_id.clone();
                        let mapped = id_map
                            .get(current.as_str())
                            .cloned()
                            .map(ToolCallId::new)
                            .unwrap_or(current);
                        *call_id = mapped.clone();

                        if let Some(position) = pending.iter().position(|id| *id == mapped) {
                            pending.remove(position);
                            parts.push(part);
                        } else {
                            notes.push(RepairNote::DroppedToolResult { call_id: mapped });
                        }
                    } else {
                        parts.push(part);
                    }
                }

                message.parts = parts;
                if !pending.is_empty() {
                    for id in std::mem::take(&mut pending) {
                        notes.push(RepairNote::SynthesizedToolResult {
                            call_id: id.clone(),
                            message: message.id.clone(),
                        });
                        message
                            .parts
                            .push(Part::tool_result(id, "No result provided", true));
                    }
                    // Anthropic requires tool_result blocks at the front of the
                    // user turn. Stable sort keeps the rest in order.
                    message.parts.sort_by_key(|part| {
                        if matches!(&part.content, Content::ToolResult { .. }) {
                            0u8
                        } else {
                            1u8
                        }
                    });
                }

                if !message.parts.is_empty() {
                    out.push(message);
                }
            }
            Role::System => out.push(message),
        }
    }

    if !pending.is_empty() {
        let id = out
            .last()
            .map(|message| format!("{}_tool_results", message.id))
            .unwrap_or_else(|| "repair_tool_results".to_owned());
        let mut synthesized = Message::with_id(MessageId::new(id), Role::User);
        for id in std::mem::take(&mut pending) {
            notes.push(RepairNote::SynthesizedToolResult {
                call_id: id.clone(),
                message: synthesized.id.clone(),
            });
            synthesized
                .parts
                .push(Part::tool_result(id, "No result provided", true));
        }
        out.push(synthesized);
    }

    RepairOutcome {
        messages: out,
        notes,
    }
}

fn portable_reasoning(
    source: Option<&Source>,
    signed: bool,
    redacted: bool,
    target: &ReplayTarget,
) -> bool {
    match source {
        None => true,
        Some(source) if target.same_origin(source) => true,
        Some(_) => signed || redacted,
    }
}
