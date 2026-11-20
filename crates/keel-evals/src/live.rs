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
use crate::replay::replay_cassette;
use crate::report::{TaskReport, TaskStatus};
use crate::runner::{
    DriveOutcome, Gate, MAX_TASK_TURNS, SuiteExecutor, build_harness, drive, eval_route,
    outcome_status, run_passed,
};
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
    file.is_some_and(|path| read_key_file(path).is_some())
        || std::env::var("NEURALWATT_API_KEY")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
}

/// Reads a key file only when it is private to its owner. The live runner
/// documents mode `0600`; enforcing it here keeps a group- or world-readable
/// key from being used by accident.
fn read_key_file(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).ok()?.permissions().mode();
        if mode & 0o077 != 0 {
            return None;
        }
    }
    let value = std::fs::read_to_string(path).ok()?.trim().to_owned();
    (!value.is_empty()).then_some(value)
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
            if let Some(value) = read_key_file(path) {
                return Some(value);
            }
        }
        reference
            .env
            .iter()
            .find_map(|name| std::env::var(name).ok())
            .filter(|value| !value.trim().is_empty())
    }
}

/// Replacing the committed baseline is allowed only after a clean run whose
/// check passed and which actually recorded something. `MaxTurns` is a clean
/// stop when the check passed (the format-table baseline); `Failed` and
/// `Cancelled` never are.
pub(crate) fn may_replace_cassette(
    outcome: &DriveOutcome,
    check: &CheckOutcome,
    interactions: usize,
) -> bool {
    !outcome.stopped_budget && run_passed(outcome, check) && interactions > 0
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

    /// Save to a temp file, prove it replays from a fresh fixture, then move it
    /// over the baseline. A failed or budget-stopped run never reaches this.
    async fn commit_cassette(&self, task: &Task, recorder: &Recorder) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.cassette_dir)?;
        let temp = tempfile::Builder::new()
            .prefix(".eval-")
            .suffix(".json")
            .tempfile_in(&self.cassette_dir)?;
        recorder.save(temp.path())?;

        let report = replay_cassette(task, temp.path()).await?;
        if report.status != TaskStatus::Passed {
            return Err(EvalError::Config(format!(
                "saved cassette did not replay: {}",
                report.detail()
            )));
        }

        let path = self.cassette_dir.join(task.cassette_file());
        temp.persist(&path)
            .map_err(|error| EvalError::Io(error.error))?;
        Ok(path)
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

            let replace = may_replace_cassette(&outcome, &check, recorder.interaction_count());
            let base_status = outcome_status(&outcome, &check);
            let mut error = outcome.error;
            let mut cassette = None;
            if replace {
                match self.commit_cassette(task, &recorder).await {
                    Ok(path) => cassette = Some(path.display().to_string()),
                    Err(commit_error) => error = Some(commit_error.to_string()),
                }
            }

            let status = if error.is_some() {
                TaskStatus::Failed
            } else {
                base_status
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
                error,
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
            "no private NeuralWatt key; set NEURALWATT_API_KEY or a mode-0600 \
             KEEL_RECORD_KEY_FILE"
                .to_owned(),
        ));
    }
    let mut executor = LiveExecutor::new(key_file, cassette_dir);
    let mut budget = Budget::default();
    Ok(crate::runner::run_suite(tasks, &mut executor, &mut budget, crate::Mode::Live).await)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use keel_llm_route::{Client, CredentialRef, SecretResolver};
    use keel_testkit::{Chunk, FauxProvider, Turn};

    use super::*;
    use crate::task::Check;

    const PASSING_CHECKS: &[Check] = &[Check::Command {
        script: "always_pass.py",
    }];
    const FAILING_CHECKS: &[Check] = &[Check::Command {
        script: "truncate.py",
    }];

    struct TestResolver;

    impl SecretResolver for TestResolver {
        fn resolve(&self, _reference: &CredentialRef) -> Option<String> {
            Some("test-key".to_owned())
        }
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

    async fn record_one_answer(task: &Task) -> (Recorder, FauxProvider) {
        let fixture = Fixture::prepare(task).expect("the fixture prepares");
        let provider = FauxProvider::start(vec![answer_turn("done")]).await;
        let recorder = Recorder::start(task.cassette, provider.url())
            .await
            .expect("the recorder starts");
        recorder.provider("test");
        recorder.protocol("openai-chat");
        recorder.model("test-model");
        recorder.source("commit_cassette test");

        let route = eval_route(format!("{}/chat/completions", recorder.url()))
            .expect("the eval route builds");
        let client = Client::with_resolver(Arc::new(TestResolver)).expect("the test client");
        let workspace = Workspace::open(fixture.root()).expect("the workspace opens");
        let mut harness = build_harness(route, client, workspace, fixture.raw_root());
        let mut gate = Gate::new(None);
        let outcome = drive(&mut harness, task.prompt, 1, &mut gate).await;
        assert_eq!(outcome.stop, Some(keel_core::RunStop::EndTurn));
        assert_eq!(recorder.interaction_count(), 1);
        (recorder, provider)
    }

    fn task(id: &'static str, check_script: &'static str, cassette: &'static str) -> Task {
        let checks: &'static [Check] = if check_script == "always_pass.py" {
            PASSING_CHECKS
        } else {
            FAILING_CHECKS
        };
        Task {
            id,
            prompt: "say done",
            check_script,
            checks,
            cassette,
        }
    }

    #[tokio::test]
    async fn commit_cassette_persists_a_clean_replay() {
        let task = task("always-pass", "always_pass.py", "evals/always-pass");
        let (recorder, _provider) = record_one_answer(&task).await;
        let dir = tempfile::tempdir().unwrap();
        let executor = LiveExecutor::new(None, dir.path());
        let target = dir.path().join(task.cassette_file());
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "old baseline").unwrap();

        let path = executor
            .commit_cassette(&task, &recorder)
            .await
            .expect("a clean replay persists");

        assert_eq!(path, target);
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(saved["name"], task.cassette);
        assert!(
            std::fs::read_dir(dir.path()).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".eval-")),
            "the temp file was moved, not left behind"
        );
    }

    #[tokio::test]
    async fn commit_cassette_rejects_a_replay_whose_check_fails() {
        let task = task("failing-check", "truncate.py", "evals/failing-check");
        let (recorder, _provider) = record_one_answer(&task).await;
        let dir = tempfile::tempdir().unwrap();
        let executor = LiveExecutor::new(None, dir.path());
        let target = dir.path().join(task.cassette_file());
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "old baseline").unwrap();

        let error = executor
            .commit_cassette(&task, &recorder)
            .await
            .expect_err("a failing check must reject the cassette");

        assert!(error.to_string().contains("did not replay"), "{error}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old baseline");
        assert!(
            std::fs::read_dir(dir.path()).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".eval-")),
            "the temp file was removed after the rejection"
        );
    }

    #[cfg(unix)]
    fn key_at(mode: u32) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, "test-key\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        (dir, path)
    }

    #[cfg(unix)]
    #[test]
    fn a_key_file_must_not_be_group_or_world_readable() {
        let (_dir, open) = key_at(0o644);
        assert!(read_key_file(&open).is_none());

        let (_dir, private) = key_at(0o600);
        assert_eq!(read_key_file(&private).as_deref(), Some("test-key"));
    }

    #[test]
    fn a_cassette_is_replaced_only_after_a_clean_pass() {
        let passed = CheckOutcome {
            passed: true,
            detail: "pass".to_owned(),
        };
        let failed = CheckOutcome {
            passed: false,
            detail: "check failed".to_owned(),
        };

        let clean = DriveOutcome {
            stop: Some(keel_core::RunStop::EndTurn),
            ..DriveOutcome::default()
        };
        assert!(may_replace_cassette(&clean, &passed, 1));
        assert!(!may_replace_cassette(&clean, &failed, 1));
        assert!(!may_replace_cassette(&clean, &passed, 0));

        let max_turns = DriveOutcome {
            stop: Some(keel_core::RunStop::MaxTurns),
            ..DriveOutcome::default()
        };
        assert!(may_replace_cassette(&max_turns, &passed, 1));

        for stop in [keel_core::RunStop::Failed, keel_core::RunStop::Cancelled] {
            let outcome = DriveOutcome {
                stop: Some(stop),
                ..DriveOutcome::default()
            };
            assert!(
                !may_replace_cassette(&outcome, &passed, 1),
                "{stop:?} must not replace a cassette"
            );
        }

        let no_turn = DriveOutcome::default();
        assert!(!may_replace_cassette(&no_turn, &passed, 1));

        let error = DriveOutcome {
            stop: Some(keel_core::RunStop::EndTurn),
            error: Some("decoder failed".to_owned()),
            ..DriveOutcome::default()
        };
        assert!(!may_replace_cassette(&error, &passed, 1));

        let stopped = DriveOutcome {
            stop: Some(keel_core::RunStop::MaxTurns),
            stopped_budget: true,
            ..DriveOutcome::default()
        };
        assert!(!may_replace_cassette(&stopped, &passed, 1));
    }
}
