//! Offline replay: one cassette per task, real tools, real checks.

use std::path::Path;
use std::sync::Arc;

use keel_llm_route::{Client, CredentialRef, SecretResolver};
use keel_testkit::Cassette;
use keel_tools::Workspace;

use crate::Fixture;
use crate::check::run_checks;
use crate::fixture::cassettes_dir;
use crate::report::{TaskReport, TaskStatus};
use crate::runner::{Gate, MAX_TASK_TURNS, build_harness, drive, eval_route, outcome_status};
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

/// Replays one task from its committed cassette and runs its checks.
pub async fn replay_task(task: &Task) -> Result<TaskReport> {
    replay_cassette(task, &cassette_path(task)).await
}

/// Replays one task from an explicit cassette path. Live recording uses this
/// to prove a freshly saved cassette replays before it replaces the baseline.
pub(crate) async fn replay_cassette(task: &Task, path: &Path) -> Result<TaskReport> {
    let fixture = Fixture::prepare(task)?;
    let cassette = Cassette::load(path)?;
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

    let base_status = outcome_status(&outcome, &check);
    let error = match (outcome.error, cassette_error) {
        (Some(error), Some(cassette)) => Some(format!("{error}; {cassette}")),
        (Some(error), None) => Some(error),
        (None, Some(cassette)) => Some(cassette),
        (None, None) => None,
    };
    let status = if error.is_some() || outcome.stopped_budget {
        TaskStatus::Failed
    } else {
        base_status
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

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;

    use super::*;
    use crate::task::{Check, Task};

    const PASSING_CHECKS: &[Check] = &[Check::Command {
        script: "always_pass.py",
    }];

    fn temp_cassette(value: &Value) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cassette.json");
        fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn an_extra_cassette_exchange_fails_the_replay() {
        let task = crate::task_by_id("truncate").unwrap();
        let source = cassettes_dir().join(task.cassette_file());
        let mut value: Value = serde_json::from_str(&fs::read_to_string(source).unwrap()).unwrap();
        let interactions = value["interactions"].as_array_mut().unwrap();
        let extra = interactions[0].clone();
        interactions.push(extra);
        let (_dir, path) = temp_cassette(&value);

        let report = replay_cassette(task, &path).await.unwrap();

        assert_eq!(report.status, TaskStatus::Failed);
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|error| error.contains("unconsumed interaction")),
            "the leftover exchange is reported: {:?}",
            report.error
        );
    }

    #[tokio::test]
    async fn a_failed_model_turn_fails_replay_even_when_the_check_passes() {
        let base = crate::task_by_id("truncate").unwrap();
        let task = Task {
            id: base.id,
            prompt: base.prompt,
            check_script: "always_pass.py",
            checks: PASSING_CHECKS,
            cassette: base.cassette,
        };
        let source = cassettes_dir().join(task.cassette_file());
        let mut value: Value = serde_json::from_str(&fs::read_to_string(source).unwrap()).unwrap();
        value["interactions"].as_array_mut().unwrap().truncate(1);
        value["interactions"][0]["response"]["body"] =
            Value::String("data: {\"error\":{\"message\":\"boom\"}}\n\n".to_owned());
        let (_dir, path) = temp_cassette(&value);

        let report = replay_cassette(&task, &path).await.unwrap();

        assert!(report.check.passed, "the check itself passes");
        assert_eq!(report.stop, Some(keel_core::RunStop::Failed));
        assert_eq!(report.status, TaskStatus::Failed);
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|error| error.contains("failed")),
            "the failed stop is reported: {:?}",
            report.error
        );
    }
}
