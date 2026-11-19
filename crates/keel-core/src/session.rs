//! The session loop.
//!
//! One step is: compact when the next request would not fit, repair the
//! transcript against the target model, lower it into a request, stream the
//! response, assemble one assistant message, record it.
//!
//! Retry policy lives here, not in the transport: after any output has arrived,
//! replaying is a session decision. Context overflow is the one failure this
//! layer handles itself: it compacts once and replays the same request.

use std::sync::Arc;

use futures_util::StreamExt;

use keel_llm_route::{Client, RetryPolicy, Route};
use keel_llm_types::{
    CachePolicy, ErrorKind, LlmError, LlmEvent, Message, RepairNote, ReplayTarget, Request,
    ToolChoice, ToolSpec, repair_transcript,
};

use crate::assemble::Assembler;
use crate::compact::{
    COMPACTION_PROMPT, CompactionReason, CompactionStats, SUMMARY_MIN_OUTPUT_TOKENS,
    compacted_view, summary_output_budget, view_tokens,
};
use crate::prompt::{DEFAULT_SYSTEM_PROMPT, effective_system_prompt};
use crate::store::CompactionRecord;
use crate::tokens::{estimate_messages_tokens, estimate_request_input_tokens};

/// Somewhere messages go as they enter history, in order.
///
/// A sink sees the canonical message, not the wire form, so a log stores what
/// the session actually holds. Recording must not fail a turn, so the method
/// returns nothing; a sink that hits an error keeps it for the caller to read.
pub trait SessionSink: Send + Sync + std::fmt::Debug {
    fn message(&self, message: &Message);

    /// One compaction replaced the history with `messages`. The record holds
    /// both the summary and the replacement view; the sink appends it without
    /// touching earlier bytes. The default is a no-op for sinks that only care
    /// about messages.
    fn compaction(&self, _record: &CompactionRecord) {}
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
    /// The most recent compaction, waiting to be reported by the harness.
    compaction: Option<CompactionStats>,
}

#[derive(Debug)]
pub struct StepOutcome {
    pub message: Message,
    pub notes: Vec<RepairNote>,
    pub warnings: Vec<String>,
    /// The typed failure when the turn failed. `message.error` carries the same
    /// thing as text for the log; this keeps the kind and `retry-after` so a
    /// caller can decide whether replaying the turn is safe.
    pub error: Option<LlmError>,
}

impl StepOutcome {
    pub fn text(&self) -> String {
        self.message.text()
    }

    pub fn failed(&self) -> bool {
        self.error.is_some()
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
            system: Some(DEFAULT_SYSTEM_PROMPT.to_owned()),
            tools: Vec::new(),
            max_output_tokens: None,
            temperature: None,
            retry: RetryPolicy::default(),
            sink: None,
            compaction: None,
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

    /// Appends host instructions after the prompt already in effect.
    ///
    /// A new session starts with [`DEFAULT_SYSTEM_PROMPT`]; a resumed session
    /// starts with the prompt stored in its header. Appending never adds the
    /// default a second time. To replace the prompt entirely, assign
    /// [`Session::system`] directly.
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        let system = system.into();
        self.system = effective_system_prompt(self.system.as_deref(), Some(&system));
        self
    }

    /// Sets the tool list the model sees.
    ///
    /// [`Harness::new`](crate::Harness::new) fills this from its registry when
    /// the list is empty, so an empty list always advertises the whole
    /// registry; there is no way to advertise no tools while holding one. A
    /// non-empty list is kept as the explicit selection.
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

    /// The last compaction this session ran, if the caller has not collected
    /// it yet. The harness turns it into a status event.
    pub fn take_compaction(&mut self) -> Option<CompactionStats> {
        self.compaction.take()
    }

    pub async fn step(&mut self, input: Option<&str>) -> Result<StepOutcome, LlmError> {
        self.step_streaming(input, |_| {}).await
    }

    /// Stream one turn, invoking `on_event` for each canonical event.
    ///
    /// A pre-stream failure returns `Err` and leaves history untouched, so the
    /// caller can retry without duplicating user input. A mid-stream failure
    /// returns `Ok` with `message.error` set, `StepOutcome::error` holding the
    /// typed failure, and the partial parts preserved; repair drops that turn
    /// on the next request, which is what makes a replay safe.
    ///
    /// Compaction runs before the request when the trigger rule says the next
    /// request would not fit, and once more per turn on a `ContextOverflow`
    /// error. A second overflow is returned as a normal failure.
    pub async fn step_streaming<F>(
        &mut self,
        input: Option<&str>,
        mut on_event: F,
    ) -> Result<StepOutcome, LlmError>
    where
        F: FnMut(&LlmEvent),
    {
        let mut pending = input
            .filter(|text| !text.trim().is_empty())
            .map(Message::user);
        let mut overflow_retried = false;
        let mut compacted_this_step = false;

        loop {
            if !compacted_this_step && !overflow_retried && self.should_compact(pending.as_ref()) {
                self.compact(CompactionReason::Threshold).await?;
                compacted_this_step = true;
            }

            let mut preview = self.history.clone();
            if let Some(message) = &pending {
                preview.push(message.clone());
            }
            let repaired = repair_transcript(preview, &self.target);
            let request = self.request_for(repaired.messages);

            let mut stream = match self
                .client
                .stream_with_retry(&self.route, &request, &self.retry)
                .await
            {
                Ok(stream) => stream,
                Err(error) if error.kind == ErrorKind::ContextOverflow && !overflow_retried => {
                    // Nothing was committed: the user input is still pending
                    // and will be sent once after compaction.
                    self.compact(CompactionReason::Overflow).await?;
                    overflow_retried = true;
                    continue;
                }
                Err(error) => return Err(error),
            };

            // Commit user input only after the stream opens.
            if let Some(message) = pending.take() {
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
            let error = turn.error.clone();
            let message = turn.into_message(&self.target.source());

            if error
                .as_ref()
                .is_some_and(|error| error.kind == ErrorKind::ContextOverflow)
                && !overflow_retried
            {
                // The user input is already committed. Keep the failed turn in
                // history, as the normal retry path does, but let compaction
                // replace the view before the replay. The failed assistant
                // turn is not copied into the view, so it is not duplicated.
                self.push_message(message);
                self.compact(CompactionReason::Overflow).await?;
                overflow_retried = true;
                continue;
            }

            let notes = repaired.notes;
            self.push_message(message.clone());

            return Ok(StepOutcome {
                message,
                notes,
                warnings,
                error,
            });
        }
    }

    fn request_for(&self, messages: Vec<Message>) -> Request {
        let mut request = Request::new(self.route.model.clone())
            .messages(messages)
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
        request
    }

    /// The output budget the next request will claim.
    fn requested_output_tokens(&self) -> u64 {
        u64::from(
            self.max_output_tokens
                .unwrap_or(self.route.max_output_tokens),
        )
    }

    /// The last usage the provider reported, from the newest message that has
    /// one. A compacted view carries no usage, so this returns `None` until the
    /// next real turn reports some.
    fn last_reported_usage(&self) -> Option<(u64, u64)> {
        self.history.iter().rev().find_map(|message| {
            (message.usage.input_tokens > 0)
                .then_some((message.usage.input_tokens, message.usage.output_tokens))
        })
    }

    /// The M4 trigger rule: compact before the next request when the last
    /// reported input plus output plus this request's output budget reach the
    /// catalog context window. With no reported usage yet (a fresh resume or
    /// the first request), estimate the request instead.
    fn should_compact(&self, pending: Option<&Message>) -> bool {
        let window = u64::from(self.route.context_window);
        if window == 0 || self.history.is_empty() {
            return false;
        }
        let output = self.requested_output_tokens();
        if let Some((input, reported_output)) = self.last_reported_usage() {
            return input.saturating_add(reported_output).saturating_add(output) >= window;
        }

        let mut preview = self.history.clone();
        if let Some(message) = pending {
            preview.push(message.clone());
        }
        let repaired = repair_transcript(preview, &self.target);
        let estimate =
            estimate_request_input_tokens(self.system.as_deref(), &repaired.messages, &self.tools);
        estimate.saturating_add(output) >= window
    }

    /// Summarizes the committed history with the session's own route and model,
    /// then replaces history with the compacted view.
    ///
    /// The summary request offers no tools. It is checked against the context
    /// window with the conservative estimator before it is sent; if even a
    /// minimal summary cannot fit, this fails with a clear error instead of
    /// sending a request that the provider would reject.
    async fn compact(&mut self, reason: CompactionReason) -> Result<CompactionStats, LlmError> {
        let window = self.route.context_window;
        if window == 0 {
            return Err(LlmError::new(
                ErrorKind::InvalidRequest,
                "cannot compact: the route has no catalog context window",
            ));
        }
        if self.history.is_empty() {
            return Err(LlmError::new(
                ErrorKind::ContextOverflow,
                "cannot compact: there is no earlier message to summarize",
            ));
        }

        let before_messages = self.history.len();
        let estimated_tokens_before = estimate_messages_tokens(&self.history);

        let repaired = repair_transcript(self.history.clone(), &self.target);
        let mut summary_messages = repaired.messages;
        summary_messages.push(Message::user(COMPACTION_PROMPT));

        let input_estimate =
            estimate_request_input_tokens(self.system.as_deref(), &summary_messages, &[]);
        let Some(summary_output_tokens) = summary_output_budget(window, input_estimate) else {
            return Err(LlmError::new(
                ErrorKind::InvalidRequest,
                format!(
                    "cannot compact: the summary request needs about {input_estimate} tokens, \
                     leaving less than {SUMMARY_MIN_OUTPUT_TOKENS} of the {window}-token context \
                     window for the summary"
                ),
            ));
        };

        let mut request = Request::new(self.route.model.clone())
            .messages(summary_messages)
            .tool_choice(ToolChoice::None)
            .cache(CachePolicy::Auto)
            .max_output_tokens(summary_output_tokens);
        if let Some(system) = &self.system {
            request = request.system(system.clone());
        }

        let mut stream = self
            .client
            .stream_with_retry(&self.route, &request, &self.retry)
            .await?;
        let mut assembler = Assembler::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(event) => assembler.apply(event),
                Err(error) => {
                    assembler.fail(error);
                    break;
                }
            }
        }
        let turn = assembler.finish();
        if let Some(error) = turn.error.clone() {
            return Err(error);
        }
        let summary_usage = turn.usage;
        let summary_text = turn
            .into_message(&self.target.source())
            .text()
            .trim()
            .to_owned();
        if summary_text.is_empty() {
            return Err(LlmError::decode(
                "cannot compact: the summary response was empty",
            ));
        }

        let (view, kept_user_messages) = compacted_view(&self.history, &summary_text, window);
        let estimated_tokens_after = view_tokens(&view);
        let summary = view
            .last()
            .cloned()
            .expect("the compacted view always ends with the summary");
        let record = CompactionRecord {
            reason,
            summary,
            messages: view.clone(),
        };
        if let Some(sink) = &self.sink {
            sink.compaction(&record);
        }
        self.history = view;

        let stats = CompactionStats {
            reason,
            before_messages,
            after_messages: self.history.len(),
            kept_user_messages,
            estimated_tokens_before,
            estimated_tokens_after,
            summary_output_tokens,
            usage: summary_usage,
        };
        self.compaction = Some(stats.clone());
        Ok(stats)
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }
}
