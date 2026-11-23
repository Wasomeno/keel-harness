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
    ImageAttachment, PermissionDecision, PermissionGate, SessionState, ToolContext, ToolLimits,
    ToolOutcome, ToolRegistry, Workspace, truncate_output,
};
use tokio_util::sync::CancellationToken;

use crate::budget::{BudgetConfig, BudgetState, PrematureReason, ProgressCounters, StallSignal};
use crate::compact::CompactionReason;
use crate::hooks::{HookConfig, ToolHook};
use crate::session::Session;
use crate::skills::SkillCatalog;
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
    /// Progress accounting, stall rules, the soft turn budget and the
    /// premature-finish guard. Disabled by default; a benchmark runner opts in.
    budget: BudgetState,
    /// Opt-in benchmark loop: track visible tests and ask once before the run
    /// ends. Default chat leaves this off.
    test_loop: bool,
    verify_prompted: bool,
    /// Durable state attached to every tool call. `None` keeps a stateful
    /// tool's value inside the tool for this run.
    session_state: Option<Arc<dyn SessionState>>,
    /// Name and description listings injected once, on the first run.
    skills: SkillCatalog,
    skills_injected: bool,
    /// Workspace command hooks. Empty means the loop never spawns a hook.
    hooks: HookConfig,
    hook_session_id: String,
    hook_source: String,
    session_hooks_started: bool,
    /// Landlock confinement for `bash` and `jobs` children. Off by default.
    sandbox: bool,
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
    /// Bounded progress counters after one completed turn. Emitted once per
    /// turn only when budget discipline is enabled; the same snapshot is in
    /// [`RunOutcome::progress`] at the end. The event is the one progress
    /// signal an embedder needs; the individual observations are counters, not
    /// separate events.
    Progress {
        counters: ProgressCounters,
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
    /// The run's wall-clock deadline passed at a turn boundary.
    TaskTimeout,
    /// A stall rule fired. The signal is part of the stop reason.
    Stalled(StallSignal),
    /// The model ended the run, but the guard rejected the finish.
    PrematureFinish(PrematureReason),
    Cancelled,
    /// The model turn failed mid-stream; the partial message is in `last`.
    Failed,
}

impl RunStop {
    /// The stable label used by the CLI status line, the adapters and their
    /// traces. A harness cut never reports the model's `end_turn` label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EndTurn => "end",
            Self::MaxTurns => "max_turns",
            Self::TaskTimeout => "task_timeout",
            Self::Stalled(signal) => signal.stop_label(),
            Self::PrematureFinish(reason) => reason.stop_label(),
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    /// Whether the harness, rather than the model, ended the run.
    pub const fn is_harness_cut(self) -> bool {
        !matches!(self, Self::EndTurn)
    }
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
    /// The one-sentence diagnosis for a stall or premature finish: the signal,
    /// what was last observed, and the cheapest next step. `None` for an
    /// ordinary stop.
    pub stop_detail: Option<String>,
    /// Bounded progress counters for the run and each phase.
    pub progress: ProgressCounters,
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
            budget: BudgetState::new(BudgetConfig::default()),
            test_loop: false,
            verify_prompted: false,
            session_state: None,
            skills: SkillCatalog::empty(),
            skills_injected: false,
            hooks: HookConfig::empty(),
            hook_session_id: "keel".to_owned(),
            hook_source: "startup".to_owned(),
            session_hooks_started: false,
            sandbox: false,
        }
    }

    /// Lists discovered skills in the system prompt on the first run, and is
    /// the catalog the `skill` tool should have been registered with.
    pub fn with_skills(mut self, skills: SkillCatalog) -> Self {
        self.skills = skills;
        self
    }

    /// Command hooks loaded from the workspace. An empty config changes nothing.
    pub fn with_hooks(mut self, hooks: HookConfig) -> Self {
        self.hooks = hooks;
        self
    }

    /// Session id and `SessionStart` source (`startup` or `resume`) sent to
    /// hook commands on stdin.
    pub fn with_hook_context(
        mut self,
        session_id: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        self.hook_session_id = session_id.into();
        self.hook_source = source.into();
        self
    }

    /// Confines `bash` and `jobs` children with Landlock when `enabled`.
    ///
    /// The keel process is not confined. File tools keep their descriptor
    /// opens. When Landlock is missing and `enabled` is set, those tools
    /// refuse the call instead of running unsandboxed. Hook commands are not
    /// routed through this path.
    pub fn with_sandbox(mut self, enabled: bool) -> Self {
        self.sandbox = enabled;
        self
    }

    /// Attaches durable state that outlives the process, such as the
    /// working directory the one-shot `bash` tool keeps. A resume passes the
    /// reopened session log here.
    pub fn with_session_state(mut self, state: Arc<dyn SessionState>) -> Self {
        self.session_state = Some(state);
        self
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
    /// per-command timeout by the time remaining, and the loop checks the same
    /// deadline at every turn boundary before starting another model request.
    /// A command is never killed mid-write by this check; the session log stays
    /// complete either way.
    pub fn with_deadline(mut self, deadline: Option<Instant>) -> Self {
        self.limits.deadline = deadline;
        self
    }

    /// Enables progress accounting, the selected stall rules, the soft turn
    /// budget and the premature-finish guard. A config with every field zero
    /// (the default) leaves normal chat unchanged.
    pub fn with_budget(mut self, config: BudgetConfig) -> Self {
        self.budget = BudgetState::new(config);
        self
    }

    /// Shorthand for [`Harness::with_budget`] with the benchmark preset:
    /// no-progress 8 turns, identical calls 3, unshrunk test failures 3,
    /// reserve 5 turns, premature-finish guard on.
    pub fn with_budget_guard(self) -> Self {
        self.with_budget(BudgetConfig::benchmark())
    }

    pub fn budget_config(&self) -> &BudgetConfig {
        self.budget.config()
    }

    pub fn progress(&self) -> &ProgressCounters {
        self.budget.counters()
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
        self.budget.reset();
        let mut usage = Usage::default();
        let mut compaction_usage = Usage::default();
        let mut last: Option<Message> = None;

        if self.cancel.is_cancelled() {
            return Ok(self.outcome(0, usage, compaction_usage, RunStop::Cancelled, None, last));
        }

        self.inject_skills();
        self.session_start_hooks().await;

        let mut next_input = Some(input.to_owned());

        for turn in 1..=self.max_turns {
            if self.cancel.is_cancelled() {
                return Ok(self.outcome(
                    turn - 1,
                    usage,
                    compaction_usage,
                    RunStop::Cancelled,
                    None,
                    last,
                ));
            }
            if self.deadline_passed() {
                return Ok(self.outcome(
                    turn - 1,
                    usage,
                    compaction_usage,
                    RunStop::TaskTimeout,
                    Some(
                        "the task wall-clock deadline passed before the next model turn; \
                         the harness stopped at the turn boundary"
                            .to_owned(),
                    ),
                    last,
                ));
            }

            self.budget.begin_turn(self.workspace.root());

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
                            return Ok(self.outcome(
                                turn,
                                usage,
                                compaction_usage,
                                RunStop::Cancelled,
                                None,
                                Some(outcome.message),
                            ));
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
                self.finish_turn_and_report(&mut on_event, None);
                return Ok(self.outcome(
                    turn,
                    usage,
                    compaction_usage,
                    RunStop::Failed,
                    None,
                    last,
                ));
            }

            let calls = tool_calls(last.as_ref().expect("message was just stored"));
            if calls.is_empty() {
                self.finish_turn_and_report(&mut on_event, None);
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
                    self.budget.note_verify_prompt();
                    self.session.push_message(Message::user(prompt));
                    next_input = None;
                    continue;
                }
                if let Some((stop, detail)) = self.premature_stop() {
                    return Ok(self.outcome(
                        turn,
                        usage,
                        compaction_usage,
                        stop,
                        Some(detail),
                        last,
                    ));
                }
                return Ok(self.outcome(
                    turn,
                    usage,
                    compaction_usage,
                    RunStop::EndTurn,
                    None,
                    last,
                ));
            }

            let mut results = Vec::with_capacity(calls.len());
            // One follow-up message per image-producing call, in call order.
            // A batch can run concurrently, so the images are collected from
            // the finished outcome, never from completion order.
            let mut image_messages: Vec<Vec<ImageAttachment>> = Vec::new();
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
                    let (part, images) = self.finish(prepared, &mut on_event);
                    results.push(part);
                    if !images.is_empty() {
                        image_messages.push(images);
                    }
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

            // A tool result is text on every protocol, so images travel in a
            // separate user message right after the results. A target that
            // cannot see images keeps the caption and replaces each image with
            // `[image omitted: <mime>]` during repair, the existing downgrade
            // path, which also records `RepairNote::DowngradedImage`.
            for images in image_messages {
                self.session.push_message(image_message(images));
            }

            if cancelled {
                self.finish_turn_and_report(&mut on_event, None);
                return Ok(self.outcome(
                    turn,
                    usage,
                    compaction_usage,
                    RunStop::Cancelled,
                    None,
                    last,
                ));
            }

            let workspace_after = self.budget.scan(self.workspace.root());
            self.finish_turn_and_report(&mut on_event, workspace_after);

            // A deadline that expires while tools run is still a graceful stop:
            // every call already has its result, so the transcript is complete.
            if self.deadline_passed() {
                return Ok(self.outcome(
                    turn,
                    usage,
                    compaction_usage,
                    RunStop::TaskTimeout,
                    Some(
                        "the task wall-clock deadline passed after the turn's tools \
                         finished; the harness stopped before another model request"
                            .to_owned(),
                    ),
                    last,
                ));
            }

            if let Some(detail) = self.budget.stall() {
                let message = detail.message();
                // Carry the diagnosis into history so a --resume retry sees
                // the same observation and next step the trace does.
                self.session
                    .push_message(Message::user(detail.history_message()));
                return Ok(self.outcome(
                    turn,
                    usage,
                    compaction_usage,
                    RunStop::Stalled(detail.signal),
                    Some(message),
                    last,
                ));
            }

            let turns_left = self.max_turns.saturating_sub(turn);
            if let Some(prompt) = self.budget.reserve_prompt(turns_left) {
                self.session.push_message(Message::user(prompt));
            }

            next_input = None;
        }

        Ok(self.outcome(
            self.max_turns,
            usage,
            compaction_usage,
            RunStop::MaxTurns,
            None,
            last,
        ))
    }

    fn outcome(
        &self,
        turns: usize,
        usage: Usage,
        compaction_usage: Usage,
        stop: RunStop,
        stop_detail: Option<String>,
        last: Option<Message>,
    ) -> RunOutcome {
        RunOutcome {
            turns,
            usage,
            compaction_usage,
            stop,
            stop_detail,
            progress: self.budget.counters().clone(),
            last,
        }
    }

    fn finish_turn_and_report<F>(&mut self, on_event: &mut F, workspace_after: Option<u64>)
    where
        F: for<'a> FnMut(RunEvent<'a>),
    {
        self.budget.finish_turn(workspace_after);
        if self.budget.is_enabled() {
            on_event(RunEvent::Progress {
                counters: self.budget.counters().clone(),
            });
        }
    }

    fn deadline_passed(&self) -> bool {
        self.limits
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// Rejects a model `EndTurn` that produced no mutation, or a mutated run
    /// that never ran a visible test while the adapter expected one. This is
    /// only consulted on `EndTurn`, so a failure or cancellation is never
    /// relabelled.
    fn premature_stop(&self) -> Option<(RunStop, String)> {
        let config = self.budget.config();
        if !config.premature_finish {
            return None;
        }
        if self.budget.mutations() == 0 {
            return Some((
                RunStop::PrematureFinish(PrematureReason::NoChanges),
                "premature finish (no_changes): the model ended the run without a \
                 successful write, edit or apply_patch. Cheapest next step: make the \
                 required edit, then run one narrow visible test."
                    .to_owned(),
            ));
        }
        if config.expect_tests && self.budget.test_runs() == 0 {
            return Some((
                RunStop::PrematureFinish(PrematureReason::NoVisibleTest),
                "premature finish (no_visible_test): the model changed a file but ran \
                 no visible test, and the adapter discovered a test command for this \
                 repository. Cheapest next step: run the narrowest test that exercises \
                 the change, then finish."
                    .to_owned(),
            ));
        }
        None
    }

    /// Waits out a backoff, or returns `false` if the run was cancelled first.
    async fn sleep_unless_cancelled(&self, after: Duration) -> bool {
        tokio::select! {
            () = tokio::time::sleep(after) => true,
            () = self.cancel.cancelled() => false,
        }
    }

    /// Appends the skill name/description listing once. The body stays unloaded.
    fn inject_skills(&mut self) {
        if self.skills_injected {
            return;
        }
        self.skills_injected = true;
        if let Some(listing) = self.skills.listing() {
            self.session.append_system(&listing);
        }
    }

    /// Runs `SessionStart` hooks once. Their stdout is extra system text. A
    /// failing hook does not stop the session; the error is part of that text.
    async fn session_start_hooks(&mut self) {
        if self.session_hooks_started {
            return;
        }
        self.session_hooks_started = true;
        if let Some(extra) = self
            .hooks
            .session_start(
                &self.hook_session_id,
                self.workspace.root(),
                &self.hook_source,
                &self.cancel,
            )
            .await
        {
            self.session.append_system(&extra);
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
                preview(&call.arguments, 140),
            );
        };

        let arguments = match parse_arguments(&call.arguments) {
            Ok(arguments) => arguments,
            Err(message) => {
                return PreparedCall::immediate(
                    call,
                    ToolOutcome::error(message),
                    false,
                    preview(&call.arguments, 140),
                );
            }
        };

        let summary = tool.summarize(&arguments);
        let test_command = (call.name == "bash")
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
                summary,
            ),
            PermissionDecision::Allow => {
                let cancel = self.cancel.clone();
                let limits = self.limits.clone();
                let context = ToolContext {
                    workspace: self.workspace.clone(),
                    cancel: cancel.clone(),
                    limits: limits.clone(),
                    session_state: self.session_state.clone(),
                    sandbox: self.sandbox,
                };
                let hooks = self.hooks.clone();
                let hook_cwd = self.workspace.root().to_path_buf();
                let hook_session = self.hook_session_id.clone();
                let tool_name = call.name.clone();
                let call_id = call.id.as_str().to_owned();
                let hook_input = arguments.clone();
                let future = Box::pin(async move {
                    let hook_call = ToolHook {
                        session_id: &hook_session,
                        cwd: &hook_cwd,
                        tool: &tool_name,
                        tool_use_id: &call_id,
                        tool_input: &hook_input,
                        cancel: &cancel,
                    };
                    if let Err(message) = hooks.pre_tool(&hook_call).await {
                        return ToolOutcome::error(message);
                    }
                    let execution =
                        tokio::time::timeout(limits.timeout, tool.execute(arguments, &context));
                    let outcome = tokio::select! {
                        _ = cancel.cancelled() => return ToolOutcome::error("cancelled"),
                        result = execution => match result {
                            Ok(outcome) => outcome,
                            Err(_) => ToolOutcome::error(format!(
                                "tool timed out after {}ms",
                                limits.timeout.as_millis()
                            )),
                        },
                    };
                    if let Err(message) = hooks.post_tool(&hook_call, &outcome.text).await {
                        let text = if outcome.text.is_empty() {
                            message
                        } else {
                            format!("{}\n\n{message}", outcome.text)
                        };
                        return ToolOutcome::error(text);
                    }
                    outcome
                });
                PreparedCall {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    summary,
                    announced: true,
                    outcome: None,
                    future: Some(future),
                    test_command,
                }
            }
        }
    }

    fn finish<F>(
        &mut self,
        prepared: PreparedCall,
        on_event: &mut F,
    ) -> (Part, Vec<ImageAttachment>)
    where
        F: for<'a> FnMut(RunEvent<'a>),
    {
        let ToolOutcome {
            text: raw_text,
            is_error,
            images,
        } = prepared
            .outcome
            .expect("every prepared call has an outcome before it is finished");
        let text = truncate_output(&raw_text, self.limits.max_output_bytes);
        let test_run = if self.test_loop || self.budget.is_enabled() {
            prepared
                .test_command
                .as_deref()
                .map(|command| parse_test_output(command, &text, is_error))
        } else {
            None
        };
        if let Some(run) = &test_run {
            if self.test_loop {
                self.session.note_test_run(run.clone());
            }
        }
        self.budget.observe_call(
            &prepared.name,
            &prepared.arguments,
            &prepared.summary,
            &text,
            is_error,
            test_run.as_ref(),
        );
        let preview = preview(&text, 160);
        if prepared.announced {
            on_event(RunEvent::ToolFinished {
                call_id: prepared.call_id.as_str(),
                name: prepared.name.as_str(),
                is_error,
                preview: &preview,
            });
        }
        (Part::tool_result(prepared.call_id, text, is_error), images)
    }
}

/// Builds the user message that carries one call's images. Captions stay in
/// front of their images so a repaired text-only request still names what was
/// omitted.
fn image_message(images: Vec<ImageAttachment>) -> Message {
    let mut message = Message::new(Role::User);
    for image in images {
        if !image.caption.trim().is_empty() {
            message.parts.push(Part::text(image.caption));
        }
        message.parts.push(Part::image_with_detail(
            image.mime,
            image.data,
            image.detail,
        ));
    }
    message
}

type ToolFutureBox = Pin<Box<dyn Future<Output = ToolOutcome> + Send>>;

/// At most this many tools from one safe run execute at once. A batch larger
/// than this is split into consecutive chunks, so results still land in call
/// order.
const MAX_CONCURRENT_TOOLS: usize = 8;

struct PreparedCall {
    call_id: ToolCallId,
    name: String,
    /// Raw arguments exactly as the model sent them. Identical calls are
    /// compared on this text, so the repeat rule needs no JSON normalization.
    arguments: String,
    /// The tool's short summary, kept for the stall diagnosis.
    summary: String,
    /// Whether `ToolStarted` was emitted for this call. Unknown tools and
    /// malformed arguments have no events, matching the sequential loop.
    announced: bool,
    outcome: Option<ToolOutcome>,
    future: Option<ToolFutureBox>,
    /// Set when the call is a `bash` command that looks like a test runner.
    /// The finished output is parsed once.
    test_command: Option<String>,
}

impl PreparedCall {
    fn immediate(call: &ToolCall, outcome: ToolOutcome, announced: bool, summary: String) -> Self {
        Self {
            call_id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            summary,
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
