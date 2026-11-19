//! The session loop.
//!
//! One step is: repair the transcript against the target model, lower it into a
//! request, stream the response, assemble one assistant message, record it.
//!
//! Retry policy lives here, not in the transport: after any output has arrived,
//! replaying is a session decision.

use std::sync::Arc;

use futures_util::StreamExt;

use keel_llm_route::{Client, RetryPolicy, Route};
use keel_llm_types::{
    CachePolicy, LlmError, LlmEvent, Message, RepairNote, ReplayTarget, Request, ToolChoice,
    ToolSpec, repair_transcript,
};

use crate::assemble::Assembler;

/// Somewhere messages go as they enter history, in order.
///
/// A sink sees the canonical message, not the wire form, so a log stores what
/// the session actually holds. Recording must not fail a turn, so the method
/// returns nothing; a sink that hits an error keeps it for the caller to read.
pub trait SessionSink: Send + Sync + std::fmt::Debug {
    fn message(&self, message: &Message);
}

#[derive(Debug)]
pub struct Session {
    pub route: Route,
    pub client: Client,
    pub target: ReplayTarget,
    /// Raw turns as produced. Repair is a view for the wire, not a mutation, so
    /// failed turns stay here for inspection and are skipped on replay.
    pub history: Vec<Message>,
    pub system: Option<String>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub retry: RetryPolicy,
    sink: Option<Arc<dyn SessionSink>>,
}

#[derive(Debug)]
pub struct StepOutcome {
    pub message: Message,
    pub notes: Vec<RepairNote>,
    pub warnings: Vec<String>,
}

impl StepOutcome {
    pub fn text(&self) -> String {
        self.message.text()
    }

    pub fn failed(&self) -> bool {
        self.message.error.is_some()
    }
}

impl Session {
    pub fn new(route: Route, client: Client) -> Self {
        let target = ReplayTarget::new(
            route.protocol_id(),
            route.provider.clone(),
            route.model.clone(),
        )
        .with_images(route.protocol.compat().supports_images);

        Self {
            route,
            client,
            target,
            history: Vec::new(),
            system: None,
            tools: Vec::new(),
            max_output_tokens: None,
            temperature: None,
            retry: RetryPolicy::default(),
            sink: None,
        }
    }

    /// Records every message as it enters history. The sink is not used to
    /// rebuild state; resume passes stored messages back through
    /// [`push_message`](Self::push_message) or sets `history` before running.
    pub fn with_sink(mut self, sink: Arc<dyn SessionSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Appends to history and tells the sink. Every history append goes through
    /// here, so a log sees the same order the wire will.
    pub fn push_message(&mut self, message: Message) {
        if let Some(sink) = &self.sink {
            sink.message(&message);
        }
        self.history.push(message);
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = Some(tokens);
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub async fn step(&mut self, input: Option<&str>) -> Result<StepOutcome, LlmError> {
        self.step_streaming(input, |_| {}).await
    }

    /// Stream one turn, invoking `on_event` for each canonical event.
    ///
    /// A pre-stream failure returns `Err` and leaves history untouched, so the
    /// caller can retry without duplicating user input. A mid-stream failure
    /// returns `Ok` with `message.error` set and the partial parts preserved;
    /// repair drops that turn on the next request.
    pub async fn step_streaming<F>(
        &mut self,
        input: Option<&str>,
        mut on_event: F,
    ) -> Result<StepOutcome, LlmError>
    where
        F: FnMut(&LlmEvent),
    {
        let pending_user = input
            .filter(|text| !text.trim().is_empty())
            .map(Message::user);

        let mut preview = self.history.clone();
        if let Some(message) = &pending_user {
            preview.push(message.clone());
        }
        let repaired = repair_transcript(preview, &self.target);

        let mut request = Request::new(self.route.model.clone())
            .messages(repaired.messages)
            .tool_choice(ToolChoice::Auto)
            .cache(CachePolicy::Auto);
        if let Some(system) = &self.system {
            request = request.system(system.clone());
        }
        if !self.tools.is_empty() {
            request = request.tools(self.tools.clone());
        }
        if let Some(max) = self.max_output_tokens {
            request = request.max_output_tokens(max);
        }
        if let Some(temperature) = self.temperature {
            request = request.temperature(temperature);
        }

        // Commit user input only after the stream opens.
        let mut stream = self
            .client
            .stream_with_retry(&self.route, &request, &self.retry)
            .await?;
        if let Some(message) = pending_user {
            self.push_message(message);
        }

        let mut assembler = Assembler::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(event) => {
                    on_event(&event);
                    assembler.apply(event);
                }
                Err(error) => {
                    assembler.fail(error);
                    break;
                }
            }
        }

        let turn = assembler.finish();
        let warnings = turn.warnings.clone();
        let message = turn.into_message(&self.target.source());
        self.push_message(message.clone());

        Ok(StepOutcome {
            message,
            notes: repaired.notes,
            warnings,
        })
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }
}
