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
    assert_eq!(
        outcome.text,
        format!(
            "path `../escape.txt` escapes the workspace root `{}`; \
             use a workspace-relative path or an absolute path inside the root",
            context.workspace.root().display()
        )
    );
}

#[tokio::test]
async fn read_reports_a_missing_file_with_the_root() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "read", json!({"path": "missing.txt"}), &context).await;

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        format!(
            "path not found: `missing.txt` (workspace root `{}`); \
             use `glob` to list matching paths, or `write` to create the file",
            context.workspace.root().display()
        )
    );
}

#[tokio::test]
async fn read_rejects_a_directory_and_says_what_to_use_instead() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("notes")).unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "read", json!({"path": "notes"}), &context).await;

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        "`notes` is a directory; use `glob` to list the files in it, then read one"
    );
}

#[tokio::test]
async fn read_rejects_a_binary_file_and_says_what_to_use_instead() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("blob.bin"), [0u8, 1, 2]).unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "read", json!({"path": "blob.bin"}), &context).await;

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        "`blob.bin` looks like a binary file; read a text file instead, or use `bash` \
         with an available inspection command if you need its type"
    );
}

#[tokio::test]
async fn read_reports_malformed_arguments() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "read", json!({"path": 7}), &context).await;

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        "invalid arguments: invalid type: integer `7`, expected a string; \
         pass arguments as a JSON object that matches the tool's schema"
    );
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
    assert_eq!(
        ambiguous.text,
        "old_string appears 2 times in a.txt; add surrounding context so it matches \
         once, or set `replace_all` to true to replace all 2"
    );

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
    assert_eq!(
        outcome.text,
        "old_string was not found in a.txt; read the file with `read` and copy the \
         exact text, including whitespace, into old_string"
    );
}

#[tokio::test]
async fn edit_rejects_an_empty_old_string() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "hello").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "edit",
        json!({"path": "a.txt", "old_string": "", "new_string": "x"}),
        &context,
    )
    .await;

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        "invalid arguments: `old_string` must not be empty; read `a.txt` and pass \
         the exact text to replace"
    );
}

#[tokio::test]
async fn edit_reports_a_missing_file() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "edit",
        json!({"path": "missing.txt", "old_string": "a", "new_string": "b"}),
        &context,
    )
    .await;

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        format!(
            "path not found: `missing.txt` (workspace root `{}`); \
             use `glob` to list matching paths, or `write` to create the file",
            context.workspace.root().display()
        )
    );
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
    assert_eq!(
        outcome.text,
        "exit: 3\nstderr:\nboom\n\
         If this exit was not expected, read stderr and fix the command or the code, \
         then retry"
    );
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
    assert_eq!(
        outcome.text,
        "timed out after 100ms: bash was running `sleep 5`; this is the harness cap, \
         so shorten the command (raising timeout_ms will not help)"
    );
}

#[tokio::test]
async fn bash_timeout_suggests_raising_timeout_below_the_cap() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context_with(
        &directory,
        ToolLimits {
            timeout: Duration::from_secs(5),
            ..ToolLimits::default()
        },
    );

    let outcome = run(
        &registry,
        "bash",
        json!({"command": "sleep 5", "timeout_ms": 100}),
        &context,
    )
    .await;

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        "timed out after 100ms: bash was running `sleep 5`; shorten the command, \
         or raise timeout_ms (the harness cap is 5s)"
    );
}

#[tokio::test]
async fn bash_respects_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);
    context.cancel.cancel();

    let outcome = run(&registry, "bash", json!({"command": "sleep 5"}), &context).await;

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        "cancelled before it finished; the run was cancelled"
    );
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
    assert_eq!(
        outcome.text,
        "no matches for `needle`; check the pattern, `path`, and `glob` filter, \
         or use `glob` to find the file first"
    );
}

#[tokio::test]
async fn glob_reports_an_invalid_pattern_and_the_syntax_to_use() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "glob", json!({"pattern": "["}), &context).await;

    assert!(outcome.is_error);
    assert!(
        outcome
            .text
            .starts_with("invalid arguments: `[` is not a valid glob: "),
        "{}",
        outcome.text
    );
    assert!(
        outcome
            .text
            .ends_with("; use `**` to recurse and `/` to match the full relative path"),
        "{}",
        outcome.text
    );
}

#[tokio::test]
async fn glob_reports_no_matches_with_guidance() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "glob", json!({"pattern": "**/*.zzz"}), &context).await;

    assert!(!outcome.is_error);
    assert_eq!(
        outcome.text,
        "no files matched `**/*.zzz` under `.`; check the pattern (`**/*.rs` matches \
         recursively) and note that .gitignore'd or hidden files are skipped"
    );
}

#[tokio::test]
async fn glob_rejects_an_empty_pattern() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "glob", json!({"pattern": "  "}), &context).await;

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        "invalid arguments: `pattern` must not be empty; pass a glob such as `**/*.rs`"
    );
}

#[tokio::test]
async fn search_reports_a_missing_path() {
    if std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: rg is not installed");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "search",
        json!({"pattern": "needle", "path": "missing"}),
        &context,
    )
    .await;

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        format!(
            "path not found: `missing` (workspace root `{}`); \
             use `glob` to list matching paths, or `write` to create the file",
            context.workspace.root().display()
        )
    );
}

#[tokio::test]
async fn search_reports_an_invalid_regex_and_what_to_check() {
    if std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: rg is not installed");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "search", json!({"pattern": "["}), &context).await;

    assert!(outcome.is_error);
    assert!(
        outcome
            .text
            .starts_with("ripgrep rejected the search for `[`: "),
        "{}",
        outcome.text
    );
    assert!(
        outcome.text.ends_with(
            "; check the regular expression and the `glob` filter, and that `path` is readable"
        ),
        "{}",
        outcome.text
    );
}

#[tokio::test]
async fn search_rejects_an_empty_pattern() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "search", json!({"pattern": ""}), &context).await;

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        "invalid arguments: `pattern` must not be empty; pass a Rust-style regular expression"
    );
}

#[tokio::test]
async fn write_reports_why_the_parent_directory_failed() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("blocker"), "not a directory").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "write",
        json!({"path": "blocker/child.txt", "content": "x"}),
        &context,
    )
    .await;

    assert!(outcome.is_error);
    assert!(
        outcome
            .text
            .starts_with("could not create directory `blocker` for `blocker/child.txt`: "),
        "{}",
        outcome.text
    );
    assert!(
        outcome
            .text
            .ends_with("; check permissions, or write to a path whose parent already exists"),
        "{}",
        outcome.text
    );
}

#[test]
fn every_builtin_description_says_when_returns_and_how_it_fails() {
    let registry = builtin::registry();
    for name in registry.names() {
        let description = registry.get(name).unwrap().spec().description;
        for section in ["Use when:", "Prefer:", "Returns:", "On failure:"] {
            assert!(
                description.contains(section),
                "{name} description is missing `{section}`: {description}"
            );
        }
    }
}

#[test]
fn golden_requests_pin_the_current_read_description() {
    let description = builtin::registry().get("read").unwrap().spec().description;
    let fixtures = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../keel-llm-protocol/tests/fixtures/requests");
    let cases = [
        ("openai_chat.json", "/tools/0/function/description"),
        ("openai_responses.json", "/tools/0/description"),
        ("anthropic_messages.json", "/tools/0/description"),
    ];
    for (file, pointer) in cases {
        let text = std::fs::read_to_string(fixtures.join(file)).unwrap();
        let body: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            body.pointer(pointer).and_then(serde_json::Value::as_str),
            Some(description.as_str()),
            "{file} drifted from the built-in read description; run UPDATE_GOLDEN=1"
        );
    }
}
