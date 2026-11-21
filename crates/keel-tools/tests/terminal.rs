//! The persistent terminal is opt-in and Unix-only.
//!
//! These tests drive the real tool against a real interactive bash on a
//! pseudo-terminal. They do not need a model or the network.

#![cfg(unix)]

use std::path::Path;
use std::time::Duration;

use keel_tools::{Tool, ToolContext, ToolLimits, Workspace, builtin};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use keel_tools::builtin::terminal::TerminalTool;

fn context(directory: &tempfile::TempDir) -> ToolContext {
    ToolContext {
        workspace: Workspace::open(directory.path()).unwrap(),
        cancel: CancellationToken::new(),
        limits: ToolLimits::default(),
    }
}

async fn run(tool: &TerminalTool, command: &str, context: &ToolContext) -> keel_tools::ToolOutcome {
    tool.execute(json!({"command": command}), context).await
}

#[tokio::test]
async fn cd_and_an_exported_variable_persist_across_calls() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("sub")).unwrap();
    let tool = TerminalTool::new();
    let context = context(&directory);

    let first = run(
        &tool,
        "cd sub && export KEEL_TERMINAL_VALUE=persisted",
        &context,
    )
    .await;
    assert!(!first.is_error, "{}", first.text);

    let second = run(
        &tool,
        "printf 'value=%s\\n' \"$KEEL_TERMINAL_VALUE\"; pwd",
        &context,
    )
    .await;

    assert!(!second.is_error, "{}", second.text);
    assert!(second.text.contains("value=persisted"), "{}", second.text);
    assert!(
        second.text.contains("cwd: ") && second.text.lines().any(|line| line.ends_with("/sub")),
        "the working directory did not persist:\n{}",
        second.text
    );
}

#[tokio::test]
async fn workdir_changes_the_persistent_directory() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("sub")).unwrap();
    let tool = TerminalTool::new();
    let context = context(&directory);

    let first = tool
        .execute(json!({"command": "pwd", "workdir": "sub"}), &context)
        .await;
    assert!(!first.is_error, "{}", first.text);
    assert!(
        first.text.lines().any(|line| line.ends_with("/sub")),
        "{}",
        first.text
    );

    let second = run(&tool, "pwd", &context).await;
    assert!(!second.is_error, "{}", second.text);
    assert!(
        second.text.lines().any(|line| line.ends_with("/sub")),
        "workdir did not persist:\n{}",
        second.text
    );
}

#[tokio::test]
async fn non_utf8_output_returns_a_usable_lossy_result() {
    let directory = tempfile::tempdir().unwrap();
    let tool = TerminalTool::new();
    let context = context(&directory);

    let outcome = run(&tool, "printf 'ok\\377\\376end\\n'", &context).await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert!(outcome.text.contains("ok"), "{}", outcome.text);
    assert!(outcome.text.contains("end"), "{}", outcome.text);
    assert!(
        outcome.text.contains('\u{fffd}'),
        "the invalid bytes were not replaced:\n{}",
        outcome.text
    );
}

#[tokio::test]
async fn a_cancelled_command_does_not_kill_the_session() {
    let directory = tempfile::tempdir().unwrap();
    let tool = TerminalTool::new();
    let context = context(&directory);

    let setup = run(&tool, "export KEEL_TERMINAL_MARKER=alive", &context).await;
    assert!(!setup.is_error, "{}", setup.text);

    // Mimic the harness: poll the tool future, drop it when the run is
    // cancelled. The tool's drop guard sends Ctrl-C.
    let cancel = CancellationToken::new();
    let cancel_context = ToolContext {
        workspace: Workspace::open(directory.path()).unwrap(),
        cancel: cancel.clone(),
        limits: ToolLimits::default(),
    };
    let mut future = Box::pin(tool.execute(
        json!({"command": "sleep 30; printf 'BAD\\n'"}),
        &cancel_context,
    ));
    tokio::select! {
        () = tokio::time::sleep(Duration::from_millis(400)) => {}
        outcome = &mut future => panic!("the long command ended early: {}", outcome.text),
    }
    cancel.cancel();
    drop(future);
    tokio::time::sleep(Duration::from_millis(250)).await;

    let after = run(
        &tool,
        "printf 'marker=%s\\n' \"$KEEL_TERMINAL_MARKER\"",
        &context,
    )
    .await;

    assert!(!after.is_error, "{}", after.text);
    assert!(after.text.contains("marker=alive"), "{}", after.text);
    assert!(!after.text.contains("BAD"), "{}", after.text);
}

#[tokio::test]
async fn a_command_waiting_for_input_times_out_and_the_session_survives() {
    let directory = tempfile::tempdir().unwrap();
    let tool = TerminalTool::new();
    let context = context(&directory);

    let setup = run(&tool, "export KEEP=yes", &context).await;
    assert!(!setup.is_error, "{}", setup.text);

    let interactive = tool
        .execute(
            json!({
                "command": "read -r answer; printf 'SHOULD_NOT_PRINT\\n'",
                "timeout_ms": 500
            }),
            &context,
        )
        .await;

    assert!(interactive.is_error, "{}", interactive.text);
    assert!(
        interactive.text.contains("did not finish") || interactive.text.contains("timed out"),
        "{}",
        interactive.text
    );
    assert!(
        interactive.text.contains("The session is still alive."),
        "{}",
        interactive.text
    );

    let after = run(&tool, "printf 'keep=%s\\n' \"$KEEP\"", &context).await;
    assert!(!after.is_error, "{}", after.text);
    assert!(after.text.contains("keep=yes"), "{}", after.text);
    assert!(
        !after.text.contains("SHOULD_NOT_PRINT"),
        "the intercepted command kept running:\n{}",
        after.text
    );
}

#[tokio::test]
async fn a_shell_exit_is_reported_and_the_next_call_restarts() {
    let directory = tempfile::tempdir().unwrap();
    let tool = TerminalTool::new();
    let context = context(&directory);

    let setup = run(&tool, "export GONE=yes", &context).await;
    assert!(!setup.is_error, "{}", setup.text);

    let exited = run(&tool, "exit 7", &context).await;
    assert!(exited.is_error, "{}", exited.text);
    assert!(
        exited.text.contains("shell exited"),
        "the exit was not reported:\n{}",
        exited.text
    );

    let fresh = run(&tool, "printf 'gone=%s\\n' \"${GONE-unset}\"", &context).await;
    assert!(!fresh.is_error, "{}", fresh.text);
    assert!(
        fresh.text.contains("gone=unset"),
        "the next call should have started a fresh shell:\n{}",
        fresh.text
    );
}

#[tokio::test]
async fn dropping_the_tool_leaves_no_child_process() {
    let directory = tempfile::tempdir().unwrap();
    let tool = TerminalTool::new();
    let context = context(&directory);

    let started = run(
        &tool,
        "sleep 30 & echo $! > bg.pid; echo $$ > shell.pid; echo started",
        &context,
    )
    .await;
    assert!(!started.is_error, "{}", started.text);

    let background = pid_from(&directory.path().join("bg.pid"));
    let shell = pid_from(&directory.path().join("shell.pid"));
    assert_ne!(background, shell);
    assert!(alive(background), "the background child did not start");
    assert!(alive(shell), "the shell did not start");

    tool.close().await;
    drop(tool);

    assert!(
        wait_until_gone(background),
        "the background child survived cleanup"
    );
    assert!(wait_until_gone(shell), "the shell survived cleanup");
}

#[tokio::test]
async fn the_default_registry_keeps_the_one_shot_bash_path() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("sub")).unwrap();
    let registry = builtin::registry();

    assert_eq!(
        registry.names(),
        vec!["bash", "edit", "glob", "read", "search", "write"]
    );
    assert!(
        registry.get("terminal").is_none(),
        "the terminal tool must stay opt-in"
    );

    let context = context(&directory);
    let first = registry
        .get("bash")
        .unwrap()
        .execute(
            json!({"command": "cd sub; export KEEL_ONE_SHOT=present; pwd"}),
            &context,
        )
        .await;
    assert!(!first.is_error, "{}", first.text);
    assert!(first.text.contains("/sub"), "{}", first.text);

    let second = registry
        .get("bash")
        .unwrap()
        .execute(
            json!({"command": "pwd; printf 'value=%s\\n' \"${KEEL_ONE_SHOT-unset}\""}),
            &context,
        )
        .await;
    assert!(!second.is_error, "{}", second.text);
    assert!(
        !second.text.contains("/sub"),
        "a later bash call inherited the earlier directory:\n{}",
        second.text
    );
    assert!(
        second.text.contains("value=unset"),
        "a later bash call inherited the earlier environment:\n{}",
        second.text
    );
}

fn pid_from(path: &Path) -> i32 {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
        .trim()
        .parse()
        .unwrap_or_else(|error| panic!("parsing {}: {error}", path.display()))
}

fn alive(pid: i32) -> bool {
    rustix::process::Pid::from_raw(pid)
        .map(|pid| rustix::process::test_kill_process(pid).is_ok())
        .unwrap_or(false)
}

fn wait_until_gone(pid: i32) -> bool {
    for _ in 0..100 {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}
