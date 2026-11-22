//! Budget discipline: progress accounting, the three stall shapes, the soft
//! turn reserve, the premature-finish guard and honest stop-reason labels.
//!
//! These tests drive the real `Harness` through a scripted provider. Tool
//! execution is real, so the workspace scan, mutation counters, read results
//! and visible-test parsing are exercised end to end rather than mocked.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{
    BudgetConfig, Harness, PrematureReason, ProgressCounters, ProgressPhase, RESERVE_PROMPT_PREFIX,
    RunEvent, RunStop, STALL_PROMPT_PREFIX, Session, StallSignal,
};
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::{Auth, Client, EnvResolver, Framing, RetryPolicy, Route};
use keel_llm_types::{Compat, ProtocolId, Role};
use keel_testkit::{Chunk, FauxProvider, Turn};
use keel_tools::{PermissionGate, Workspace, builtin};
use serde_json::json;

fn route(endpoint: String) -> Route {
    Route {
        provider: "test".into(),
        model: "gpt-4.1".into(),
        endpoint,
        protocol: Arc::new(OpenAiChat::new(Compat::for_protocol(
            ProtocolId::OpenAiChat,
        ))),
        auth: Auth::None,
        framing: Framing::Sse,
        headers: Vec::new(),
        context_window: 0,
        max_output_tokens: 0,
    }
}

fn tool_turn(id: &str, name: &str, arguments: &serde_json::Value) -> Turn {
    // The protocol carries tool arguments as a JSON string inside the SSE
    // payload, so the object is stringified and then JSON-encoded again.
    let arguments = serde_json::to_string(&arguments.to_string()).unwrap();
    Turn::sse(vec![
        Chunk::text(format!(
            "data: {{\"model\":\"gpt-4.1\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"{id}\",\"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":{arguments}}}}}]}},\"finish_reason\":null}}]}}\n\n"
        )),
        Chunk::text(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ),
        Chunk::text("data: [DONE]\n\n"),
    ])
}

fn read_turn(id: &str, path: &str) -> Turn {
    tool_turn(id, "read", &json!({"path": path}))
}

fn write_turn(id: &str, path: &str, content: &str) -> Turn {
    tool_turn(id, "write", &json!({"path": path, "content": content}))
}

fn bash_turn(id: &str, command: &str) -> Turn {
    tool_turn(id, "bash", &json!({"command": command}))
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

fn malformed_turn() -> Turn {
    Turn::sse(vec![Chunk::text("data: {not json}\n\n")])
}

fn build(
    provider: &FauxProvider,
    workspace: &Path,
    budget: BudgetConfig,
    max_turns: usize,
) -> Harness {
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session = Session::new(route(provider.url().to_owned()), client);
    Harness::new(
        session,
        Arc::new(builtin::registry()),
        Workspace::open(workspace).unwrap(),
    )
    .with_permissions(PermissionGate::AllowAll)
    .with_max_turns(max_turns)
    .with_budget(budget)
}

fn only_no_progress(turns: usize) -> BudgetConfig {
    BudgetConfig {
        no_progress_turns: turns,
        ..BudgetConfig::default()
    }
}

fn only_repeated(calls: usize) -> BudgetConfig {
    BudgetConfig {
        repeated_calls: calls,
        ..BudgetConfig::default()
    }
}

fn only_unshrunk(runs: usize) -> BudgetConfig {
    BudgetConfig {
        unshrunk_test_runs: runs,
        ..BudgetConfig::default()
    }
}

fn only_reserve(turns: usize) -> BudgetConfig {
    BudgetConfig {
        reserve_turns: turns,
        ..BudgetConfig::default()
    }
}

fn reserve_messages(harness: &Harness) -> Vec<usize> {
    harness
        .session()
        .history()
        .iter()
        .enumerate()
        .filter(|(_, message)| message.text().starts_with(RESERVE_PROMPT_PREFIX))
        .map(|(index, _)| index)
        .collect()
}

fn assistant_messages_before(harness: &Harness, index: usize) -> usize {
    harness
        .session()
        .history()
        .iter()
        .take(index)
        .filter(|message| message.role == Role::Assistant)
        .count()
}

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[tokio::test]
async fn a_no_progress_stall_fires_at_the_threshold_and_not_one_turn_below() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "same\n").unwrap();

    // N = 3. The first read is new information and resets the idle count, so
    // turns 2-4 are the three consecutive no-progress turns.
    let provider = FauxProvider::start(vec![
        read_turn("call_1", "a.txt"),
        read_turn("call_2", "a.txt"),
        read_turn("call_3", "a.txt"),
        read_turn("call_4", "a.txt"),
        answer_turn("not reached"),
    ])
    .await;
    let mut harness = build(&provider, directory.path(), only_no_progress(3), 10);
    let outcome = harness.run("find the bug", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::Stalled(StallSignal::NoProgress));
    assert_eq!(outcome.turns, 4);
    let detail = outcome.stop_detail.expect("a stall carries a detail");
    assert!(detail.contains("no_progress"), "{detail}");
    assert!(detail.contains("Cheapest next step"), "{detail}");
    assert_eq!(provider.request_count().await, 4);
    assert!(
        harness
            .session()
            .history()
            .last()
            .unwrap()
            .text()
            .starts_with(STALL_PROMPT_PREFIX)
    );

    // One below the threshold: three idle turns, then the model ends the run.
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "same\n").unwrap();
    let provider = FauxProvider::start(vec![
        read_turn("call_1", "a.txt"),
        read_turn("call_2", "a.txt"),
        read_turn("call_3", "a.txt"),
        read_turn("call_4", "a.txt"),
        answer_turn("done"),
    ])
    .await;
    let mut harness = build(&provider, directory.path(), only_no_progress(4), 10);
    let outcome = harness.run("find the bug", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(outcome.turns, 5);
}

#[tokio::test]
async fn a_repeated_call_stall_fires_at_k_and_not_one_below() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "same\n").unwrap();

    let provider = FauxProvider::start(vec![
        read_turn("call_1", "a.txt"),
        read_turn("call_2", "a.txt"),
        read_turn("call_3", "a.txt"),
        answer_turn("not reached"),
    ])
    .await;
    let mut harness = build(&provider, directory.path(), only_repeated(3), 10);
    let outcome = harness.run("find the bug", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::Stalled(StallSignal::RepeatedCall));
    assert_eq!(outcome.turns, 3);
    let detail = outcome.stop_detail.expect("a stall carries a detail");
    assert!(detail.contains("repeated_call"), "{detail}");
    assert!(detail.contains("identical arguments"), "{detail}");

    // K = 4: three identical calls are one below the threshold.
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "same\n").unwrap();
    let provider = FauxProvider::start(vec![
        read_turn("call_1", "a.txt"),
        read_turn("call_2", "a.txt"),
        read_turn("call_3", "a.txt"),
        answer_turn("done"),
    ])
    .await;
    let mut harness = build(&provider, directory.path(), only_repeated(4), 10);
    let outcome = harness.run("find the bug", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(outcome.turns, 4);
}

#[tokio::test]
async fn an_unshrunk_test_failure_stall_fires_at_m_and_not_one_below() {
    if !python3_available() {
        eprintln!("skipping: python3 is not on PATH");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("tests")).unwrap();
    std::fs::write(
        directory.path().join("tests/test_bug.py"),
        "import unittest\n\
         from pathlib import Path\n\
         \n\
         class TestBug(unittest.TestCase):\n\
         \x20   def test_value(self):\n\
         \x20       self.assertEqual(Path(\"value.txt\").read_text().strip(), \"2\")\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("value.txt"), "1\n").unwrap();
    let command = "python3 -m unittest discover -s tests -v";

    let provider = FauxProvider::start(vec![
        bash_turn("call_1", command),
        bash_turn("call_2", command),
        bash_turn("call_3", command),
        answer_turn("not reached"),
    ])
    .await;
    let mut harness = build(&provider, directory.path(), only_unshrunk(3), 10);
    let outcome = harness.run("fix the bug", |_| {}).await.unwrap();

    assert_eq!(
        outcome.stop,
        RunStop::Stalled(StallSignal::TestFailuresUnshrunk)
    );
    assert_eq!(outcome.turns, 3);
    let detail = outcome.stop_detail.expect("a stall carries a detail");
    assert!(detail.contains("test_failures_unshrunk"), "{detail}");
    assert!(detail.contains("test_value"), "{detail}");

    // M = 4: three failing runs are one below the threshold.
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("tests")).unwrap();
    std::fs::write(
        directory.path().join("tests/test_bug.py"),
        "import unittest\n\
         from pathlib import Path\n\
         \n\
         class TestBug(unittest.TestCase):\n\
         \x20   def test_value(self):\n\
         \x20       self.assertEqual(Path(\"value.txt\").read_text().strip(), \"2\")\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("value.txt"), "1\n").unwrap();
    let provider = FauxProvider::start(vec![
        bash_turn("call_1", command),
        bash_turn("call_2", command),
        bash_turn("call_3", command),
        answer_turn("done"),
    ])
    .await;
    let mut harness = build(&provider, directory.path(), only_unshrunk(4), 10);
    let outcome = harness.run("fix the bug", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(outcome.turns, 4);
}

#[tokio::test]
async fn a_converging_run_that_mutates_early_and_investigates_is_not_stopped() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("target.txt"), "old\n").unwrap();

    let mut turns = vec![
        read_turn("call_1", "target.txt"),
        write_turn("call_2", "target.txt", "new\n"),
    ];
    for step in 1..=8 {
        turns.push(bash_turn(
            &format!("call_{}", step + 2),
            &format!("echo step-{step}"),
        ));
    }
    turns.push(answer_turn("changed target.txt; nothing unverified"));
    let provider = FauxProvider::start(turns).await;

    let budget = BudgetConfig {
        no_progress_turns: 3,
        repeated_calls: 3,
        unshrunk_test_runs: 3,
        reserve_turns: 6,
        premature_finish: false,
        expect_tests: false,
    };
    let mut harness = build(&provider, directory.path(), budget, 14);
    let outcome = harness.run("change it", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(outcome.progress.run.mutations, 1);
    assert!(outcome.progress.run.workspace_changes >= 1);
    assert!(outcome.progress.run.new_information >= 9);
    assert_eq!(outcome.progress.phase, ProgressPhase::Implementing);
    assert!(reserve_messages(&harness).is_empty());
}

#[tokio::test]
async fn the_soft_budget_message_appears_once_at_the_reserve_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let turns = (1..=7)
        .map(|step| bash_turn(&format!("call_{step}"), &format!("echo step-{step}")))
        .collect::<Vec<_>>();
    let provider = FauxProvider::start(turns).await;
    let mut harness = build(&provider, directory.path(), only_reserve(3), 7);
    let outcome = harness.run("look around", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::MaxTurns);
    assert_eq!(outcome.turns, 7);
    assert_eq!(outcome.progress.soft_budget_prompts, 1);
    let messages = reserve_messages(&harness);
    assert_eq!(messages.len(), 1, "the message is injected exactly once");
    // max_turns=7, reserve=3: after turn 4 three turns remain (not below),
    // after turn 5 two remain, so five model turns have completed.
    assert_eq!(assistant_messages_before(&harness, messages[0]), 5);
}

#[tokio::test]
async fn the_soft_budget_message_is_never_sent_after_a_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let mut turns = vec![write_turn("call_1", "done.txt", "done\n")];
    for step in 1..=5 {
        turns.push(bash_turn(
            &format!("call_{}", step + 1),
            &format!("echo check-{step}"),
        ));
    }
    let provider = FauxProvider::start(turns).await;
    let mut harness = build(&provider, directory.path(), only_reserve(3), 6);
    let outcome = harness.run("change it", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::MaxTurns);
    assert_eq!(outcome.progress.run.mutations, 1);
    assert_eq!(outcome.progress.soft_budget_prompts, 0);
    assert!(reserve_messages(&harness).is_empty());
}

#[tokio::test]
async fn a_sympy_shaped_run_stalls_before_the_cap_and_names_the_signal() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "same\n").unwrap();
    let provider = FauxProvider::start(
        (1..=6)
            .map(|step| read_turn(&format!("call_{step}"), "a.txt"))
            .collect::<Vec<_>>(),
    )
    .await;

    let budget = BudgetConfig {
        repeated_calls: 3,
        ..BudgetConfig::benchmark()
    };
    let mut harness = build(&provider, directory.path(), budget, 20);
    let outcome = harness.run("solve the task", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::Stalled(StallSignal::RepeatedCall));
    assert!(outcome.turns < 20, "the run stops before the hard cap");
    assert_eq!(outcome.progress.run.mutations, 0);
    assert_eq!(outcome.progress.run.test_runs, 0);
    let detail = outcome.stop_detail.expect("a stall carries a detail");
    assert!(detail.contains("repeated_call"), "{detail}");
}

#[tokio::test]
async fn a_sphinx_shaped_first_empty_attempt_does_not_block_the_retry() {
    if !python3_available() {
        eprintln!("skipping: python3 is not on PATH");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("tests")).unwrap();
    let provider = FauxProvider::start(vec![
        answer_turn("I could not find anything to change."),
        write_turn("call_1", "value.txt", "2\n"),
        bash_turn("call_2", "python3 -m unittest discover -s tests"),
        answer_turn("changed value.txt and ran the visible tests"),
    ])
    .await;
    let budget = BudgetConfig {
        premature_finish: true,
        expect_tests: true,
        ..BudgetConfig::default()
    };
    let mut harness = build(&provider, directory.path(), budget, 10);

    let first = harness.run("fix the bug", |_| {}).await.unwrap();
    assert_eq!(
        first.stop,
        RunStop::PrematureFinish(PrematureReason::NoChanges)
    );

    let retry = harness.run("try again", |_| {}).await.unwrap();
    assert_eq!(retry.stop, RunStop::EndTurn);
    assert_eq!(retry.progress.run.mutations, 1);
    assert_eq!(retry.progress.run.test_runs, 1);
}

#[tokio::test]
async fn an_end_turn_without_a_mutation_is_premature() {
    let directory = tempfile::tempdir().unwrap();
    let provider = FauxProvider::start(vec![answer_turn("All done.")]).await;
    let budget = BudgetConfig {
        premature_finish: true,
        ..BudgetConfig::default()
    };
    let mut harness = build(&provider, directory.path(), budget, 10);
    let outcome = harness.run("fix the bug", |_| {}).await.unwrap();

    assert_eq!(
        outcome.stop,
        RunStop::PrematureFinish(PrematureReason::NoChanges)
    );
    let detail = outcome
        .stop_detail
        .expect("premature finish carries a reason");
    assert!(detail.contains("no_changes"), "{detail}");
}

#[tokio::test]
async fn a_mutation_without_a_visible_test_is_premature_only_when_tests_are_expected() {
    let directory = tempfile::tempdir().unwrap();
    let provider = FauxProvider::start(vec![
        write_turn("call_1", "value.txt", "2\n"),
        answer_turn("done"),
    ])
    .await;
    let budget = BudgetConfig {
        premature_finish: true,
        expect_tests: true,
        ..BudgetConfig::default()
    };
    let mut harness = build(&provider, directory.path(), budget, 10);
    let outcome = harness.run("fix the bug", |_| {}).await.unwrap();

    assert_eq!(
        outcome.stop,
        RunStop::PrematureFinish(PrematureReason::NoVisibleTest)
    );
    let detail = outcome
        .stop_detail
        .expect("premature finish carries a reason");
    assert!(detail.contains("no_visible_test"), "{detail}");

    // No discovered test command: the same shape is an ordinary EndTurn.
    let directory = tempfile::tempdir().unwrap();
    let provider = FauxProvider::start(vec![
        write_turn("call_1", "value.txt", "2\n"),
        answer_turn("done"),
    ])
    .await;
    let budget = BudgetConfig {
        premature_finish: true,
        expect_tests: false,
        ..BudgetConfig::default()
    };
    let mut harness = build(&provider, directory.path(), budget, 10);
    let outcome = harness.run("fix the bug", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);
}

#[tokio::test]
async fn a_failure_or_cancellation_is_never_relabelled_premature() {
    let directory = tempfile::tempdir().unwrap();
    let provider = FauxProvider::start(vec![malformed_turn()]).await;
    let budget = BudgetConfig {
        premature_finish: true,
        ..BudgetConfig::default()
    };
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session = Session::new(route(provider.url().to_owned()), client);
    let mut harness = Harness::new(
        session,
        Arc::new(builtin::registry()),
        Workspace::open(directory.path()).unwrap(),
    )
    .with_permissions(PermissionGate::AllowAll)
    .with_max_turns(10)
    .with_budget(budget)
    .with_retry(RetryPolicy {
        max_attempts: 1,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
    });
    let outcome = harness.run("go", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::Failed);

    let directory = tempfile::tempdir().unwrap();
    let provider = FauxProvider::start(vec![answer_turn("never reached")]).await;
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session = Session::new(route(provider.url().to_owned()), client);
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let mut harness = Harness::new(
        session,
        Arc::new(builtin::registry()),
        Workspace::open(directory.path()).unwrap(),
    )
    .with_permissions(PermissionGate::AllowAll)
    .with_max_turns(10)
    .with_budget(BudgetConfig {
        premature_finish: true,
        ..BudgetConfig::default()
    })
    .with_cancellation(cancel);
    let outcome = harness.run("go", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::Cancelled);
}

#[cfg(unix)]
#[tokio::test]
async fn progress_is_one_event_per_turn_with_per_phase_counters() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("Makefile"), "test:\n\t@echo OK\n").unwrap();
    std::fs::write(directory.path().join("target.txt"), "old\n").unwrap();
    let provider = FauxProvider::start(vec![
        read_turn("call_1", "target.txt"),
        write_turn("call_2", "target.txt", "new\n"),
        bash_turn("call_3", "make test"),
        answer_turn("changed target.txt and ran the tests"),
    ])
    .await;
    let mut harness = build(&provider, directory.path(), only_no_progress(99), 10);

    let mut snapshots: Vec<ProgressCounters> = Vec::new();
    let outcome = harness
        .run("fix it", |event| {
            if let RunEvent::Progress { counters } = event {
                snapshots.push(counters);
            }
        })
        .await
        .unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(
        snapshots.len(),
        outcome.turns,
        "one event per completed turn"
    );
    let last = snapshots.last().unwrap();
    assert_eq!(last.run.turns, 4);
    assert_eq!(last.run.mutations, 1);
    assert!(last.run.workspace_changes >= 1);
    assert_eq!(last.run.test_runs, 1);
    assert_eq!(last.run.test_improvements, 1);
    assert!(last.run.new_information >= 3);
    assert_eq!(last.phase, ProgressPhase::Implementing);
    assert!(last.exploring.turns >= 1);
    assert!(last.implementing.turns >= 1);
}

#[tokio::test]
async fn a_workspace_change_from_bash_counts_even_without_a_mutation_tool() {
    let directory = tempfile::tempdir().unwrap();
    let provider = FauxProvider::start(vec![
        bash_turn("call_1", "printf 'created' > created.txt"),
        answer_turn("created the file"),
    ])
    .await;
    let mut harness = build(&provider, directory.path(), only_no_progress(99), 10);
    let outcome = harness.run("create it", |_| {}).await.unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(outcome.progress.run.mutations, 0);
    assert_eq!(outcome.progress.run.workspace_changes, 1);
}

#[tokio::test]
async fn a_task_deadline_stops_at_the_next_turn_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let provider = FauxProvider::start(vec![answer_turn("never reached")]).await;
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session = Session::new(route(provider.url().to_owned()), client);
    let mut harness = Harness::new(
        session,
        Arc::new(builtin::registry()),
        Workspace::open(directory.path()).unwrap(),
    )
    .with_permissions(PermissionGate::AllowAll)
    .with_max_turns(10)
    .with_budget_guard()
    .with_deadline(Some(Instant::now()));

    let outcome = harness.run("go", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::TaskTimeout);
    assert_eq!(outcome.turns, 0);
    assert!(outcome.stop_detail.as_deref().unwrap().contains("deadline"));
    assert_eq!(provider.request_count().await, 0);
}

#[tokio::test]
async fn every_stop_path_has_a_distinct_label_and_a_harness_cut_is_not_end_turn() {
    let table = [
        (RunStop::EndTurn, "end"),
        (RunStop::MaxTurns, "max_turns"),
        (RunStop::TaskTimeout, "task_timeout"),
        (
            RunStop::Stalled(StallSignal::NoProgress),
            "stalled_no_progress",
        ),
        (
            RunStop::Stalled(StallSignal::RepeatedCall),
            "stalled_repeated_call",
        ),
        (
            RunStop::Stalled(StallSignal::TestFailuresUnshrunk),
            "stalled_test_failures_unshrunk",
        ),
        (
            RunStop::PrematureFinish(PrematureReason::NoChanges),
            "premature_finish_no_changes",
        ),
        (
            RunStop::PrematureFinish(PrematureReason::NoVisibleTest),
            "premature_finish_no_test",
        ),
        (RunStop::Cancelled, "cancelled"),
        (RunStop::Failed, "failed"),
    ];

    let mut seen = HashSet::new();
    for (stop, label) in table {
        assert_eq!(stop.as_str(), label);
        assert!(seen.insert(label), "the label {label} is unique");
        if stop != RunStop::EndTurn {
            assert!(stop.is_harness_cut());
            assert_ne!(label, "end");
            assert_ne!(label, "end_turn");
        }
    }
}
