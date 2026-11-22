//! The agent loop.
//!
//! `Session` is one model turn. `Harness` is the thing that keeps going: model
//! turn, tool calls, tool results, next turn, until the model stops asking, the
//! turn budget runs out, or the caller cancels.
//!
//! Two invariants matter more than the rest:
//!
//! 1. Every tool call gets exactly one result, even when the tool is unknown,
//!    denied, cancelled or timed out. A missing result makes the next request
//!    invalid on both supported protocols.
//! 2. A cancellation stops the loop; it does not leave a half-written
//!    transcript. The remaining calls get explicit cancelled results.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use keel_llm_route::{RetryDecision, RetryPolicy};
use keel_llm_types::{
    Content, LlmError, LlmEvent, Message, Part, Role, StopReason, ToolCall, ToolCallId, Usage,
};
use keel_tools::{
    PermissionDecision, PermissionGate, ToolContext, ToolLimits, ToolOutcome, ToolRegistry,
    Workspace, truncate_output,
};
use tokio_util::sync::CancellationToken;

use crate::compact::CompactionReason;
use crate::session::Session;
use crate::test_loop::{
    TestOutcome, TestStatus, parse_test_output, test_command, verification_prompt,
};
use crate::working_memory::MemoryStats;

#[derive(Debug)]
pub struct Harness {
    session: Session,
    tools: Arc<ToolRegistry>,
    permissions: PermissionGate,
    workspace: Workspace,
    limits: ToolLimits,
    max_turns: usize,
    retry: RetryPolicy,
    cancel: CancellationToken,
    /// Opt-in benchmark loop: track visible tests and ask once before the run
    /// ends. Default chat leaves this off.
    test_loop: bool,
    verify_prompted: bool,
}

#[derive(Debug)]
pub enum RunEvent<'a> {
    Model(&'a LlmEvent),
    /// The turn failed mid-stream and is being replayed. The failed attempt
    /// stays in history, where repair hides it from the next request.
    Retrying {
        attempt: u32,
        after: Duration,
        error: &'a str,
    },
    ToolStarted {
        call_id: &'a str,
        name: &'a str,
        summary: &'a str,
    },
    ToolFinished {
        call_id: &'a str,
        name: &'a str,
        is_error: bool,
        preview: &'a str,
    },
    TurnFinished {
        turn: usize,
        stop: StopReason,
    },
    /// The session replaced its history with a compacted view. Emitted once
    /// per compaction; a step that threshold-compacts and then overflow-
    /// compacts emits two. The summary usage is reported separately from the
    /// assistant message so the trigger keeps reading the main request.
    Compacted {
        reason: CompactionReason,
        before_messages: usize,
        after_messages: usize,
        kept_user_messages: usize,
        estimated_tokens_before: u64,
        estimated_tokens_after: u64,
        /// Bounded working-memory counters for this cut: bytes and tokens in
        /// the injected block, file and command counts actually rendered, and
        /// compactions so far. A benchmark can compare them across cuts.
        memory: MemoryStats,
        summary_usage: Usage,
    },
    /// One pre-final verification message entered history. Emitted at most once
    /// per run and only when the harness was built with
    /// [`Harness::with_test_loop`].
    VerifyRequested {
        tests_run: bool,
        last_failed: bool,
        failing_tests: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStop {
    /// The model produced a turn with no tool calls.
    EndTurn,
    MaxTurns,
    Cancelled,
    /// The model turn failed mid-stream; the partial message is in `last`.
    Failed,
}

#[derive(Debug)]
pub struct RunOutcome {
    pub turns: usize,
    /// Usage reported by normal model turns. It deliberately excludes
    /// compaction summary requests so the next trigger reads only main-request
    /// numbers.
    pub usage: Usage,
    /// Usage reported by compaction summary requests during this run. Callers
    /// that account for spend (the CLI line, the eval budget) add it to
    /// `usage`. A compaction that fails returns `Err` and has no outcome; the
    /// session keeps its summary usage for
    /// [`Session::take_compaction_usage`](crate::Session::take_compaction_usage).
    pub compaction_usage: Usage,
    pub stop: RunStop,
    pub last: Option<Message>,
}

impl RunOutcome {
    pub fn text(&self) -> String {
        self.last.as_ref().map(Message::text).unwrap_or_default()
    }
}

impl Harness {
    /// Creates a harness around `session` and the tools it can run.
    ///
    /// The registry is both what the loop executes and what the model is told
    /// exists, so `new` copies its specs into a session whose tool list is
    /// empty. That makes the default path work without an extra `with_tools`
    /// call; otherwise the loop would execute calls the model was never
    /// offered. An empty list always means "advertise the whole registry";
    /// there is no way to hold a registry and advertise no tools.
    ///
    /// A non-empty list is left untouched as the embedder's explicit curation.
    /// Execution still resolves names through the registry, so a curated name
    /// without a registered tool becomes an error result when the model calls
    /// it.
    pub fn new(mut session: Session, tools: Arc<ToolRegistry>, workspace: Workspace) -> Self {
        if session.tools.is_empty() {
            session.tools = tools.specs();
        }
        Self {
            session,
            tools,
            // Safe default: a library embedder must opt into tool execution.
            permissions: PermissionGate::DenyAll,
            workspace,
            limits: ToolLimits::default(),
            max_turns: 24,
            retry: RetryPolicy::default(),
            cancel: CancellationToken::new(),
            test_loop: false,
            verify_prompted: false,
        }
    }

    /// Policy for replaying a turn that failed after output arrived. Pre-stream
    /// retries use the session's own policy.
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn with_permissions(mut self, permissions: PermissionGate) -> Self {
        self.permissions = permissions;
        self
    }

    pub fn with_limits(mut self, limits: ToolLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets the run's wall-clock deadline. Background jobs cap their
    /// per-command timeout by the time remaining; the loop itself keeps its
    /// turn budget.
    pub fn with_deadline(mut self, deadline: Option<Instant>) -> Self {
        self.limits.deadline = deadline;
        self
    }

    pub fn with_max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    pub fn with_cancellation(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Track visible test runs and inject one pre-final verification message at
    /// the first `EndTurn`. Benchmark adapters opt in; default chat does not.
    pub fn with_test_loop(mut self) -> Self {
        self.test_loop = true;
        self
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    pub fn tools(&self) -> &Arc<ToolRegistry> {
        &self.tools
    }

    /// Latest visible-test status, or an empty status when no test command was
    /// recognised.
    pub fn test_status(&self) -> &TestStatus {
        self.session.test_status()
    }

    pub async fn run<F>(&mut self, input: &str, mut on_event: F) -> Result<RunOutcome, LlmError>
    where
        F: for<'a> FnMut(RunEvent<'a>),
    {
        let mut usage = Usage::default();
        let mut compaction_usage = Usage::default();
        let mut last: Option<Message> = None;

        if self.cancel.is_cancelled() {
            return Ok(RunOutcome {
                turns: 0,
                usage,
                compaction_usage,
                stop: RunStop::Cancelled,
                last,
            });
        }

        let mut next_input = Some(input.to_owned());

        for turn in 1..=self.max_turns {
            if self.cancel.is_cancelled() {
                return Ok(RunOutcome {
                    turns: turn - 1,
                    usage,
                    compaction_usage,
                    stop: RunStop::Cancelled,
                    last,
                });
            }

            let mut attempt = 1;
            let outcome = loop {
                let result = self
                    .session
                    .step_streaming(next_input.as_deref(), |event| {
                        on_event(RunEvent::Model(event));
                    })
                    .await;
                // Summary usage is accumulated by the session before it knows
                // whether the compaction will succeed. Drain it here so a
                // truncated, empty or unfit summary is still charged.
                compaction_usage += self.session.take_compaction_usage();
                for stats in self.session.take_compactions() {
                    on_event(RunEvent::Compacted {
                        reason: stats.reason,
                        before_messages: stats.before_messages,
                        after_messages: stats.after_messages,
                        kept_user_messages: stats.kept_user_messages,
                        estimated_tokens_before: stats.estimated_tokens_before,
                        estimated_tokens_after: stats.estimated_tokens_after,
                        memory: stats.memory,
                        summary_usage: stats.usage,
                    });
                }
                let outcome = match result {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        // No `RunOutcome` exists to carry the spent side usage.
                        // Put it back so a budgeting caller can still drain and
                        // charge it after the error.
                        self.session.restore_compaction_usage(compaction_usage);
                        return Err(error);
                    }
                };

                let Some(error) = outcome.error.clone() else {
                    break outcome;
                };
                match self.retry.decide(&error, attempt) {
                    RetryDecision::Retry { after } => {
                        let summary = format!("{}: {}", error.kind.as_str(), error.message);
                        on_event(RunEvent::Retrying {
                            attempt,
                            after,
                            error: &summary,
                        });
                        if !self.sleep_unless_cancelled(after).await {
                            return Ok(RunOutcome {
                                turns: turn,
                                usage,
                                compaction_usage,
                                stop: RunStop::Cancelled,
                                last: Some(outcome.message),
                            });
                        }
                        // The user input was committed when the first attempt
                        // opened the stream, so a replay sends no new input.
                        next_input = None;
                        attempt += 1;
                    }
                    RetryDecision::Fail => break outcome,
                }
            };

            usage += outcome.message.usage;
            let stop = outcome.message.stop.unwrap_or(StopReason::EndTurn);
            let failed = outcome.message.error.is_some();
            let message = outcome.message.clone();
            last = Some(message);
            on_event(RunEvent::TurnFinished { turn, stop });

            if failed {
                return Ok(RunOutcome {
                    turns: turn,
                    usage,
                    compaction_usage,
                    stop: RunStop::Failed,
                    last,
                });
            }

            let calls = tool_calls(last.as_ref().expect("message was just stored"));
            if calls.is_empty() {
                if self.test_loop && !self.verify_prompted {
                    self.verify_prompted = true;
                    let status = self.session.test_status();
                    let tests_run = status.tests_run();
                    let last_failed = status.last_outcome == Some(TestOutcome::Failed);
                    let failing_tests = status.failing_tests.len();
                    let prompt = verification_prompt(status);
                    on_event(RunEvent::VerifyRequested {
                        tests_run,
                        last_failed,
                        failing_tests,
                    });
                    self.session.push_message(Message::user(prompt));
                    next_input = None;
                    continue;
                }
                return Ok(RunOutcome {
                    turns: turn,
                    usage,
                    compaction_usage,
                    stop: RunStop::EndTurn,
                    last,
                });
            }

            let mut results = Vec::with_capacity(calls.len());
            let mut cancelled = false;
            let mut index = 0;
            while index < calls.len() {
                if self.cancel.is_cancelled() {
                    cancelled = true;
                    for call in &calls[index..] {
                        results.push(cancelled_result(call));
                    }
                    break;
                }

                let end = if self.is_parallel_safe(&calls[index]) {
                    let mut end = index + 1;
                    while end < calls.len() && self.is_parallel_safe(&calls[end]) {
                        end += 1;
                    }
                    end
                } else {
                    index + 1
                };

                let mut batch = Vec::with_capacity(end - index);
                let mut stopped = false;
                for call in &calls[index..end] {
                    if self.cancel.is_cancelled() {
                        stopped = true;
                        break;
                    }
                    batch.push(self.prepare(call, &mut on_event).await);
                }

                run_batch(&mut batch).await;

                for prepared in batch {
                    results.push(self.finish(prepared, &mut on_event));
                    index += 1;
                }

                if stopped || self.cancel.is_cancelled() {
                    cancelled = true;
                    for call in &calls[index..] {
                        results.push(cancelled_result(call));
                    }
                    break;
                }
            }

            let mut message = Message::new(Role::User);
            message.parts = results;
            self.session.push_message(message);

            if cancelled {
                return Ok(RunOutcome {
                    turns: turn,
                    usage,
                    compaction_usage,
                    stop: RunStop::Cancelled,
                    last,
                });
            }

            next_input = None;
        }

        Ok(RunOutcome {
            turns: self.max_turns,
            usage,
            compaction_usage,
            stop: RunStop::MaxTurns,
            last,
        })
    }

    /// Waits out a backoff, or returns `false` if the run was cancelled first.
    async fn sleep_unless_cancelled(&self, after: Duration) -> bool {
        tokio::select! {
            () = tokio::time::sleep(after) => true,
            () = self.cancel.cancelled() => false,
        }
    }

    /// Whether `call` names a registered tool that may run beside another.
    fn is_parallel_safe(&self, call: &ToolCall) -> bool {
        self.tools
            .get(&call.name)
            .is_some_and(|tool| tool.parallel_safe())
    }

    /// Resolves, announces, parses and permission-checks one call. The result
    /// is either immediate (unknown, malformed, denied) or an execution future
    /// that `run_batch` polls beside the other prepared futures.
    async fn prepare<F>(&self, call: &ToolCall, on_event: &mut F) -> PreparedCall
    where
        F: for<'a> FnMut(RunEvent<'a>),
    {
        let Some(tool) = self.tools.get(&call.name) else {
            let available = self.tools.names().join(", ");
            return PreparedCall::immediate(
                call,
                ToolOutcome::error(format!(
                    "unknown tool `{}`; available tools: {available}",
                    call.name
                )),
                false,
            );
        };

        let arguments = match parse_arguments(&call.arguments) {
            Ok(arguments) => arguments,
            Err(message) => {
                return PreparedCall::immediate(call, ToolOutcome::error(message), false);
            }
        };

        let summary = tool.summarize(&arguments);
        let test_command = matches!(call.name.as_str(), "bash" | "terminal")
            .then(|| test_command(&arguments))
            .flatten();
        on_event(RunEvent::ToolStarted {
            call_id: call.id.as_str(),
            name: &call.name,
            summary: &summary,
        });

        match self
            .permissions
            .check(&call.name, &summary, &arguments, &self.cancel)
            .await
        {
            PermissionDecision::Deny { reason } => PreparedCall::immediate(
                call,
                ToolOutcome::error(format!("permission denied: {reason}")),
                true,
            ),
            PermissionDecision::Allow => {
                let cancel = self.cancel.clone();
                let limits = self.limits.clone();
                let context = ToolContext {
                    workspace: self.workspace.clone(),
                    cancel: cancel.clone(),
                    limits: limits.clone(),
                };
                let future = Box::pin(async move {
                    let execution =
                        tokio::time::timeout(limits.timeout, tool.execute(arguments, &context));
                    tokio::select! {
                        _ = cancel.cancelled() => ToolOutcome::error("cancelled"),
                        result = execution => match result {
                            Ok(outcome) => outcome,
                            Err(_) => ToolOutcome::error(format!(
                                "tool timed out after {}ms",
                                limits.timeout.as_millis()
                            )),
                        },
                    }
                });
                PreparedCall {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    announced: true,
                    outcome: None,
                    future: Some(future),
                    test_command,
                }
            }
        }
    }

    fn finish<F>(&mut self, prepared: PreparedCall, on_event: &mut F) -> Part
    where
        F: for<'a> FnMut(RunEvent<'a>),
    {
        let outcome = prepared
            .outcome
            .expect("every prepared call has an outcome before it is finished");
        let text = truncate_output(&outcome.text, self.limits.max_output_bytes);
        if self.test_loop {
            if let Some(command) = &prepared.test_command {
                self.session
                    .note_test_run(parse_test_output(command, &text, outcome.is_error));
            }
        }
        let preview = preview(&text, 160);
        if prepared.announced {
            on_event(RunEvent::ToolFinished {
                call_id: prepared.call_id.as_str(),
                name: prepared.name.as_str(),
                is_error: outcome.is_error,
                preview: &preview,
            });
        }
        Part::tool_result(prepared.call_id, text, outcome.is_error)
    }
}

type ToolFutureBox = Pin<Box<dyn Future<Output = ToolOutcome> + Send>>;

/// At most this many tools from one safe run execute at once. A batch larger
/// than this is split into consecutive chunks, so results still land in call
/// order.
const MAX_CONCURRENT_TOOLS: usize = 8;

struct PreparedCall {
    call_id: ToolCallId,
    name: String,
    /// Whether `ToolStarted` was emitted for this call. Unknown tools and
    /// malformed arguments have no events, matching the sequential loop.
    announced: bool,
    outcome: Option<ToolOutcome>,
    future: Option<ToolFutureBox>,
    /// Set when the call is a `bash` or `terminal` command that looks like a
    /// test runner. The finished output is parsed once.
    test_command: Option<String>,
}

impl PreparedCall {
    fn immediate(call: &ToolCall, outcome: ToolOutcome, announced: bool) -> Self {
        Self {
            call_id: call.id.clone(),
            name: call.name.clone(),
            announced,
            outcome: Some(outcome),
            future: None,
            test_command: None,
        }
    }
}

/// Polls execution futures concurrently in bounded chunks, then puts each
/// outcome back on its call so `finish` can keep call order.
async fn run_batch(batch: &mut [PreparedCall]) {
    for chunk in batch.chunks_mut(MAX_CONCURRENT_TOOLS) {
        run_chunk(chunk).await;
    }
}

async fn run_chunk(batch: &mut [PreparedCall]) {
    let running: Vec<usize> = batch
        .iter()
        .enumerate()
        .filter(|(_, call)| call.future.is_some())
        .map(|(index, _)| index)
        .collect();
    let futures: Vec<&mut ToolFutureBox> = batch
        .iter_mut()
        .filter_map(|call| call.future.as_mut())
        .collect();
    let outcomes = join_all(futures).await;
    for (index, outcome) in running.into_iter().zip(outcomes) {
        batch[index].future = None;
        batch[index].outcome = Some(outcome);
    }
}

fn cancelled_result(call: &ToolCall) -> Part {
    Part::tool_result(call.id.clone(), "cancelled before execution", true)
}

fn tool_calls(message: &Message) -> Vec<ToolCall> {
    message
        .parts
        .iter()
        .filter_map(|part| match &part.content {
            Content::ToolCall {
                id,
                name,
                arguments,
            } => Some(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn parse_arguments(raw: &str) -> Result<serde_json::Value, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(serde_json::json!({}));
    }
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value) if value.is_object() => Ok(value),
        Ok(value) => Ok(serde_json::json!({"value": value})),
        Err(error) => Err(format!("invalid tool arguments: {error}")),
    }
}

fn preview(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    truncate_output(text, max_bytes)
}

/// A convenience limit for tests and embedders that want a short leash.
pub fn short_limits(timeout: Duration) -> ToolLimits {
    ToolLimits {
        timeout,
        ..ToolLimits::default()
    }
}
