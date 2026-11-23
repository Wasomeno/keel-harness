//! Plan mode: tool filtering (read-only tools plus `bash`), enforcement before
//! permissions for blocked tools, and dynamic mid-session toggling.

use std::path::Path;
use std::sync::Arc;

use keel_core::{
    Harness, PLAN_MODE_OFF_NOTICE, PLAN_MODE_PROMPT, RunEvent, RunStop, Session, SkillCatalog,
    plan_mode_prompt,
};
use keel_llm_protocol::openai_chat::OpenAiChat;
use keel_llm_route::{Auth, Client, EnvResolver, Framing, Route};
use keel_llm_types::{Compat, ProtocolId};
use keel_testkit::{Chunk, FauxProvider, Turn};
use keel_tools::{PermissionDecision, PermissionGate, ToolRegistry, Workspace, builtin};
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

fn tool_turn_many(calls: &[(&str, &str, &str)]) -> Turn {
    let calls = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            let arguments = serde_json::to_string(arguments).unwrap();
            format!(
                "{{\"index\":{index},\"id\":\"{id}\",\"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":{arguments}}}}}"
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    Turn::sse(vec![
        Chunk::text(format!(
            "data: {{\"model\":\"gpt-4.1\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{calls}]}},\"finish_reason\":null}}]}}\n\n"
        )),
        Chunk::text(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ),
        Chunk::text("data: [DONE]\n\n"),
    ])
}

fn tool_turn(id: &str, name: &str, arguments: &str) -> Turn {
    tool_turn_many(&[(id, name, arguments)])
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

fn build(
    endpoint: String,
    registry: ToolRegistry,
    permissions: PermissionGate,
    workspace: &Path,
) -> Harness {
    let client = Client::with_resolver(Arc::new(EnvResolver)).unwrap();
    let session = Session::new(route(endpoint), client);
    Harness::new(
        session,
        Arc::new(registry),
        Workspace::open(workspace).unwrap(),
    )
    .with_permissions(permissions)
}

fn advertised_tool_names(body: &Value) -> Vec<&str> {
    body["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect()
}

fn system_prompt(body: &Value) -> &str {
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find_map(|msg| {
            (msg["role"] == "system")
                .then(|| msg["content"].as_str())
                .flatten()
        })
        .expect("system message")
}

/// Returns false on macOS when the test runner itself is already inside a
/// Seatbelt sandbox (macOS refuses nested `sandbox_apply`), or on unsupported
/// platforms where OS sandboxing is unavailable.
#[cfg(target_os = "macos")]
fn plan_sandbox_available() -> bool {
    if !keel_tools::plan_sandbox_supported() {
        return false;
    }
    std::process::Command::new("/usr/bin/sandbox-exec")
        .args(["-p", "(version 1)(allow default)", "--", "/usr/bin/true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(target_os = "macos"))]
fn plan_sandbox_available() -> bool {
    keel_tools::plan_sandbox_supported()
}

fn expected_plan_tools(with_skill: bool) -> Vec<&'static str> {
    let mut tools = Vec::new();
    if keel_tools::plan_sandbox_supported() {
        tools.push("bash");
    }
    tools.extend(["glob", "read", "search"]);
    if with_skill {
        tools.push("skill");
    }
    tools.push("view_image");
    tools
}

#[tokio::test]
async fn plan_mode_advertises_read_only_tools_plus_bash_and_injects_prompt() {
    let provider = FauxProvider::start(vec![answer_turn("Here is my plan.")]).await;
    let directory = tempfile::tempdir().unwrap();
    let skill_dir = directory.path().join(".agents/skills/review");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: review\ndescription: Review code.\n---\nCheck edge cases.\n",
    )
    .unwrap();

    let mut registry = builtin::registry();
    let skills = SkillCatalog::discover_with_home(directory.path(), None);
    skills.register(&mut registry);

    let mut harness = build(
        provider.url().to_owned(),
        registry,
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_skills(skills)
    .with_plan_mode(true);

    assert!(harness.plan_mode());
    let outcome = harness.run("plan the refactor", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(outcome.text(), "Here is my plan.");

    let requests = provider.requests().await;
    let body = requests[0].json();
    assert_eq!(advertised_tool_names(&body), expected_plan_tools(true));
    let expected_prompt = plan_mode_prompt(keel_tools::plan_sandbox_supported());
    assert!(
        system_prompt(&body).contains(expected_prompt.trim()),
        "system prompt should include the active plan-mode prompt"
    );
}

#[tokio::test]
async fn plan_mode_omits_bash_guidance_when_bash_is_not_advertised() {
    let provider = FauxProvider::start(vec![answer_turn("Read-only plan.")]).await;
    let directory = tempfile::tempdir().unwrap();

    // A registry without `bash` exercises the exact same prompt path as
    // Windows or Linux kernels below Landlock ABI v4.
    let mut registry = ToolRegistry::new();
    registry.register(builtin::read::ReadTool);
    registry.register(builtin::glob::GlobTool);

    let mut harness = build(
        provider.url().to_owned(),
        registry,
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_plan_mode(true);

    let outcome = harness.run("plan without bash", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);

    let requests = provider.requests().await;
    let body = requests[0].json();
    assert_eq!(advertised_tool_names(&body), vec!["glob", "read"]);
    let sys = system_prompt(&body);
    assert!(sys.contains(PLAN_MODE_PROMPT.trim()), "{sys}");
    assert!(
        !sys.contains("You may also use `bash`"),
        "prompt must not tell the model to use bash when bash is absent: {sys}"
    );
}

#[tokio::test]
async fn plan_mode_allows_reads_and_blocks_file_edits_even_with_allow_all() {
    let read_args = serde_json::json!({"path": "notes.txt"}).to_string();
    let write_args = serde_json::json!({"path": "hacked.txt", "content": "nope"}).to_string();
    let edit_args =
        serde_json::json!({"path": "notes.txt", "old": "hello", "new": "bye"}).to_string();

    let provider = FauxProvider::start(vec![
        tool_turn_many(&[
            ("call_1", "read", &read_args),
            ("call_2", "write", &write_args),
            ("call_3", "edit", &edit_args),
        ]),
        answer_turn("Plan complete."),
    ])
    .await;

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "hello world\n").unwrap();

    let mut harness = build(
        provider.url().to_owned(),
        builtin::registry(),
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_plan_mode(true);

    let mut events = Vec::new();
    let outcome = harness
        .run("inspect notes and try writing", |event| {
            if let RunEvent::ToolFinished {
                call_id,
                name,
                is_error,
                ..
            } = event
            {
                events.push((call_id.to_owned(), name.to_owned(), is_error));
            }
        })
        .await
        .unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(
        events,
        vec![
            ("call_1".to_owned(), "read".to_owned(), false),
            ("call_2".to_owned(), "write".to_owned(), true),
            ("call_3".to_owned(), "edit".to_owned(), true),
        ]
    );
    assert!(!directory.path().join("hacked.txt").exists());
    assert_eq!(
        std::fs::read_to_string(directory.path().join("notes.txt")).unwrap(),
        "hello world\n"
    );

    let requests = provider.requests().await;
    let body = requests[1].json();
    let tool_msgs: Vec<(&str, &str)> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| {
            (
                m["tool_call_id"].as_str().unwrap(),
                m["content"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(tool_msgs.len(), 3);
    assert!(tool_msgs[0].1.contains("hello world"));
    assert!(tool_msgs[1].1.contains("plan mode: `write` is disabled"));
    assert!(tool_msgs[2].1.contains("plan mode: `edit` is disabled"));
}

#[tokio::test]
async fn plan_mode_bash_runs_in_read_only_sandbox_and_blocks_workspace_writes() {
    if !plan_sandbox_available() {
        return;
    }
    let read_cmd = serde_json::json!({"command": "cat notes.txt"}).to_string();
    let write_cmd = serde_json::json!({"command": "printf 'pwn' > blocked.txt"}).to_string();
    let provider = FauxProvider::start(vec![
        tool_turn_many(&[
            ("call_1", "bash", &read_cmd),
            ("call_2", "bash", &write_cmd),
        ]),
        answer_turn("inspected"),
    ])
    .await;

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "hello from sandbox\n").unwrap();

    let mut harness = build(
        provider.url().to_owned(),
        builtin::registry(),
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_plan_mode(true);

    let mut events = Vec::new();
    let outcome = harness
        .run("read via bash and try writing", |event| {
            if let RunEvent::ToolFinished {
                call_id,
                name,
                is_error,
                ..
            } = event
            {
                events.push((call_id.to_owned(), name.to_owned(), is_error));
            }
        })
        .await
        .unwrap();

    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(
        events,
        vec![
            ("call_1".to_owned(), "bash".to_owned(), false),
            ("call_2".to_owned(), "bash".to_owned(), true),
        ]
    );
    assert!(!directory.path().join("blocked.txt").exists());
}

#[tokio::test]
async fn plan_mode_blocks_mutating_tools_without_asking_for_permission() {
    let write_args = serde_json::json!({"path": "blocked.txt", "content": "x"}).to_string();
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "write", &write_args),
        answer_turn("understood"),
    ])
    .await;

    let directory = tempfile::tempdir().unwrap();
    let (gate, mut receiver) = PermissionGate::ask();
    let mut harness = build(
        provider.url().to_owned(),
        builtin::registry(),
        gate,
        directory.path(),
    )
    .with_plan_mode(true);

    let outcome = harness.run("try write", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert!(
        receiver.try_recv().is_err(),
        "plan mode must block before asking the permission handler"
    );
    assert!(!directory.path().join("blocked.txt").exists());
}

#[tokio::test]
async fn plan_mode_bash_still_checks_permissions() {
    if !plan_sandbox_available() {
        return;
    }
    let bash_args = serde_json::json!({"command": "printf 'allowed-by-user'"}).to_string();
    let provider = FauxProvider::start(vec![
        tool_turn("call_1", "bash", &bash_args),
        answer_turn("checked"),
    ])
    .await;

    let directory = tempfile::tempdir().unwrap();
    let (gate, mut receiver) = PermissionGate::ask();
    let prompter = tokio::spawn(async move {
        let request = receiver.recv().await.expect("bash asks for permission");
        assert_eq!(request.tool, "bash");
        request.respond(PermissionDecision::Allow);
    });

    let mut harness = build(
        provider.url().to_owned(),
        builtin::registry(),
        gate,
        directory.path(),
    )
    .with_plan_mode(true);

    let outcome = harness.run("inspect with bash", |_| {}).await.unwrap();
    prompter.await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);

    let requests = provider.requests().await;
    let body = requests[1].json();
    let tool_msg = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .expect("bash tool result");
    assert!(tool_msg.contains("allowed-by-user"), "{tool_msg}");
}

#[tokio::test]
async fn plan_mode_toggles_off_and_back_on_between_turns() {
    let write_ok = serde_json::json!({"path": "created.txt", "content": "allowed\n"}).to_string();
    let write_blocked =
        serde_json::json!({"path": "second.txt", "content": "blocked\n"}).to_string();

    let provider = FauxProvider::start(vec![
        answer_turn("Here is the plan."),
        tool_turn("call_1", "write", &write_ok),
        answer_turn("Created the file."),
        tool_turn("call_2", "write", &write_blocked),
        answer_turn("Back in plan mode."),
    ])
    .await;

    let directory = tempfile::tempdir().unwrap();
    let mut harness = build(
        provider.url().to_owned(),
        builtin::registry(),
        PermissionGate::AllowAll,
        directory.path(),
    )
    .with_plan_mode(true);

    // Turn 1: plan mode ON from startup.
    let outcome = harness.run("make a plan", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);

    // Turn 2: toggle plan mode OFF and execute a write.
    harness.set_plan_mode(false);
    assert!(!harness.plan_mode());
    let outcome = harness.run("execute the plan", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("created.txt")).unwrap(),
        "allowed\n"
    );

    // Turn 3: toggle plan mode back ON and verify write is blocked again.
    harness.set_plan_mode(true);
    assert!(harness.plan_mode());
    let outcome = harness.run("plan next step", |_| {}).await.unwrap();
    assert_eq!(outcome.stop, RunStop::EndTurn);
    assert!(!directory.path().join("second.txt").exists());

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 5);

    let expected_prompt = plan_mode_prompt(keel_tools::plan_sandbox_supported()).trim();

    // Request 0: plan mode ON -> plan tools + active plan-mode prompt in system.
    assert_eq!(
        advertised_tool_names(&requests[0].json()),
        expected_plan_tools(false)
    );
    assert!(system_prompt(&requests[0].json()).contains(expected_prompt));

    // Request 1: plan mode OFF -> all tools restored, plan prompt stripped
    // from system, and PLAN_MODE_OFF_NOTICE sent in messages.
    let req1 = requests[1].json();
    assert!(advertised_tool_names(&req1).contains(&"write"));
    assert!(advertised_tool_names(&req1).contains(&"bash"));
    assert!(!system_prompt(&req1).contains(expected_prompt));
    assert!(req1["messages"].to_string().contains(PLAN_MODE_OFF_NOTICE));

    // Request 3: plan mode ON again -> plan tools narrowed and downstream
    // plan-mode notice sent without mutating system prompt.
    let req3 = requests[3].json();
    assert_eq!(advertised_tool_names(&req3), expected_plan_tools(false));
    assert!(
        req3["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|msg| msg["content"].as_str())
            .any(|text| text.contains(expected_prompt)),
        "req3 messages should include active plan-mode prompt: {req3}"
    );
}
