//! The shared harness setup, the per-request budget gate, and the task suite.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use keel_core::{Harness, RunEvent, RunStop, Session};
use keel_llm_route::{Client, RetryPolicy, Route};
use keel_llm_types::{
    CachePolicy, LlmEvent, Message, Request, ToolChoice, Usage, repair_transcript,
};
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
    let registry = normalized_registry(&workspace, raw_root);
    let specs = registry.specs();
    let mut session = Session::new(route, client)
        .with_system(SYSTEM_PROMPT)
        .with_tools(specs)
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
                // `ToolStarted` fires only after a known tool is found and its
                // arguments parse. Count the authoritative model call instead,
                // so an unknown tool still appears in the report.
                if matches!(event, RunEvent::Model(LlmEvent::ToolCall { .. })) {
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

/// The request `Session::step_streaming` is about to build, before encoding.
fn preview_request(session: &Session, input: Option<&str>) -> Result<Request> {
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
    Ok(request)
}

/// The input half of the request `Session::step_streaming` is about to build.
/// One serialized byte is charged as one token, a safe upper bound for the
/// byte-fallback tokenizers these protocols use.
fn estimate_input_tokens(session: &Session, input: Option<&str>) -> Result<u64> {
    let request = preview_request(session, input)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;

    use crate::budget::NEURALWATT_DEEPSEEK_V4_1_FLASH;
    use keel_llm_route::{CredentialRef, SecretResolver};
    use keel_testkit::{Chunk, FauxProvider, Turn};
    use serde_json::Value;

    struct TestResolver;

    impl SecretResolver for TestResolver {
        fn resolve(&self, _reference: &CredentialRef) -> Option<String> {
            Some("test-key".to_owned())
        }
    }

    fn harness_for(base_url: &str, root: &std::path::Path) -> Harness {
        fs::write(root.join("README.md"), "# fixture\n").expect("fixture file");
        let route = eval_route(format!("{base_url}/chat/completions")).expect("eval route");
        let client = Client::with_resolver(Arc::new(TestResolver)).expect("test client");
        let workspace = Workspace::open(root).expect("workspace");
        build_harness(route, client, workspace, root)
    }

    fn tool_turn(id: &str, name: &str, arguments: &str) -> Turn {
        let arguments = serde_json::to_string(arguments).unwrap();
        Turn::sse(vec![
            Chunk::text(format!(
                "data: {{\"model\":\"deepseek-v4.1-flash\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"{id}\",\"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":{arguments}}}}}]}},\"finish_reason\":null}}]}}\n\n"
            )),
            Chunk::text(
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            ),
            Chunk::text("data: [DONE]\n\n"),
        ])
    }

    fn answer_turn(text: &str) -> Turn {
        let text = serde_json::to_string(text).unwrap();
        Turn::sse(vec![
            Chunk::text(format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{text}}},\"finish_reason\":\"stop\"}}]}}\n\n"
            )),
            Chunk::text("data: [DONE]\n\n"),
        ])
    }

    #[tokio::test]
    async fn the_budget_gate_stops_before_the_first_request_when_the_cap_is_one_micro_short() {
        let dir = tempfile::tempdir().unwrap();
        let provider = FauxProvider::start(Vec::<Turn>::new()).await;
        let mut harness = harness_for(provider.url(), dir.path());

        let input = Some("hello");
        let estimate = estimate_input_tokens(harness.session(), input).unwrap();
        let prices = NEURALWATT_DEEPSEEK_V4_1_FLASH;
        let worst = prices.worst_case_micros(estimate, MAX_OUTPUT_TOKENS as u64);
        assert!(worst > 0, "the request has a cost");
        let mut budget = Budget::new(prices, worst - 1);
        let mut gate = Gate::new(Some(&mut budget));

        let outcome = drive(&mut harness, "hello", 1, &mut gate).await;

        assert!(outcome.stopped_budget);
        assert_eq!(outcome.turns, 0);
        assert_eq!(outcome.tool_calls, 0);
        assert!(outcome.stop.is_none());
        assert!(outcome.error.is_none());
        assert_eq!(
            provider.request_count().await,
            0,
            "the gate must stop before the request is sent"
        );
        assert_eq!(budget.spent_micros(), 0);
    }

    #[tokio::test]
    async fn the_input_estimate_covers_the_encoded_body_with_system_and_tools() {
        let dir = tempfile::tempdir().unwrap();
        let harness = harness_for("http://127.0.0.1:9", dir.path());
        let session = harness.session();
        let input = Some("inspect the fixture and fix the failing check");

        let request = preview_request(session, input).unwrap();
        let encoded = session.route.protocol.encode(&request).unwrap();
        let bytes = serde_json::to_vec(&encoded.body).unwrap().len() as u64;
        let estimate = estimate_input_tokens(session, input).unwrap();

        assert!(
            estimate >= bytes,
            "estimate {estimate} is below the {bytes}-byte encoded body"
        );
        let tools = encoded.body["tools"]
            .as_array()
            .expect("the encoded body carries tools");
        assert!(!tools.is_empty(), "the encoded body has tool definitions");
        let messages = encoded.body["messages"]
            .as_array()
            .expect("the encoded body carries messages");
        assert_eq!(messages[0]["role"], "system");
        assert!(
            messages[0]["content"]
                .as_str()
                .is_some_and(|text| text.contains("coding agent")),
            "the system prompt is part of the request"
        );
        assert!(
            tools.iter().any(|tool| {
                tool.pointer("/function/name").and_then(Value::as_str) == Some("bash")
            }),
            "the eval tools are part of the request"
        );
    }

    #[tokio::test]
    async fn tool_calls_counts_an_unknown_tool_call() {
        let dir = tempfile::tempdir().unwrap();
        let provider = FauxProvider::start(vec![
            tool_turn("call_1", "not_a_real_tool", "{}"),
            answer_turn("done"),
        ])
        .await;
        let mut harness = harness_for(provider.url(), dir.path());
        let mut gate = Gate::new(None);

        let outcome = drive(&mut harness, "use a tool", 2, &mut gate).await;

        assert_eq!(outcome.turns, 2);
        assert_eq!(
            outcome.tool_calls, 1,
            "an unknown tool is still an authoritative model tool call"
        );
        assert_eq!(outcome.stop, Some(RunStop::EndTurn));
        assert_eq!(provider.request_count().await, 2);
    }
}
