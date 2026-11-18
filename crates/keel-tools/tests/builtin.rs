use std::time::Duration;

use keel_tools::{ToolContext, ToolLimits, Workspace, builtin};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn context(directory: &tempfile::TempDir) -> ToolContext {
    context_with(directory, ToolLimits::default())
}

fn context_with(directory: &tempfile::TempDir, limits: ToolLimits) -> ToolContext {
    ToolContext {
        workspace: Workspace::open(directory.path()).unwrap(),
        cancel: CancellationToken::new(),
        limits,
    }
}

async fn run(
    registry: &keel_tools::ToolRegistry,
    name: &str,
    arguments: serde_json::Value,
    context: &ToolContext,
) -> keel_tools::ToolOutcome {
    registry
        .get(name)
        .expect("tool is registered")
        .execute(arguments, context)
        .await
}

#[test]
fn builtin_specs_are_consistent() {
    let registry = builtin::registry();
    assert_eq!(registry.len(), 6);
    for name in registry.names() {
        let tool = registry.get(name).unwrap();
        let spec = tool.spec();
        assert_eq!(spec.name, name);
        assert_eq!(spec.input_schema["type"], json!("object"));
        assert!(!spec.description.is_empty());
        assert!(!tool.summarize(&json!({})).is_empty());
    }
}

#[tokio::test]
async fn read_returns_file_contents() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "first\nsecond\n").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "read", json!({"path": "a.txt"}), &context).await;

    assert!(!outcome.is_error);
    assert_eq!(outcome.text, "first\nsecond");
}

#[tokio::test]
async fn read_rejects_paths_outside_the_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "read",
        json!({"path": "../escape.txt"}),
        &context,
    )
    .await;

    assert!(outcome.is_error);
    assert!(outcome.text.contains("escapes the workspace"));
}

#[tokio::test]
async fn write_creates_directories_and_round_trips() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let written = run(
        &registry,
        "write",
        json!({"path": "nested/a.txt", "content": "hello"}),
        &context,
    )
    .await;
    assert!(!written.is_error, "{}", written.text);

    let read_back = run(&registry, "read", json!({"path": "nested/a.txt"}), &context).await;
    assert_eq!(read_back.text, "hello");
}

#[tokio::test]
async fn edit_requires_a_unique_match() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "same same").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let ambiguous = run(
        &registry,
        "edit",
        json!({"path": "a.txt", "old_string": "same", "new_string": "once"}),
        &context,
    )
    .await;
    assert!(ambiguous.is_error);
    assert!(ambiguous.text.contains("appears 2 times"));

    let replaced = run(
        &registry,
        "edit",
        json!({"path": "a.txt", "old_string": "same", "new_string": "once", "replace_all": true}),
        &context,
    )
    .await;
    assert!(!replaced.is_error, "{}", replaced.text);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "once once"
    );
}

#[tokio::test]
async fn edit_reports_a_missing_string() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "hello").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "edit",
        json!({"path": "a.txt", "old_string": "absent", "new_string": "x"}),
        &context,
    )
    .await;

    assert!(outcome.is_error);
    assert!(outcome.text.contains("was not found"));
}

#[tokio::test]
async fn bash_returns_output_and_exit_code() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "bash",
        json!({"command": "echo hello && pwd"}),
        &context,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert!(outcome.text.contains("exit: 0"));
    assert!(outcome.text.contains("hello"));
}

#[tokio::test]
async fn bash_marks_a_failing_command_as_an_error() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "bash",
        json!({"command": "echo boom >&2; exit 3"}),
        &context,
    )
    .await;

    assert!(outcome.is_error);
    assert!(outcome.text.contains("exit: 3"));
    assert!(outcome.text.contains("boom"));
}

#[tokio::test]
async fn bash_times_out() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context_with(
        &directory,
        ToolLimits {
            timeout: Duration::from_millis(100),
            ..ToolLimits::default()
        },
    );

    let outcome = run(&registry, "bash", json!({"command": "sleep 5"}), &context).await;

    assert!(outcome.is_error);
    assert!(outcome.text.contains("timed out"));
}

#[tokio::test]
async fn bash_respects_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);
    context.cancel.cancel();

    let outcome = run(&registry, "bash", json!({"command": "sleep 5"}), &context).await;

    assert!(outcome.is_error);
    assert!(outcome.text.contains("cancelled"));
}

#[tokio::test]
async fn glob_matches_paths_and_basenames() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("src")).unwrap();
    std::fs::write(directory.path().join("src/main.rs"), "").unwrap();
    std::fs::write(directory.path().join("README.md"), "").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let rust_files = run(&registry, "glob", json!({"pattern": "**/*.rs"}), &context).await;
    assert!(!rust_files.is_error, "{}", rust_files.text);
    assert!(rust_files.text.contains("src/main.rs"));

    let markdown = run(&registry, "glob", json!({"pattern": "*.md"}), &context).await;
    assert!(markdown.text.contains("README.md"));
}

#[tokio::test]
async fn search_finds_matches_with_ripgrep() {
    if std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: rg is not installed");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "first\nneedle here\n").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "search", json!({"pattern": "needle"}), &context).await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert!(
        outcome.text.contains("a.txt:2:needle here"),
        "{}",
        outcome.text
    );
}

#[tokio::test]
async fn search_reports_no_matches_without_an_error() {
    if std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: rg is not installed");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "nothing here\n").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "search", json!({"pattern": "needle"}), &context).await;

    assert!(!outcome.is_error);
    assert!(outcome.text.contains("no matches"));
}
