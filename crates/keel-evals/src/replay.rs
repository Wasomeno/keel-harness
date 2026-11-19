//! Offline replay: one cassette per task, real tools, real checks.

use std::sync::Arc;

use keel_llm_route::{Client, CredentialRef, SecretResolver};
use keel_testkit::Cassette;
use keel_tools::Workspace;

use crate::Fixture;
use crate::check::run_checks;
use crate::fixture::cassettes_dir;
use crate::report::{TaskReport, TaskStatus};
use crate::runner::{Gate, MAX_TASK_TURNS, build_harness, drive, eval_route};
use crate::task::Task;
use crate::{EvalError, Result};

/// Replay auth never leaves loopback. The cassette matches method, path and
/// body only, so the placeholder value exercises the real auth path without a
/// secret.
#[derive(Debug)]
struct ReplayResolver;

impl SecretResolver for ReplayResolver {
    fn resolve(&self, _reference: &CredentialRef) -> Option<String> {
        Some("replay-key".to_owned())
    }
}

pub fn cassette_path(task: &Task) -> std::path::PathBuf {
    cassettes_dir().join(task.cassette_file())
}

/// Replays one task from its cassette and runs its checks.
pub async fn replay_task(task: &Task) -> Result<TaskReport> {
    let fixture = Fixture::prepare(task)?;
    let path = cassette_path(task);
    let cassette = Cassette::load(&path)?;
    if cassette.name != task.cassette {
        return Err(EvalError::Config(format!(
            "{}: cassette name {:?} does not match task {:?}",
            path.display(),
            cassette.name,
            task.cassette
        )));
    }

    let server = cassette.start().await;
    let route = eval_route(format!("{}/chat/completions", server.url()))?;
    let client = Client::with_resolver(Arc::new(ReplayResolver))?;
    let workspace = Workspace::open(fixture.root())?;
    let mut harness = build_harness(route, client, workspace, fixture.raw_root());

    let mut gate = Gate::new(None);
    let outcome = drive(&mut harness, task.prompt, MAX_TASK_TURNS, &mut gate).await;
    let check = run_checks(task, fixture.root());

    let misses = server.misses();
    let remaining = server.remaining();
    let cassette_error = if let Some(miss) = misses.first() {
        Some(format!(
            "cassette miss: {}",
            miss.closest
                .as_deref()
                .unwrap_or("the request did not match the next interaction")
        ))
    } else if remaining > 0 {
        Some(format!(
            "cassette has {remaining} unconsumed interaction(s) of {}",
            cassette.interactions.len()
        ))
    } else {
        None
    };

    let error = match (outcome.error, cassette_error) {
        (Some(error), Some(cassette)) => Some(format!("{error}; {cassette}")),
        (Some(error), None) => Some(error),
        (None, Some(cassette)) => Some(cassette),
        (None, None) => None,
    };
    let status = if outcome.stopped_budget || error.is_some() || !check.passed {
        TaskStatus::Failed
    } else {
        TaskStatus::Passed
    };

    Ok(TaskReport {
        id: task.id.to_owned(),
        status,
        turns: outcome.turns,
        tool_calls: outcome.tool_calls,
        usage: outcome.usage,
        stop: outcome.stop,
        check,
        spend_micros: None,
        cassette: Some(task.cassette.to_owned()),
        error,
    })
}
