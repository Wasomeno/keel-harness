//! The live recording mode.
//!
//! Opt-in, ignored by default, and refused under `CI`. It records one cassette
//! per task through the existing loopback [`Recorder`] against NeuralWatt
//! `deepseek-v4.1-flash`, with the $10 cap checked before every request.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use keel_llm_route::{Client, CredentialRef, SecretResolver};
use keel_testkit::Recorder;
use keel_tools::Workspace;

use crate::Fixture;
use crate::budget::Budget;
use crate::check::{CheckOutcome, run_checks};
use crate::report::{TaskReport, TaskStatus};
use crate::runner::{Gate, MAX_TASK_TURNS, SuiteExecutor, build_harness, drive, eval_route};
use crate::task::Task;
use crate::{EvalError, Result};

const UPSTREAM: &str = "https://api.neuralwatt.com/v1";

/// The live mode's own opt-in, in addition to the recorder's
/// `KEEL_CASSETTE_RECORD=1`.
pub const LIVE_ENV: &str = "KEEL_EVALS_LIVE";

pub fn live_allowed(opt_in: bool, in_ci: bool) -> std::result::Result<(), &'static str> {
    if in_ci {
        return Err("CI must replay evals, not record them; unset the live opt-in");
    }
    if !opt_in {
        return Err("set KEEL_EVALS_LIVE=1 to record live evals");
    }
    Ok(())
}

/// Resolve the recording key file without ever printing its value: explicit
/// argument, then `KEEL_RECORD_KEY_FILE`, then the recorder's usual
/// `~/.config/neuralwatt/key`.
pub fn resolve_key_file(explicit: Option<PathBuf>) -> Option<PathBuf> {
    explicit
        .or_else(|| std::env::var_os("KEEL_RECORD_KEY_FILE").map(PathBuf::from))
        .or_else(|| {
            let home = std::env::var_os("HOME")?;
            let path = PathBuf::from(home).join(".config/neuralwatt/key");
            path.is_file().then_some(path)
        })
}

pub fn key_available(file: Option<&Path>) -> bool {
    file.is_some_and(Path::is_file)
        || std::env::var("NEURALWATT_API_KEY")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
}

/// A resolver that prefers the key file, then the route's environment
/// reference. The key is never part of a cassette, a report, or an error.
#[derive(Debug)]
struct KeyResolver {
    file: Option<PathBuf>,
}

impl SecretResolver for KeyResolver {
    fn resolve(&self, reference: &CredentialRef) -> Option<String> {
        if let Some(path) = &self.file {
            if let Ok(value) = std::fs::read_to_string(path) {
                let value = value.trim().to_owned();
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
        reference
            .env
            .iter()
            .find_map(|name| std::env::var(name).ok())
            .filter(|value| !value.trim().is_empty())
    }
}

pub struct LiveExecutor {
    key_file: Option<PathBuf>,
    cassette_dir: PathBuf,
    source: String,
}

impl LiveExecutor {
    pub fn new(key_file: Option<PathBuf>, cassette_dir: impl Into<PathBuf>) -> Self {
        Self {
            key_file,
            cassette_dir: cassette_dir.into(),
            source: "live eval recording through keel-testkit's recorder".to_owned(),
        }
    }

    fn failed(task: &Task, error: String) -> TaskReport {
        TaskReport {
            id: task.id.to_owned(),
            status: TaskStatus::Failed,
            turns: 0,
            tool_calls: 0,
            usage: Default::default(),
            stop: None,
            check: CheckOutcome {
                passed: false,
                detail: "not run".to_owned(),
            },
            spend_micros: Some(0),
            cassette: None,
            error: Some(error),
        }
    }
}

impl SuiteExecutor for LiveExecutor {
    fn run_task<'a>(
        &'a mut self,
        task: &'a Task,
        budget: &'a mut Budget,
    ) -> BoxFuture<'a, TaskReport> {
        Box::pin(async move {
            let start_spent = budget.spent_micros();

            let fixture = match Fixture::prepare(task) {
                Ok(fixture) => fixture,
                Err(error) => return Self::failed(task, error.to_string()),
            };

            let recorder = match Recorder::start(task.cassette, UPSTREAM).await {
                Ok(recorder) => recorder,
                Err(error) => return Self::failed(task, format!("recorder: {error}")),
            };
            recorder.provider("neuralwatt");
            recorder.protocol("openai-chat");
            recorder.model("deepseek-v4.1-flash");
            recorder.source(self.source.clone());

            let route = match eval_route(format!("{}/chat/completions", recorder.url())) {
                Ok(route) => route,
                Err(error) => return Self::failed(task, error.to_string()),
            };

            let client = match Client::with_resolver(Arc::new(KeyResolver {
                file: self.key_file.clone(),
            })) {
                Ok(client) => client,
                Err(error) => return Self::failed(task, error.to_string()),
            };

            let workspace = match Workspace::open(fixture.root()) {
                Ok(workspace) => workspace,
                Err(error) => return Self::failed(task, error.to_string()),
            };

            let mut harness = build_harness(route, client, workspace, fixture.raw_root());
            let mut gate = Gate::new(Some(budget));
            let outcome = drive(&mut harness, task.prompt, MAX_TASK_TURNS, &mut gate).await;
            let check = run_checks(task, fixture.root());
            let spend_micros = budget.spent_micros().saturating_sub(start_spent);

            let mut cassette = None;
            if !outcome.stopped_budget && recorder.interaction_count() > 0 {
                let path = self.cassette_dir.join(task.cassette_file());
                match recorder.save(&path) {
                    Ok(()) => cassette = Some(path.display().to_string()),
                    Err(error) => {
                        let mut report = Self::failed(task, format!("cassette save: {error}"));
                        report.turns = outcome.turns;
                        report.tool_calls = outcome.tool_calls;
                        report.usage = outcome.usage;
                        report.stop = outcome.stop;
                        report.spend_micros = Some(spend_micros);
                        return report;
                    }
                }
            }

            let status = if outcome.stopped_budget {
                TaskStatus::StoppedBudget
            } else if outcome.error.is_none() && check.passed {
                TaskStatus::Passed
            } else {
                TaskStatus::Failed
            };

            TaskReport {
                id: task.id.to_owned(),
                status,
                turns: outcome.turns,
                tool_calls: outcome.tool_calls,
                usage: outcome.usage,
                stop: outcome.stop,
                check,
                spend_micros: Some(spend_micros),
                cassette,
                error: outcome.error,
            }
        })
    }
}

/// Records every task. The caller has already checked [`live_allowed`],
/// `CI`, and the key.
pub async fn record_all(
    tasks: &[Task],
    key_file: Option<PathBuf>,
    cassette_dir: impl Into<PathBuf>,
) -> Result<crate::RunReport> {
    let cassette_dir = cassette_dir.into();
    if !key_available(key_file.as_deref()) {
        return Err(EvalError::Config(
            "no NeuralWatt key; set NEURALWATT_API_KEY or KEEL_RECORD_KEY_FILE".to_owned(),
        ));
    }
    let mut executor = LiveExecutor::new(key_file, cassette_dir);
    let mut budget = Budget::default();
    Ok(crate::runner::run_suite(tasks, &mut executor, &mut budget, crate::Mode::Live).await)
}
