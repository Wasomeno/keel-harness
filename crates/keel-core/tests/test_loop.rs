//! A synthetic bug-fix task that runs the repository's own test command.
//!
//! This is the end-to-end acceptance for the opt-in test loop: a real `bash`
//! tool runs a failing unittest, the agent edits the value, the same test
//! passes, and the harness records both runs and stops after one verification
//! prompt. It is Unix-only because the suite needs `python3` on PATH.

#![cfg(unix)]

use std::sync::Arc;

use keel_core::{Harness, RunEvent, RunStop, TestOutcome, VERIFY_PROMPT_PREFIX};
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::{Auth, Client, EnvResolver, Framing, Route};
use keel_llm_types::{Compat, ProtocolId};
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

fn tool_turn(id: &str, name: &str, arguments: &str) -> Turn {
    let arguments = serde_json::to_string(arguments).unwrap();
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

fn answer_turn(text: &str) -> Turn {
    let text = serde_json::to_string(text).unwrap();
    Turn::sse(vec![
        Chunk::text(format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{text}}},\"finish_reason\":\"stop\"}}]}}\n\n"
        )),
        Chunk::text("data: [DONE]\n\n"),
    ])
}

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[tokio::test]
async fn a_bug_fix_that_runs_the_test_passes_and_records_both_runs() {
    if !python3_available() {
        eprintln!("skipping: python3 is not on PATH");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("value.txt"), "1\n").unwrap();
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

    let command = r#"{"command":"python3 -m unittest discover -s tests -v"}"#;
    let write = json!({"path": "value.txt", "content": "2\n"}).to_string();
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "bash", command),
        tool_turn("call_2", "write", &write),
        tool_turn("call_3", "bash", command),
        answer_turn("Fixed value.txt; the test now passes; nothing unverified."),
        answer_turn("Changed value.txt to 2. Ran the unittest twice; its last run passed. Nothing unverified."),
    ])
    .await;

    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session = keel_core::Session::new(route(provider.url().to_owned()), client);
    let mut harness = Harness::new(
        session,
        Arc::new(builtin::registry()),
        Workspace::open(directory.path()).unwrap(),
    )
    .with_permissions(PermissionGate::AllowAll)
    .with_test_loop();

    let mut prompts = 0;
    let outcome = harness
        .run("make the test pass", |event| {
            if matches!(event, RunEvent::VerifyRequested { .. }) {
                prompts += 1;
            }
        })
        .await
        .unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(prompts, 1);
    let status = harness.test_status();
    assert_eq!(status.runs, 2, "the first run failed and the second passed");
    assert_eq!(status.last_outcome, Some(TestOutcome::Passed));
    assert!(
        status.failing_tests.is_empty(),
        "a later pass clears the name"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("value.txt")).unwrap(),
        "2\n"
    );
    assert_eq!(provider.request_count().await, 5);

    let history = harness.session().history();
    assert!(
        history
            .iter()
            .any(|message| message.text().starts_with(VERIFY_PROMPT_PREFIX))
    );
    let first_test_result = history
        .iter()
        .flat_map(|message| message.tool_results())
        .map(|(_, text, is_error)| (text.to_owned(), is_error))
        .next()
        .expect("the first test result");
    assert!(first_test_result.1, "the first run is an error result");
    assert!(
        first_test_result.0.contains("FAILED"),
        "the first run really failed: {}",
        first_test_result.0
    );
}
