//! The CLI chat path must advertise the registry's tools and run what the
//! model asks for.
//!
//! `chat` builds its harness without touching the session's tool list, so this
//! test points the NeuralWatt route at a faux provider and checks the encoded
//! request plus the tool execution that follows it. A second test covers the
//! `--resume` branch, which builds the session from a stored log.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

#[cfg(unix)]
use keel_core::VERIFY_PROMPT_PREFIX;
use keel_core::{
    COMPACTION_PROMPT, DEFAULT_SYSTEM_PROMPT, SUMMARY_PREFIX, SYSTEM_PROMPT_SEPARATOR, SessionLog,
    SessionMeta,
};
use keel_llm_types::CachePolicy;
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

fn answer_turn_with_usage(text: &str, input: u64, output: u64) -> Turn {
    let text = serde_json::to_string(text).unwrap();
    Turn::sse(vec![
        Chunk::text(format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{text}}},\"finish_reason\":null}}]}}\n\n"
        )),
        Chunk::text(format!(
            "data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{input},\"completion_tokens\":{output},\"total_tokens\":{}}}}}\n\n",
            input + output
        )),
        Chunk::text(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ),
        Chunk::text("data: [DONE]\n\n"),
    ])
}

fn command(data: &Path, provider: &FauxProvider) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_keel"));
    command
        .env("KEEL_NEURALWATT_BASE_URL", provider.url())
        .env("NEURALWATT_API_KEY", "test-key")
        .env("KEEL_DATA_DIR", data)
        .env("KEEL_CATALOG", data.join("absent-catalog.toml"))
        .env_remove("KEEL_CACHE")
        .env_remove("KEEL_TERMINAL");
    command
}

fn chat_args(workspace: &Path, prompt: &str) -> Vec<String> {
    vec![
        "chat".to_owned(),
        "neuralwatt/deepseek-v4.1-flash".to_owned(),
        "--workspace".to_owned(),
        workspace.display().to_string(),
        "--yes".to_owned(),
        "--prompt".to_owned(),
        prompt.to_owned(),
    ]
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

fn system_text(body: &Value) -> Option<&str> {
    body["messages"].as_array()?.iter().find_map(|message| {
        (message["role"] == "system")
            .then(|| message["content"].as_str())
            .flatten()
    })
}

fn seed_session(
    data: &Path,
    workspace: &Path,
    system: Option<String>,
) -> keel_core::SessionSummary {
    let directory = keel_core::store::project_dir(data, workspace);
    let meta = SessionMeta::new(
        "resume-test",
        "neuralwatt",
        "deepseek-v4.1-flash",
        "openai-chat",
    )
    .workspace(workspace.display().to_string())
    .system(system);
    SessionLog::create(&directory, &meta).expect("seed the session log");
    keel_core::store::latest(&directory).expect("the seeded session is listed")
}

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
    assert!(stderr.contains("[tool] write:"), "{stderr}");
    assert!(stderr.contains("[tool] bash:"), "{stderr}");
    assert!(stderr.contains("[write ok] wrote 25 bytes"), "{stderr}");
    assert!(stderr.contains("[bash ok] exit: 0"), "{stderr}");
    assert!(
        !stderr.contains('\r') && !stderr.contains('\x1b'),
        "piped stderr carries no indicator control bytes: {stderr:?}"
    );
    assert!(
        !stderr.contains("waiting for model") && !stderr.contains("running bash"),
        "piped stderr carries no indicator labels: {stderr:?}"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("done"), "{stdout}");
    assert!(stdout.contains("[end] turns 3"), "{stdout}");
    assert!(
        !stdout.contains('\r') && !stdout.contains('\x1b'),
        "piped stdout carries no indicator control bytes: {stdout:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn chat_test_loop_injects_one_verification_prompt() {
    let provider = FauxProvider::start(vec![
        answer_turn("done"),
        answer_turn("Changed app.py; ran the test; nothing unverified."),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut args = chat_args(workspace.path(), "fix the bug");
    args.push("--test-loop".to_owned());

    let output = command(data.path(), &provider)
        .args(&args)
        .output()
        .await
        .expect("run keel chat");
    assert!(
        output.status.success(),
        "keel chat failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("[verify] asking for the final summary"),
        "{stderr}"
    );
    assert_eq!(provider.request_count().await, 2);

    let requests = provider.requests().await;
    let second = requests[1].json().to_string();
    assert!(
        second.contains(VERIFY_PROMPT_PREFIX),
        "the model sees the reminder: {second}"
    );
}

#[cfg(unix)]
fn failing_test_workspace(script: &str) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;

    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("test.py");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    workspace
}

#[cfg(unix)]
#[tokio::test]
async fn chat_verify_message_does_not_call_an_unparsed_failure_zero_failures() {
    let provider = FauxProvider::start(vec![
        tool_turn(
            "call_1",
            "bash",
            &serde_json::json!({"command": "./test.py"}).to_string(),
        ),
        answer_turn("I ran it."),
        answer_turn("Done."),
    ])
    .await;
    let workspace = failing_test_workspace("#!/bin/sh\necho 'FAILED (errors=1)'\nexit 1\n");
    let data = tempfile::tempdir().unwrap();
    let mut args = chat_args(workspace.path(), "fix the bug");
    args.push("--test-loop".to_owned());

    let output = command(data.path(), &provider)
        .args(&args)
        .output()
        .await
        .expect("run keel chat");
    assert!(
        output.status.success(),
        "keel chat failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("the last run failed without a parsed failure list"),
        "{stderr}"
    );
    assert!(!stderr.contains("0 failing test(s)"), "{stderr}");
}

#[cfg(unix)]
#[tokio::test]
async fn chat_verify_message_keeps_the_parsed_failure_count() {
    let provider = FauxProvider::start(vec![
        tool_turn(
            "call_1",
            "bash",
            &serde_json::json!({"command": "./test.py"}).to_string(),
        ),
        answer_turn("I ran it."),
        answer_turn("Done."),
    ])
    .await;
    let workspace =
        failing_test_workspace("#!/bin/sh\necho 'FAILED test_widget - assert 1 == 2'\nexit 1\n");
    let data = tempfile::tempdir().unwrap();
    let mut args = chat_args(workspace.path(), "fix the bug");
    args.push("--test-loop".to_owned());

    let output = command(data.path(), &provider)
        .args(&args)
        .output()
        .await
        .expect("run keel chat");
    assert!(
        output.status.success(),
        "keel chat failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("1 failing test(s)"), "{stderr}");
    assert!(
        !stderr.contains("without a parsed failure list"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn chat_with_terminal_advertises_and_runs_the_persistent_shell() {
    let setup = serde_json::json!({"command": "cd sub && export KEEL_TERMINAL_VALUE=persisted"})
        .to_string();
    let check = serde_json::json!({
        "command": "printf '%s %s\\n' \"$KEEL_TERMINAL_VALUE\" \"$PWD\" > state.txt"
    })
    .to_string();
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "terminal", &setup),
        tool_turn("call_2", "terminal", &check),
        answer_turn("done"),
    ])
    .await;

    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join("sub")).unwrap();
    let data = tempfile::tempdir().unwrap();

    let output = command(data.path(), &provider)
        .args([
            "chat",
            "neuralwatt/deepseek-v4.1-flash",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--terminal",
            "--yes",
            "--max-turns",
            "8",
            "--prompt",
            "persist shell state",
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

    let state = std::fs::read_to_string(workspace.path().join("sub/state.txt"))
        .expect("the second terminal call wrote state.txt in the persisted directory");
    assert!(
        state.contains("persisted") && state.trim_end().ends_with("/sub"),
        "shell state did not persist: {state:?}"
    );

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 3, "two terminal calls and the answer");
    assert_eq!(
        tool_names(&requests[0].json()),
        vec![
            "bash", "edit", "glob", "jobs", "read", "search", "terminal", "write"
        ]
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[tool] terminal:"), "{stderr}");
    assert!(stderr.contains("[terminal ok] exit: 0"), "{stderr}");
}

#[cfg(unix)]
#[tokio::test]
async fn chat_with_terminal_runs_a_background_job_through_the_harness() {
    let start = serde_json::json!({
        "action": "start",
        "command": "sleep 30",
        "timeout_ms": 60000
    })
    .to_string();
    let poll = serde_json::json!({"action": "poll", "job": 1}).to_string();
    let kill = serde_json::json!({"action": "kill", "job": 1}).to_string();
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "jobs", &start),
        tool_turn("call_2", "jobs", &poll),
        tool_turn("call_3", "jobs", &kill),
        answer_turn("jobs done"),
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
            "--terminal",
            "--yes",
            "--max-turns",
            "8",
            "--prompt",
            "run a job",
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

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 4, "three job calls and the answer");
    assert!(tool_names(&requests[0].json()).contains(&"jobs"));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("[tool] jobs: jobs start: sleep 30"),
        "{stderr}"
    );
    assert!(stderr.contains("[jobs ok] job: 1"), "{stderr}");
    assert!(stderr.contains("[tool] jobs: jobs kill 1"), "{stderr}");
    assert!(stderr.contains("state: killed"), "{stderr}");
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

async fn run_chat(
    provider: &FauxProvider,
    data: &Path,
    workspace: &Path,
    extra: &[&str],
) -> std::process::Output {
    let mut command = command(data, provider);
    command.args([
        "chat",
        "neuralwatt/deepseek-v4.1-flash",
        "--workspace",
        workspace.to_str().unwrap(),
        "--yes",
        "--max-turns",
        "8",
        "--prompt",
        "hello",
    ]);
    command.args(extra);
    command.output().await.expect("run keel chat")
}

#[tokio::test]
async fn a_closed_stdout_stops_the_run() {
    let provider = FauxProvider::start(vec![answer_turn("done")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let mut child = command(data.path(), &provider)
        .args([
            "chat",
            "neuralwatt/deepseek-v4.1-flash",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
            "--prompt",
            "hello",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn keel chat");
    drop(child.stdout.take());

    let output = child.wait_with_output().await.expect("wait for keel chat");
    assert!(
        !output.status.success(),
        "a closed stdout stops the run:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed printing to stdout"),
        "the write failure is reported: {stderr}"
    );
}

#[tokio::test]
async fn chat_sends_and_records_the_default_prompt() {
    let provider = FauxProvider::start(vec![answer_turn("done")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let output = run_chat(&provider, data.path(), workspace.path(), &[]).await;
    assert!(
        output.status.success(),
        "keel chat failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = provider.requests().await;
    assert_eq!(
        system_text(&requests[0].json()),
        Some(DEFAULT_SYSTEM_PROMPT),
        "the request carries the default prompt"
    );

    let directory = keel_core::store::project_dir(data.path(), workspace.path());
    let summary = keel_core::store::latest(&directory).expect("the session is logged");
    assert_eq!(
        summary.meta.system.as_deref(),
        Some(DEFAULT_SYSTEM_PROMPT),
        "the header records the prompt exactly as sent"
    );
}

#[tokio::test]
async fn chat_appends_system_text_to_the_default() {
    let provider = FauxProvider::start(vec![answer_turn("done")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let output = run_chat(
        &provider,
        data.path(),
        workspace.path(),
        &["--system", "Answer in haiku."],
    )
    .await;
    assert!(
        output.status.success(),
        "keel chat failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let expected = format!("{DEFAULT_SYSTEM_PROMPT}{SYSTEM_PROMPT_SEPARATOR}Answer in haiku.");
    let requests = provider.requests().await;
    assert_eq!(
        system_text(&requests[0].json()),
        Some(expected.as_str()),
        "--system appends after the default"
    );

    let directory = keel_core::store::project_dir(data.path(), workspace.path());
    let summary = keel_core::store::latest(&directory).expect("the session is logged");
    assert_eq!(
        summary.meta.system.as_deref(),
        Some(expected.as_str()),
        "the header records the effective prompt, not just the appended text"
    );
}

#[tokio::test]
async fn resumed_chat_keeps_the_recorded_prompt_without_the_default() {
    let provider = FauxProvider::start(vec![answer_turn("done")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    seed_session(
        data.path(),
        workspace.path(),
        Some("stored prompt".to_owned()),
    );

    let output = command(data.path(), &provider)
        .args([
            "chat",
            "--resume",
            "last",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
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
    assert_eq!(
        system_text(&requests[0].json()),
        Some("stored prompt"),
        "resume uses the header prompt as-is"
    );
}

#[tokio::test]
async fn resumed_chat_appends_system_text_to_the_recorded_prompt() {
    let provider = FauxProvider::start(vec![answer_turn("done")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    seed_session(
        data.path(),
        workspace.path(),
        Some("stored prompt".to_owned()),
    );

    let output = command(data.path(), &provider)
        .args([
            "chat",
            "--resume",
            "last",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
            "--prompt",
            "hello",
            "--system",
            "Answer in haiku.",
        ])
        .output()
        .await
        .expect("run keel chat --resume --system");
    assert!(
        output.status.success(),
        "keel chat --resume --system failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let expected = format!("stored prompt{SYSTEM_PROMPT_SEPARATOR}Answer in haiku.");
    let requests = provider.requests().await;
    assert_eq!(
        system_text(&requests[0].json()),
        Some(expected.as_str()),
        "--system appends to the stored prompt and never re-adds the default"
    );

    let directory = keel_core::store::project_dir(data.path(), workspace.path());
    let summary = keel_core::store::latest(&directory).expect("the session is still listed");
    assert_eq!(
        summary.meta.system.as_deref(),
        Some("stored prompt"),
        "the append-only header keeps the original prompt"
    );
}

#[tokio::test]
async fn a_legacy_session_without_a_prompt_sends_no_system_message() {
    let provider = FauxProvider::start(vec![answer_turn("done")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    seed_session(data.path(), workspace.path(), None);

    let output = command(data.path(), &provider)
        .args([
            "chat",
            "--resume",
            "last",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
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
    assert_eq!(
        system_text(&requests[0].json()),
        None,
        "resume does not inject the default into a legacy transcript"
    );
}

#[tokio::test]
async fn a_legacy_session_without_a_prompt_takes_system_text_alone() {
    let provider = FauxProvider::start(vec![answer_turn("done")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    seed_session(data.path(), workspace.path(), None);

    let output = command(data.path(), &provider)
        .args([
            "chat",
            "--resume",
            "last",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
            "--prompt",
            "hello",
            "--system",
            "Only this.",
        ])
        .output()
        .await
        .expect("run keel chat --resume --system");
    assert!(
        output.status.success(),
        "keel chat --resume --system failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = provider.requests().await;
    assert_eq!(
        system_text(&requests[0].json()),
        Some("Only this."),
        "a legacy session appends to nothing rather than gaining the default"
    );
}

#[tokio::test]
async fn chat_compacts_a_long_session_and_prints_one_status_line() {
    let provider = FauxProvider::start(vec![
        answer_turn_with_usage("one", 700_000, 1_000),
        answer_turn_with_usage("handoff: the first prompt is done", 120, 30),
        answer_turn_with_usage("two", 500, 20),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let first = command(data.path(), &provider)
        .args([
            "chat",
            "neuralwatt/deepseek-v4.1-flash",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
            "--prompt",
            "first",
        ])
        .output()
        .await
        .expect("run keel chat");
    assert!(
        first.status.success(),
        "keel chat failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    // The deepseek entry has a 1,048,560-token window and a 393,216-token
    // output limit, so 700,000 + 1,000 + 393,216 crosses the window.
    let second = command(data.path(), &provider)
        .args([
            "chat",
            "--resume",
            "last",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
            "--prompt",
            "second",
        ])
        .output()
        .await
        .expect("run keel chat --resume");
    assert!(
        second.status.success(),
        "keel chat --resume failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("[compact] threshold: 2 -> 2 messages (1 kept user)"),
        "one status line on stderr:\n{stderr}"
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        stdout.contains("| compact in 120 out 30"),
        "the summary usage is on the final usage line:\n{stdout}"
    );

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 3, "first turn, summary, resumed turn");
    let summary = requests[1].json();
    assert_eq!(
        summary.get("tools"),
        None,
        "the summarizer is offered no tools"
    );
    let summary_messages = summary["messages"].as_array().unwrap();
    assert_eq!(
        summary_messages.last().unwrap()["content"].as_str(),
        Some(COMPACTION_PROMPT)
    );

    let compacted = requests[2].json();
    let texts: Vec<&str> = compacted["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect();
    assert!(
        texts.iter().any(|text| text.starts_with(SUMMARY_PREFIX)),
        "{texts:?}"
    );
    assert_eq!(
        texts.iter().filter(|text| **text == "second").count(),
        1,
        "the user input is not duplicated: {texts:?}"
    );

    let directory = keel_core::store::project_dir(data.path(), workspace.path());
    let session = keel_core::store::latest(&directory).expect("the session is logged");
    let path = session.path(&directory);
    let raw = std::fs::read_to_string(&path).expect("read the log");
    assert!(
        raw.contains("\"type\":\"compaction\""),
        "the log has a compaction record"
    );
    let (_, messages) = keel_core::store::read(&path).expect("the resolved view");
    assert!(
        messages
            .iter()
            .any(|message| message.text().starts_with(SUMMARY_PREFIX))
    );
}

#[tokio::test]
async fn chat_prints_paid_summary_usage_when_the_run_fails() {
    let provider = FauxProvider::start(vec![
        answer_turn_with_usage("one", 700_000, 1_000),
        answer_turn_with_usage("handoff: paid before the failure", 120, 30),
        Turn::text(400, "invalid request"),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let first = command(data.path(), &provider)
        .args([
            "chat",
            "neuralwatt/deepseek-v4.1-flash",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
            "--prompt",
            "first",
        ])
        .output()
        .await
        .expect("run keel chat");
    assert!(
        first.status.success(),
        "keel chat failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    let second = command(data.path(), &provider)
        .args([
            "chat",
            "--resume",
            "last",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
            "--prompt",
            "second",
        ])
        .output()
        .await
        .expect("run keel chat --resume");
    assert!(
        !second.status.success(),
        "the main request failed after the summary"
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("[compact] summary usage paid before the run failed: in 120 out 30"),
        "the paid summary usage is printed before the error:\n{stderr}"
    );
    assert_eq!(
        provider.requests().await.len(),
        3,
        "first turn, summary, failed resumed turn"
    );
}

#[tokio::test]
async fn cache_defaults_to_auto_and_sends_the_openai_key() {
    let provider = FauxProvider::start(vec![answer_turn("ok")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let output = command(data.path(), &provider)
        .args(chat_args(workspace.path(), "hello"))
        .output()
        .await
        .expect("run keel chat");
    assert!(
        output.status.success(),
        "keel chat failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = provider.requests().await;
    let key = requests[0].json()["prompt_cache_key"]
        .as_str()
        .expect("auto sends prompt_cache_key")
        .to_owned();
    assert!(key.starts_with("keel-v1-"), "{key}");
}

#[tokio::test]
async fn cache_none_flag_strips_the_openai_key() {
    let provider = FauxProvider::start(vec![answer_turn("ok")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let mut args = chat_args(workspace.path(), "hello");
    args.extend(["--cache".to_owned(), "none".to_owned()]);
    let output = command(data.path(), &provider)
        .args(&args)
        .output()
        .await
        .expect("run keel chat");
    assert!(
        output.status.success(),
        "keel chat failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = provider.requests().await;
    assert!(requests[0].json().get("prompt_cache_key").is_none());
}

#[tokio::test]
async fn cache_none_from_the_environment_strips_the_openai_key() {
    let provider = FauxProvider::start(vec![answer_turn("ok")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let output = command(data.path(), &provider)
        .env("KEEL_CACHE", "none")
        .args(chat_args(workspace.path(), "hello"))
        .output()
        .await
        .expect("run keel chat");
    assert!(
        output.status.success(),
        "keel chat failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = provider.requests().await;
    assert!(requests[0].json().get("prompt_cache_key").is_none());
}

#[tokio::test]
async fn cache_flag_wins_over_the_environment() {
    let provider = FauxProvider::start(vec![answer_turn("ok")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let mut args = chat_args(workspace.path(), "hello");
    args.extend(["--cache".to_owned(), "auto".to_owned()]);
    let output = command(data.path(), &provider)
        .env("KEEL_CACHE", "none")
        .args(&args)
        .output()
        .await
        .expect("run keel chat");
    assert!(
        output.status.success(),
        "keel chat failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = provider.requests().await;
    assert!(requests[0].json().get("prompt_cache_key").is_some());
}

#[tokio::test]
async fn a_resumed_session_keeps_the_recorded_cache_policy() {
    let provider = FauxProvider::start(vec![answer_turn("ok"), answer_turn("again")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let mut first_args = chat_args(workspace.path(), "hello");
    first_args.extend(["--cache".to_owned(), "none".to_owned()]);
    let first = command(data.path(), &provider)
        .args(&first_args)
        .output()
        .await
        .expect("run keel chat");
    assert!(
        first.status.success(),
        "the first run failed:\n{}",
        String::from_utf8_lossy(&first.stderr)
    );

    let directory = keel_core::store::project_dir(data.path(), workspace.path());
    let stored = keel_core::store::latest(&directory).expect("the session was stored");
    assert_eq!(stored.meta.cache, Some(CachePolicy::None));

    let second = command(data.path(), &provider)
        .args([
            "chat",
            "--resume",
            "last",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
            "--prompt",
            "again",
        ])
        .output()
        .await
        .expect("run keel chat --resume");
    assert!(
        second.status.success(),
        "the resumed run failed:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 2, "the first turn and the resumed turn");
    assert!(
        requests[1].json().get("prompt_cache_key").is_none(),
        "resume must not re-enable the key after --cache none"
    );
}

#[tokio::test]
async fn a_cache_flag_overrides_the_policy_stored_on_resume() {
    let provider = FauxProvider::start(vec![answer_turn("ok")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let directory = keel_core::store::project_dir(data.path(), workspace.path());
    let meta = SessionMeta::new(
        "resume-cache",
        "neuralwatt",
        "deepseek-v4.1-flash",
        "openai-chat",
    )
    .workspace(workspace.path().display().to_string())
    .cache(Some(CachePolicy::None));
    SessionLog::create(&directory, &meta).expect("seed the session log");

    let output = command(data.path(), &provider)
        .args([
            "chat",
            "--resume",
            "last",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
            "--cache",
            "auto",
            "--prompt",
            "hello",
        ])
        .output()
        .await
        .expect("run keel chat --resume");
    assert!(
        output.status.success(),
        "the resumed run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = provider.requests().await;
    assert!(
        requests[0].json().get("prompt_cache_key").is_some(),
        "--cache auto overrides the stored none for this run"
    );
}

#[tokio::test]
async fn a_stalled_provider_stream_reports_a_timeout_instead_of_hanging() {
    let provider = FauxProvider::start(vec![Turn::stalled()]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let output = tokio::time::timeout(
        Duration::from_secs(10),
        command(data.path(), &provider)
            .env("KEEL_STREAM_IDLE_TIMEOUT_SECS", "1")
            .args(chat_args(workspace.path(), "hello"))
            .args(["--max-attempts", "1"])
            .output(),
    )
    .await
    .expect("the stalled stream must not hang the CLI")
    .expect("run keel chat");

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("timeout: no data from the provider for 1s"),
        "the timeout reaches the user: {text}"
    );
    assert_eq!(provider.request_count().await, 1);
}

/// A workspace with one commit, so `git status --short` is meaningful.
fn git_workspace() -> tempfile::TempDir {
    let workspace = tempfile::tempdir().unwrap();
    let git = |arguments: &[&str]| {
        let output = std::process::Command::new("git")
            .args(["-C", workspace.path().to_str().unwrap()])
            .args(arguments)
            .output()
            .expect("run git for the test workspace");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(workspace.path().join("app.py"), "value = 1\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);
    workspace
}

#[tokio::test]
async fn require_changes_fails_a_run_that_leaves_the_workspace_clean() {
    let provider = FauxProvider::start(vec![answer_turn("nothing to change")]).await;
    let workspace = git_workspace();
    let data = tempfile::tempdir().unwrap();

    let output = command(data.path(), &provider)
        .args(chat_args(workspace.path(), "change app.py"))
        .arg("--require-changes")
        .output()
        .await
        .expect("run keel chat --require-changes");

    assert!(
        !output.status.success(),
        "an unchanged workspace must fail:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(3), "the adapter reads exit 3");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--require-changes: no changes in the workspace"),
        "{stderr}"
    );
    assert!(stderr.contains("retry once"), "{stderr}");
}

#[tokio::test]
async fn require_changes_passes_when_the_run_writes_a_file() {
    let write = serde_json::json!({
        "path": "hello.py",
        "content": "print('hello from keel')\n",
    })
    .to_string();
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "write", &write),
        answer_turn("done"),
    ])
    .await;
    let workspace = git_workspace();
    let data = tempfile::tempdir().unwrap();

    let output = command(data.path(), &provider)
        .args(chat_args(workspace.path(), "create hello.py"))
        .arg("--require-changes")
        .output()
        .await
        .expect("run keel chat --require-changes");

    assert!(
        output.status.success(),
        "a new file satisfies --require-changes:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(workspace.path().join("hello.py").is_file());
}

#[tokio::test]
async fn require_changes_does_not_mask_a_failed_turn() {
    let malformed = Turn::sse(vec![Chunk::text("data: {not json}\n\n")]);
    let provider = FauxProvider::start(vec![malformed]).await;
    let workspace = git_workspace();
    let data = tempfile::tempdir().unwrap();

    let output = command(data.path(), &provider)
        .args(chat_args(workspace.path(), "change app.py"))
        .args(["--require-changes", "--max-attempts", "1"])
        .output()
        .await
        .expect("run keel chat --require-changes");

    assert!(!output.status.success());
    assert_ne!(
        output.status.code(),
        Some(3),
        "a failed turn is a real failure, not an empty run"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--require-changes: the turn failed"),
        "{stderr}"
    );
}

#[tokio::test]
async fn require_changes_needs_a_one_shot_prompt() {
    let provider = FauxProvider::start(vec![]).await;
    let workspace = git_workspace();
    let data = tempfile::tempdir().unwrap();

    let output = command(data.path(), &provider)
        .args([
            "chat",
            "neuralwatt/deepseek-v4.1-flash",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--yes",
            "--require-changes",
        ])
        .output()
        .await
        .expect("run keel chat");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--require-changes needs --prompt"),
        "{stderr}"
    );
}

#[tokio::test]
async fn an_invalid_cache_env_fails_before_a_session_is_created() {
    let provider = FauxProvider::start(vec![answer_turn("ok")]).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    let output = command(data.path(), &provider)
        .env("KEEL_CACHE", "bogus")
        .args(chat_args(workspace.path(), "hello"))
        .output()
        .await
        .expect("run keel chat");

    assert!(!output.status.success(), "a bad policy must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("KEEL_CACHE must be `auto` or `none`, got `bogus`"),
        "{stderr}"
    );
    assert!(!stderr.contains("keel: session"), "{stderr}");
    assert!(
        !keel_core::store::project_dir(data.path(), workspace.path()).exists(),
        "no session directory is created before the policy resolves"
    );
    assert!(provider.requests().await.is_empty());
}
