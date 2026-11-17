//! A canonical request. Transport-independent: no URL, no headers, no auth.

use serde::{Deserialize, Serialize};

use crate::ids::ModelId;
use crate::message::Message;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema for the tool input, before any per-protocol sanitizing.
    pub input_schema: serde_json::Value,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Named(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePolicy {
    /// Place breakpoints where they pay off, up to the protocol's cap.
    #[default]
    Auto,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub model: ModelId,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub tool_choice: ToolChoice,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub reasoning: Option<ReasoningEffort>,
    pub cache: CachePolicy,
}

impl Request {
    pub fn new(model: impl Into<ModelId>) -> Self {
        Self {
            model: model.into(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            reasoning: None,
            cache: CachePolicy::Auto,
        }
    }

    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn messages(mut self, messages: Vec<Message>) -> Self {
        self.messages = messages;
        self
    }

    pub fn tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = choice;
        self
    }

    pub fn max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = Some(tokens);
        self
    }

    pub fn temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn reasoning(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning = Some(effort);
        self
    }

    pub fn cache(mut self, policy: CachePolicy) -> Self {
        self.cache = policy;
        self
    }
}
