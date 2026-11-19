//! The CLI chat path must advertise the registry's tools and run what the
//! model asks for.
//!
//! `chat` builds its harness without touching the session's tool list, so this
//! test points the NeuralWatt route at a faux provider and checks the encoded
//! request plus the tool execution that follows it. A second test covers the
//! `--resume` branch, which builds the session from a stored log.

use std::path::Path;

use keel_core::{SessionLog, SessionMeta};
use keel_testkit::{Chunk, FauxProvider, Turn};
use serde_json::Value;
use tokio::process::Command;

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

fn command(data: &Path, provider: &FauxProvider) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_keel"));
    command
        .env("KEEL_NEURALWATT_BASE_URL", provider.url())
        .env("NEURALWATT_API_KEY", "test-key")
        .env("KEEL_DATA_DIR", data);
    command
}

fn tool_names(body: &Value) -> Vec<&str> {
    body["tools"]
        .as_array()
        .expect("the request advertises tools")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect()
}

const BUILTIN_TOOLS: [&str; 6] = ["bash", "edit", "glob", "read", "search", "write"];

#[tokio::test]
async fn chat_advertises_and_runs_the_builtin_tools() {
    let write = serde_json::json!({
        "path": "hello.py",
        "content": "print('hello from keel')\n",
    })
    .to_string();
    let bash = serde_json::json!({"command": "true"}).to_string();
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "write", &write),
        tool_turn("call_2", "bash", &bash),
        answer_turn("done"),
    ])
    .await;

    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let output = command(data.path(), &provider)
        .args([
            "chat",
            "neuralwatt/deepseek-v4.1-flash",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
            "--max-turns",
            "8",
            "--prompt",
            "create hello.py",
        ])
        .output()
        .await
        .expect("run keel chat");

    assert!(
        output.status.success(),
        "keel chat failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let hello = workspace.path().join("hello.py");
    assert_eq!(
        std::fs::read_to_string(&hello).expect("the write tool created hello.py"),
        "print('hello from keel')\n"
    );

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 3, "write, bash and the final answer");
    let body = requests[0].json();
    assert_eq!(tool_names(&body), BUILTIN_TOOLS);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[write ok] wrote 25 bytes"), "{stderr}");
    assert!(stderr.contains("[bash ok] exit: 0"), "{stderr}");
}

#[tokio::test]
async fn resumed_chat_advertises_the_builtin_tools() {
    let provider = FauxProvider::start(vec![answer_turn("done")]).await;

    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let directory = keel_core::store::project_dir(data.path(), workspace.path());
    let meta = SessionMeta::new(
        "resume-test",
        "neuralwatt",
        "deepseek-v4.1-flash",
        "openai-chat",
    )
    .workspace(workspace.path().display().to_string());
    SessionLog::create(&directory, &meta).expect("seed the session log");

    let output = command(data.path(), &provider)
        .args([
            "chat",
            "--resume",
            "last",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
            "--max-turns",
            "8",
            "--prompt",
            "hello",
        ])
        .output()
        .await
        .expect("run keel chat --resume");

    assert!(
        output.status.success(),
        "keel chat --resume failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 1, "the resumed turn");
    let body = requests[0].json();
    assert_eq!(tool_names(&body), BUILTIN_TOOLS);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("resumed resume-test"), "{stderr}");
}
