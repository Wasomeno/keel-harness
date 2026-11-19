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

use std::sync::Arc;
use std::time::Duration;

use keel_llm_route::{RetryDecision, RetryPolicy};
use keel_llm_types::{
    Content, LlmError, LlmEvent, Message, Part, Role, StopReason, ToolCall, Usage,
};
use keel_tools::{
    PermissionDecision, PermissionGate, ToolContext, ToolLimits, ToolOutcome, ToolRegistry,
    Workspace, truncate_output,
};
use tokio_util::sync::CancellationToken;

use crate::compact::CompactionReason;
use crate::session::Session;

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
        summary_usage: Usage,
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
    /// Usage reported by compaction summary requests. Callers that account for
    /// spend (the CLI line, the eval budget) add it to `usage`.
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

    pub fn with_max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    pub fn with_cancellation(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
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
                for stats in self.session.take_compactions() {
                    compaction_usage += stats.usage;
                    on_event(RunEvent::Compacted {
                        reason: stats.reason,
                        before_messages: stats.before_messages,
                        after_messages: stats.after_messages,
                        kept_user_messages: stats.kept_user_messages,
                        estimated_tokens_before: stats.estimated_tokens_before,
                        estimated_tokens_after: stats.estimated_tokens_after,
                        summary_usage: stats.usage,
                    });
                }
                let outcome = result?;

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
            for call in calls {
                let part = if cancelled {
                    Part::tool_result(call.id, "cancelled before execution", true)
                } else {
                    let part = self.execute(&call, &mut on_event).await;
                    if self.cancel.is_cancelled() {
                        cancelled = true;
                    }
                    part
                };
                results.push(part);
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

    async fn execute<F>(&self, call: &ToolCall, on_event: &mut F) -> Part
    where
        F: for<'a> FnMut(RunEvent<'a>),
    {
        let Some(tool) = self.tools.get(&call.name) else {
            let available = self.tools.names().join(", ");
            return Part::tool_result(
                call.id.clone(),
                format!("unknown tool `{}`; available tools: {available}", call.name),
                true,
            );
        };

        let arguments = match parse_arguments(&call.arguments) {
            Ok(arguments) => arguments,
            Err(message) => return Part::tool_result(call.id.clone(), message, true),
        };

        let summary = tool.summarize(&arguments);
        on_event(RunEvent::ToolStarted {
            call_id: call.id.as_str(),
            name: &call.name,
            summary: &summary,
        });

        let outcome = match self
            .permissions
            .check(&call.name, &summary, &arguments, &self.cancel)
            .await
        {
            PermissionDecision::Deny { reason } => {
                ToolOutcome::error(format!("permission denied: {reason}"))
            }
            PermissionDecision::Allow => {
                let context = ToolContext {
                    workspace: self.workspace.clone(),
                    cancel: self.cancel.clone(),
                    limits: self.limits.clone(),
                };
                let execution =
                    tokio::time::timeout(self.limits.timeout, tool.execute(arguments, &context));
                tokio::select! {
                    _ = self.cancel.cancelled() => ToolOutcome::error("cancelled"),
                    result = execution => match result {
                        Ok(outcome) => outcome,
                        Err(_) => ToolOutcome::error(format!(
                            "tool timed out after {}ms",
                            self.limits.timeout.as_millis()
                        )),
                    },
                }
            }
        };

        let text = truncate_output(&outcome.text, self.limits.max_output_bytes);
        let preview = preview(&text, 160);
        on_event(RunEvent::ToolFinished {
            call_id: call.id.as_str(),
            name: &call.name,
            is_error: outcome.is_error,
            preview: &preview,
        });

        Part::tool_result(call.id.clone(), text, outcome.is_error)
    }
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
