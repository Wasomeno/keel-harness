//! The shared harness setup, the per-request budget gate, and the task suite.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use keel_core::{Harness, RunEvent, RunStop, Session};
use keel_llm_route::{Client, RetryPolicy, Route};
use keel_llm_types::{CachePolicy, Message, Request, ToolChoice, Usage, repair_transcript};
use keel_tools::{PermissionGate, ToolLimits, Workspace};

use crate::budget::Budget;
use crate::normalize::normalized_registry;
use crate::report::{Mode, RunReport, TaskReport, TaskStatus};
use crate::task::Task;
use crate::{EvalError, Result};

pub const EVAL_MODEL_LABEL: &str = "neuralwatt/deepseek-v4.1-flash";

pub(crate) const SYSTEM_PROMPT: &str = "\
You are a coding agent working in a small Python repository. Use the read, write, \
edit, glob, and search tools to inspect and change files. Use bash only to run \
`python3 tests/check.py` and short deterministic probes. Do not run `find`, `ls`, \
`stat`, `du`, or `tree`; use the glob tool to list files. Do not modify \
`tests/check.py`. When the task is complete, reply with a short summary.";

/// One model request's worst case is bounded by this output limit. The budget
/// gate uses the same number, so a request cannot cross the cap and then report
/// a larger output than was priced.
pub(crate) const MAX_OUTPUT_TOKENS: u32 = 8192;

/// The outer loop's leash. `Harness` itself runs exactly one model turn per
/// call (see [`drive`]), so this is the maximum number of model requests per
/// task.
pub(crate) const MAX_TASK_TURNS: usize = 12;

/// One task executor. Live mode records; the budget test uses a fake.
pub trait SuiteExecutor {
    fn run_task<'a>(
        &'a mut self,
        task: &'a Task,
        budget: &'a mut Budget,
    ) -> BoxFuture<'a, TaskReport>;
}

/// Runs tasks in order and marks the rest `not run (budget)` as soon as one
/// executor reports that its next request would cross the cap.
pub async fn run_suite(
    tasks: &[Task],
    executor: &mut dyn SuiteExecutor,
    budget: &mut Budget,
    mode: Mode,
) -> RunReport {
    let mut reports = Vec::with_capacity(tasks.len());
    let mut stopped = false;

    for task in tasks {
        if stopped {
            reports.push(TaskReport::not_run_budget(task));
            continue;
        }
        let report = executor.run_task(task, budget).await;
        if report.status == TaskStatus::StoppedBudget {
            stopped = true;
        }
        reports.push(report);
    }

    RunReport {
        mode,
        model: EVAL_MODEL_LABEL.to_owned(),
        cap_micros: Some(budget.cap_micros()),
        prices: budget.prices(),
        tasks: reports,
        spent_micros: Some(budget.spent_micros()),
    }
}

pub(crate) fn build_harness(
    route: Route,
    client: Client,
    workspace: Workspace,
    raw_root: &Path,
) -> Harness {
    // `Harness::new` copies the registry's specs into the session, so the
    // normalized registry needs no separate `with_tools` list. Normalization
    // changes tool results at execution time, not the advertised schemas.
    let registry = normalized_registry(&workspace, raw_root);
    let mut session = Session::new(route, client)
        .with_system(SYSTEM_PROMPT)
        .with_max_output_tokens(MAX_OUTPUT_TOKENS);
    session.retry = RetryPolicy {
        max_attempts: 1,
        ..RetryPolicy::default()
    };

    Harness::new(session, Arc::new(registry), workspace)
        .with_permissions(PermissionGate::AllowAll)
        .with_retry(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        })
        .with_max_turns(1)
        .with_limits(ToolLimits {
            timeout: Duration::from_secs(60),
            ..ToolLimits::default()
        })
}

pub(crate) enum StopBefore {
    Budget,
    Failed(String),
}

/// The budget gate. Replay passes `None` (no spend tracking); live passes the
/// run's [`Budget`].
pub(crate) struct Gate<'a> {
    budget: Option<&'a mut Budget>,
}

impl<'a> Gate<'a> {
    pub(crate) fn new(budget: Option<&'a mut Budget>) -> Self {
        Self { budget }
    }

    fn before(
        &mut self,
        harness: &Harness,
        input: Option<&str>,
    ) -> std::result::Result<(), StopBefore> {
        let Some(budget) = self.budget.as_deref_mut() else {
            return Ok(());
        };
        let session = harness.session();
        let input_tokens = estimate_input_tokens(session, input)
            .map_err(|error| StopBefore::Failed(error.to_string()))?;
        let output_tokens = session.max_output_tokens.unwrap_or(0) as u64;
        if budget.allows(input_tokens, output_tokens) {
            Ok(())
        } else {
            Err(StopBefore::Budget)
        }
    }

    fn after(&mut self, usage: &Usage) {
        if let Some(budget) = self.budget.as_deref_mut() {
            budget.charge_usage(usage);
        }
    }
}

#[derive(Default)]
pub(crate) struct DriveOutcome {
    pub turns: usize,
    pub tool_calls: usize,
    pub usage: Usage,
    pub stop: Option<RunStop>,
    pub stopped_budget: bool,
    pub error: Option<String>,
}

/// Runs one task turn at a time.
///
/// `Harness` is configured with `max_turns = 1` and retries disabled, so one
/// `harness.run` call is exactly one model request (followed by any tool
/// executions it asked for). That is what lets the gate check the budget
/// before every request instead of only between tasks.
pub(crate) async fn drive(
    harness: &mut Harness,
    prompt: &str,
    task_turns: usize,
    gate: &mut Gate<'_>,
) -> DriveOutcome {
    let mut outcome = DriveOutcome::default();
    let mut input: Option<String> = Some(prompt.to_owned());

    for _ in 0..task_turns {
        match gate.before(harness, input.as_deref()) {
            Ok(()) => {}
            Err(StopBefore::Budget) => {
                outcome.stopped_budget = true;
                return outcome;
            }
            Err(StopBefore::Failed(error)) => {
                outcome.error = Some(error);
                return outcome;
            }
        }

        let mut calls = 0usize;
        let result = harness
            .run(input.as_deref().unwrap_or(""), |event| {
                if matches!(event, RunEvent::ToolStarted { .. }) {
                    calls += 1;
                }
            })
            .await;
        outcome.tool_calls += calls;

        match result {
            Ok(run) => {
                outcome.turns += run.turns;
                outcome.usage += run.usage;
                outcome.stop = Some(run.stop);
                gate.after(&run.usage);
                match run.stop {
                    RunStop::EndTurn | RunStop::Failed | RunStop::Cancelled => return outcome,
                    RunStop::MaxTurns => input = None,
                }
            }
            Err(error) => {
                outcome.error = Some(error.to_string());
                return outcome;
            }
        }
    }

    outcome
}

/// The input half of the request `Session::step_streaming` is about to build.
/// One serialized byte is charged as one token, a safe upper bound for the
/// byte-fallback tokenizers these protocols use.
fn estimate_input_tokens(session: &Session, input: Option<&str>) -> Result<u64> {
    let pending = input
        .filter(|text| !text.trim().is_empty())
        .map(Message::user);

    let mut preview = session.history.clone();
    if let Some(message) = pending {
        preview.push(message);
    }
    let repaired = repair_transcript(preview, &session.target);

    let mut request = Request::new(session.route.model.clone())
        .messages(repaired.messages)
        .tool_choice(ToolChoice::Auto)
        .cache(CachePolicy::Auto);
    if let Some(system) = &session.system {
        request = request.system(system.clone());
    }
    if !session.tools.is_empty() {
        request = request.tools(session.tools.clone());
    }
    if let Some(max) = session.max_output_tokens {
        request = request.max_output_tokens(max);
    }
    if let Some(temperature) = session.temperature {
        request = request.temperature(temperature);
    }

    let encoded = session.route.protocol.encode(&request)?;
    let body = serde_json::to_vec(&encoded.body)?;
    Ok(body.len() as u64)
}

/// Builds the catalog route for the eval model at an explicit endpoint
/// (a recorder or cassette server).
pub(crate) fn eval_route(endpoint: String) -> Result<Route> {
    let catalog = keel_llm_catalog::Catalog::builtin();
    let (provider, model) = catalog
        .find("neuralwatt", "deepseek-v4.1-flash")
        .ok_or_else(|| {
            EvalError::Config("neuralwatt/deepseek-v4.1-flash is not catalogued".to_owned())
        })?;
    let mut route = keel_core::build_route(provider, model)?;
    route.endpoint = endpoint;
    Ok(route)
}
