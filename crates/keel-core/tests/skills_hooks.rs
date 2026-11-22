//! Skills reach the model as a name and description. The body is loaded only
//! when the skill is invoked. PreToolUse hooks can allow or block `bash`.

use std::path::Path;
use std::sync::Arc;

use keel_core::{Harness, HookConfig, RunStop, Session, SkillCatalog};
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::{Auth, Client, EnvResolver, Framing, Route};
use keel_llm_types::{Compat, ProtocolId};
use keel_testkit::{Chunk, FauxProvider, Turn};
use keel_tools::{PermissionGate, ToolRegistry, Workspace, builtin};
use serde_json::Value;

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

fn system_text(body: &Value) -> &str {
    body["messages"][0]["content"]
        .as_str()
        .expect("the first message is the system prompt")
}

fn tool_result(harness: &Harness) -> (String, bool) {
    let (output, is_error) = harness
        .session()
        .history()
        .iter()
        .find_map(|message| {
            message
                .tool_results()
                .next()
                .map(|(_, output, is_error)| (output.to_owned(), is_error))
        })
        .expect("the run recorded a tool result");
    (output, is_error)
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn a_skill_is_listed_in_the_prompt_and_loading_returns_its_body() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write(
        &workspace.path().join(".claude/skills/review-diff/SKILL.md"),
        "\
---
name: review-diff
description: Review a diff for risky changes. Use when the user asks for a code review.
---

UNIQUE_SKILL_BODY always mention the risk ledger.
",
    );

    let catalog = SkillCatalog::discover_with_home(workspace.path(), Some(home.path()));
    let loaded = catalog.load("review-diff").expect("the skill is invocable");
    assert_eq!(
        loaded.body,
        "UNIQUE_SKILL_BODY always mention the risk ledger."
    );
    assert!(loaded.instructions().contains(loaded.body.as_str()));
    assert!(!loaded.description.contains("UNIQUE_SKILL_BODY"));

    let mut registry = ToolRegistry::new();
    catalog.register(&mut registry);
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "skill", r#"{"skill":"review-diff"}"#),
        answer_turn("done"),
    ])
    .await;
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session = Session::new(route(provider.url().to_owned()), client);
    let mut harness = Harness::new(
        session,
        Arc::new(registry),
        Workspace::open(workspace.path()).unwrap(),
    )
    .with_permissions(PermissionGate::AllowAll)
    .with_skills(catalog);

    let outcome = harness.run("review the diff", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);

    let requests = provider.requests().await;
    let body = requests[0].json();
    let system = system_text(&body);
    assert!(
        system.contains("Review a diff for risky changes"),
        "the description must reach the model prompt:\n{system}"
    );
    assert!(
        !system.contains("UNIQUE_SKILL_BODY"),
        "the body stays unloaded until the skill is invoked:\n{system}"
    );

    let (output, is_error) = tool_result(&harness);
    assert!(!is_error, "{output}");
    assert!(
        output.contains("UNIQUE_SKILL_BODY always mention the risk ledger."),
        "invoking the skill returns the SKILL.md body:\n{output}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_pre_tool_hook_that_exits_zero_allows_bash() {
    let workspace = tempfile::tempdir().unwrap();
    write(
        &workspace.path().join("hooks/allow.sh"),
        "#!/bin/sh\nexit 0\n",
    );
    write(
        &workspace.path().join("hooks/after.sh"),
        "#!/bin/sh\ntouch post-hook-ran\nexit 0\n",
    );
    write(
        &workspace.path().join(".claude/settings.json"),
        r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "/bin/sh hooks/allow.sh"}]
                }],
                "PostToolUse": [{
                    "matcher": "^Bash$",
                    "hooks": [{"type": "command", "command": "/bin/sh hooks/after.sh"}]
                }],
                "SessionStart": [{
                    "hooks": [{"type": "command", "command": "/bin/sh -c 'echo SESSION_HOOK_MARKER'"}]
                }]
            }
        }"#,
    );

    let hooks = HookConfig::load(workspace.path());
    assert!(hooks.warnings().is_empty(), "{:?}", hooks.warnings());
    let registry = builtin::registry();
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "bash", r#"{"command":"echo keel-hook-ok"}"#),
        answer_turn("done"),
    ])
    .await;
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session = Session::new(route(provider.url().to_owned()), client);
    let mut harness = Harness::new(
        session,
        Arc::new(registry),
        Workspace::open(workspace.path()).unwrap(),
    )
    .with_permissions(PermissionGate::AllowAll)
    .with_hooks(hooks);

    let outcome = harness.run("say ok", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);
    let (output, is_error) = tool_result(&harness);
    assert!(!is_error, "{output}");
    assert!(
        output.contains("keel-hook-ok"),
        "an exit-0 PreToolUse hook lets bash run:\n{output}"
    );
    assert!(
        workspace.path().join("post-hook-ran").is_file(),
        "PostToolUse ran after bash"
    );

    let requests = provider.requests().await;
    let body = requests[0].json();
    let system = system_text(&body);
    assert!(
        system.contains("SESSION_HOOK_MARKER"),
        "SessionStart stdout reaches the prompt:\n{system}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_pre_tool_hook_that_exits_nonzero_blocks_the_tool_and_names_the_hook() {
    let workspace = tempfile::tempdir().unwrap();
    write(
        &workspace.path().join("hooks/block.sh"),
        "#!/bin/sh\necho named-hook-block >&2\nexit 1\n",
    );
    write(
        &workspace.path().join("hooks/after.sh"),
        "#!/bin/sh\ntouch post-should-not-run\nexit 0\n",
    );
    write(
        &workspace.path().join(".codex/hooks.json"),
        r#"{
            "hooks": {
                "PreToolUse": [{
                    "matcher": "^Bash$",
                    "hooks": [{"type": "command", "command": "/bin/sh hooks/block.sh"}]
                }],
                "PostToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "/bin/sh hooks/after.sh"}]
                }]
            }
        }"#,
    );

    let hooks = HookConfig::load(workspace.path());
    assert!(hooks.warnings().is_empty(), "{:?}", hooks.warnings());
    let registry = builtin::registry();
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "bash", r#"{"command":"touch should-not-exist"}"#),
        answer_turn("done"),
    ])
    .await;
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session = Session::new(route(provider.url().to_owned()), client);
    let mut harness = Harness::new(
        session,
        Arc::new(registry),
        Workspace::open(workspace.path()).unwrap(),
    )
    .with_permissions(PermissionGate::AllowAll)
    .with_hooks(hooks);

    let outcome = harness.run("touch a file", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);
    let (output, is_error) = tool_result(&harness);
    assert!(is_error, "{output}");
    assert!(
        output.contains("/bin/sh hooks/block.sh"),
        "the error names the hook:\n{output}"
    );
    assert!(
        output.contains("named-hook-block"),
        "the error includes the hook's stderr:\n{output}"
    );
    assert!(
        !workspace.path().join("should-not-exist").exists(),
        "bash did not run"
    );
    assert!(
        !workspace.path().join("post-should-not-run").exists(),
        "PostToolUse does not run when PreToolUse blocks the tool"
    );
}
