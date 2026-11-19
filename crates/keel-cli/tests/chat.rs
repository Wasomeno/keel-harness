//! The CLI chat path must advertise the registry's tools and run what the
//! model asks for.
//!
//! `chat` builds its harness without touching the session's tool list, so this
//! test points the NeuralWatt route at a faux provider and checks the encoded
//! request plus the tool execution that follows it. A second test covers the
//! `--resume` branch, which builds the session from a stored log.

use std::path::Path;

use keel_core::{
    COMPACTION_PROMPT, DEFAULT_SYSTEM_PROMPT, SUMMARY_PREFIX, SYSTEM_PROMPT_SEPARATOR, SessionLog,
    SessionMeta,
};
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
        .env_remove("KEEL_CACHE");
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
