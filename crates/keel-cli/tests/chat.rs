//! The CLI chat path must advertise the registry's tools and run what the
//! model asks for.
//!
//! `chat` builds its harness without touching the session's tool list, so this
//! test points the NeuralWatt route at a faux provider and checks the encoded
//! request plus the tool execution that follows it.

use keel_testkit::{Chunk, FauxProvider, Turn};
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

#[tokio::test]
async fn chat_advertises_and_runs_the_builtin_tools() {
    let write = serde_json::json!({
        "path": "hello.py",
        "content": "print('hello from keel')\n",
    })
    .to_string();
    let bash = serde_json::json!({"command": "python3 hello.py"}).to_string();
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "write", &write),
        tool_turn("call_2", "bash", &bash),
        answer_turn("done"),
    ])
    .await;

    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_keel"))
        .args([
            "chat",
            "neuralwatt/deepseek-v4.1-flash",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
            "--max-turns",
            "8",
            "--prompt",
            "create hello.py and run it",
        ])
        .env("KEEL_NEURALWATT_BASE_URL", provider.url())
        .env("NEURALWATT_API_KEY", "test-key")
        .env("KEEL_DATA_DIR", data.path())
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
    let tools: Vec<&str> = body["tools"]
        .as_array()
        .expect("the first request advertises tools")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert_eq!(
        tools,
        vec!["bash", "edit", "glob", "read", "search", "write"]
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[tool] write: write hello.py"), "{stderr}");
    assert!(
        stderr.contains("[tool] bash: bash: python3 hello.py"),
        "{stderr}"
    );
}
