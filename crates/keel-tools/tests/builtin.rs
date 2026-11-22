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
    assert_eq!(registry.len(), 7);
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
    assert_eq!(outcome.text, "     1→first\n     2→second");
}

#[tokio::test]
async fn read_numbers_the_first_page_and_keeps_real_line_numbers() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("a.txt"),
        "one\ntwo\nthree\nfour\nfive\n",
    )
    .unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "read",
        json!({"path": "a.txt", "offset": 0, "limit": 2}),
        &context,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(outcome.text, "     1→one\n     2→two\n\n[lines 1-2 of 5]");
}

#[tokio::test]
async fn read_numbers_a_partial_range_with_the_original_line_numbers() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("a.txt"),
        "one\ntwo\nthree\nfour\nfive\n",
    )
    .unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "read",
        json!({"path": "a.txt", "offset": 1, "limit": 2}),
        &context,
    )
    .await;

    assert_eq!(outcome.text, "     2→two\n     3→three\n\n[lines 2-3 of 5]");
}

#[tokio::test]
async fn read_numbers_the_last_page_and_reports_its_range() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("a.txt"),
        "one\ntwo\nthree\nfour\nfive\n",
    )
    .unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "read",
        json!({"path": "a.txt", "offset": 3}),
        &context,
    )
    .await;

    assert_eq!(outcome.text, "     4→four\n     5→five\n\n[lines 4-5 of 5]");
}

#[tokio::test]
async fn read_reports_an_empty_file_without_numbering() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("empty.txt"), "").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "read", json!({"path": "empty.txt"}), &context).await;

    assert!(!outcome.is_error);
    assert_eq!(outcome.text, "[empty file]");
}

#[tokio::test]
async fn read_numbers_a_single_line_without_a_trailing_newline() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("one.txt"), "only").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "read", json!({"path": "one.txt"}), &context).await;

    assert_eq!(outcome.text, "     1→only");
}

#[tokio::test]
async fn read_does_not_mangle_content_that_begins_with_digits() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("numbers.txt"),
        "123\n456\n     1→already gutter-like\n",
    )
    .unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "read", json!({"path": "numbers.txt"}), &context).await;

    assert_eq!(
        outcome.text,
        "     1→123\n     2→456\n     3→     1→already gutter-like"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn read_refuses_a_symlink_even_inside_the_workspace() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("target.txt"), "target").unwrap();
    std::os::unix::fs::symlink("target.txt", directory.path().join("link.txt")).unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "read", json!({"path": "link.txt"}), &context).await;

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        "`link.txt` is a symbolic link; symlinks are not followed inside the workspace. \
         If you trust the target, use `bash cat 'link.txt'` to read it"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn read_quotes_a_symlink_path_in_the_bash_hint() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("target.txt"), "target").unwrap();
    std::os::unix::fs::symlink("target.txt", directory.path().join("my link's.txt")).unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "read",
        json!({"path": "my link's.txt"}),
        &context,
    )
    .await;

    assert!(outcome.is_error);
    assert!(
        outcome
            .text
            .contains("use `bash cat 'my link'\\''s.txt'` to read it"),
        "{}",
        outcome.text
    );
}

#[cfg(unix)]
#[tokio::test]
async fn write_refuses_to_replace_a_symlink() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "unchanged").unwrap();
    std::os::unix::fs::symlink(&secret, directory.path().join("link.txt")).unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "write",
        json!({"path": "link.txt", "content": "replaced"}),
        &context,
    )
    .await;

    assert!(outcome.is_error);
    assert!(outcome.text.contains("symbolic link"), "{}", outcome.text);
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "unchanged");
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
async fn directory_messages_keep_the_requested_spelling() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("notes")).unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let read = run(&registry, "read", json!({"path": "./notes"}), &context).await;
    assert_eq!(
        read.text,
        "`./notes` is a directory; use `glob` to list the files in it, then read one"
    );

    let edit = run(
        &registry,
        "edit",
        json!({"path": "./notes", "old_string": "a", "new_string": "b"}),
        &context,
    )
    .await;
    assert_eq!(edit.text, "`./notes` is a directory; choose a file to edit");

    let write = run(
        &registry,
        "write",
        json!({"path": "./notes", "content": "x"}),
        &context,
    )
    .await;
    assert_eq!(write.text, "`./notes` is a directory; choose a file path");
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
    assert_eq!(written.text, "wrote 5 bytes to nested/a.txt");

    let read_back = run(&registry, "read", json!({"path": "nested/a.txt"}), &context).await;
    assert_eq!(read_back.text, "     1→hello");
}

#[tokio::test]
async fn write_strips_a_uniform_numbered_block_and_reports_it() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);
    let numbered = "     1→alpha\n     2→beta";

    let outcome = run(
        &registry,
        "write",
        json!({"path": "a.txt", "content": numbered}),
        &context,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(
        outcome.text,
        "wrote 10 bytes to a.txt (removed the read line-number gutter)"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "alpha\nbeta"
    );
}

#[tokio::test]
async fn write_keeps_mixed_content_verbatim() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);
    let mixed = "     1→alpha\nplain line\n";

    let outcome = run(
        &registry,
        "write",
        json!({"path": "a.txt", "content": mixed}),
        &context,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(outcome.text, "wrote 26 bytes to a.txt");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        mixed
    );
}

#[tokio::test]
async fn write_keeps_a_single_narrow_numbered_line_verbatim() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "write",
        json!({"path": "a.txt", "content": "12→x"}),
        &context,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(outcome.text, "wrote 6 bytes to a.txt");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "12→x"
    );
}

#[tokio::test]
async fn edit_accepts_a_block_copied_from_read_with_its_gutter() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let read = run(
        &registry,
        "read",
        json!({"path": "a.txt", "offset": 1, "limit": 2}),
        &context,
    )
    .await;
    assert_eq!(read.text, "     2→beta\n     3→gamma\n\n[lines 2-3 of 3]");

    let outcome = run(
        &registry,
        "edit",
        json!({
            "path": "a.txt",
            "old_string": "     2→beta\n     3→gamma",
            "new_string": "     2→BETA\n     3→GAMMA"
        }),
        &context,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(outcome.text, "replaced 1 occurrence(s) in a.txt");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "alpha\nBETA\nGAMMA\n"
    );
}

#[tokio::test]
async fn edit_still_matches_literal_text_that_looks_numbered() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "     1→literal\n").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "edit",
        json!({
            "path": "a.txt",
            "old_string": "     1→literal",
            "new_string": "changed"
        }),
        &context,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "changed\n"
    );
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
async fn edit_reports_a_directory_without_read_advice() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("notes")).unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "edit",
        json!({"path": "notes", "old_string": "a", "new_string": "b"}),
        &context,
    )
    .await;

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        "`notes` is a directory; choose a file to edit"
    );
}

#[tokio::test]
async fn edit_reports_a_read_only_file_as_not_writable() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("a.txt");
    std::fs::write(&path, "hello").unwrap();
    if !make_read_only(&path) {
        eprintln!("skipping: the test user can open a read-only file for writing");
        make_writable(&path);
        return;
    }
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "edit",
        json!({"path": "a.txt", "old_string": "hello", "new_string": "bye"}),
        &context,
    )
    .await;
    make_writable(&path);

    assert!(outcome.is_error);
    assert!(
        outcome.text.contains("could not open `a.txt` for editing"),
        "{}",
        outcome.text
    );
    assert!(outcome.text.contains("can write it"), "{}", outcome.text);
    assert!(!outcome.text.contains("can read it"), "{}", outcome.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
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
        outcome.text.contains("./a.txt:2:needle here"),
        "{}",
        outcome.text
    );
}

#[tokio::test]
async fn search_without_context_keeps_the_existing_format() {
    if std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: rg is not installed");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("a.txt"),
        "needle one\nmiddle\nneedle two\n",
    )
    .unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(&registry, "search", json!({"pattern": "needle"}), &context).await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(outcome.text, "./a.txt:1:needle one\n./a.txt:3:needle two");
}

#[tokio::test]
async fn search_context_shows_before_and_after_with_distinct_prefixes() {
    if std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: rg is not installed");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("a.txt"),
        "a\nneedle\nc\nd\ne\nf\ng\nh\nneedle\ni\n",
    )
    .unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "search",
        json!({"pattern": "needle", "context": 1}),
        &context,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(
        outcome.text,
        "./a.txt-1-a\n\
         ./a.txt:2:needle\n\
         ./a.txt-3-c\n\
         --\n\
         ./a.txt-8-h\n\
         ./a.txt:9:needle\n\
         ./a.txt-10-i"
    );
}

#[tokio::test]
async fn search_context_near_the_start_has_no_leading_context() {
    if std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: rg is not installed");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "needle\nb\nc\n").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "search",
        json!({"pattern": "needle", "before": 2, "after": 1}),
        &context,
    )
    .await;

    assert_eq!(outcome.text, "./a.txt:1:needle\n./a.txt-2-b");
}

#[tokio::test]
async fn search_context_near_the_end_has_no_trailing_context() {
    if std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: rg is not installed");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "a\nb\nneedle\n").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "search",
        json!({"pattern": "needle", "context": 1, "after": 0}),
        &context,
    )
    .await;

    assert_eq!(outcome.text, "./a.txt-2-b\n./a.txt:3:needle");
}

#[tokio::test]
async fn search_context_respects_the_result_cap() {
    if std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: rg is not installed");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("a.txt"),
        "a\nneedle\nb\nc\nneedle\nd\n",
    )
    .unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "search",
        json!({"pattern": "needle", "context": 1, "max_results": 1}),
        &context,
    )
    .await;

    assert_eq!(outcome.text, "./a.txt-1-a\n./a.txt:2:needle\n./a.txt-3-b");
}

#[tokio::test]
async fn search_context_does_not_leak_from_a_file_past_the_result_cap() {
    if std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: rg is not installed");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "a-only\nneedle\n").unwrap();
    std::fs::write(directory.path().join("b.txt"), "b-one\nb-two\nneedle\n").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "search",
        json!({"pattern": "needle", "context": 2, "max_results": 1}),
        &context,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    let a_rendered = outcome.text.contains("./a.txt:2:needle");
    let b_rendered = outcome.text.contains("./b.txt:3:needle");
    assert!(
        a_rendered ^ b_rendered,
        "exactly one match should be rendered: {}",
        outcome.text
    );
    let other = if a_rendered { "./b.txt" } else { "./a.txt" };
    assert!(
        !outcome.text.contains(other),
        "context from `{other}` leaked past the match cap: {}",
        outcome.text
    );
}

#[tokio::test]
async fn search_context_is_capped_at_twenty_lines_and_output_is_truncated() {
    if std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: rg is not installed");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let body = (1..=60)
        .map(|line| format!("line {line:02}: needle here\n"))
        .collect::<String>();
    std::fs::write(directory.path().join("a.txt"), body).unwrap();
    let registry = builtin::registry();
    let limits = ToolLimits {
        max_output_bytes: 300,
        ..ToolLimits::default()
    };
    let limited_context = context_with(&directory, limits);

    let outcome = run(
        &registry,
        "search",
        json!({"pattern": "needle", "context": 1}),
        &limited_context,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert!(outcome.text.contains("bytes truncated"), "{}", outcome.text);

    let uncapped = run(
        &registry,
        "search",
        json!({"pattern": "line 30: needle", "context": 100}),
        &context(&directory),
    )
    .await;
    assert!(!uncapped.is_error, "{}", uncapped.text);
    assert_eq!(
        uncapped.text.lines().count(),
        41,
        "context must clamp to 20 lines before and after"
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

#[cfg(unix)]
#[tokio::test]
async fn search_timeout_names_the_pattern_and_next_step() {
    if std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: rg is not installed");
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let pipe = directory.path().join("pipe");
    let mkfifo = std::process::Command::new("mkfifo").arg(&pipe).status();
    if !matches!(mkfifo, Ok(status) if status.success()) {
        eprintln!("skipping: mkfifo is not available");
        return;
    }

    let registry = builtin::registry();
    let context = context_with(
        &directory,
        ToolLimits {
            timeout: Duration::from_millis(100),
            ..ToolLimits::default()
        },
    );

    let outcome = run(
        &registry,
        "search",
        json!({"pattern": "needle", "path": "pipe"}),
        &context,
    )
    .await;

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        "timed out after 100ms: search for pattern `needle` was still running; \
         narrow `path` or `glob`, or use a simpler pattern"
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
async fn write_names_a_file_that_should_be_a_directory() {
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
    assert_eq!(
        outcome.text,
        "`blocker` is not a directory while resolving `blocker/child.txt`; \
         use `write` on `blocker` to replace that file, or choose another path"
    );
}

#[tokio::test]
async fn write_reports_an_existing_directory_as_a_directory() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("notes")).unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "write",
        json!({"path": "notes", "content": "x"}),
        &context,
    )
    .await;

    assert!(outcome.is_error);
    assert_eq!(outcome.text, "`notes` is a directory; choose a file path");
    assert!(!outcome.text.contains("glob"), "{}", outcome.text);
    assert!(!outcome.text.contains("read"), "{}", outcome.text);
}

#[tokio::test]
async fn read_and_edit_name_a_file_that_should_be_a_directory() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("blocker"), "not a directory").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let read = run(
        &registry,
        "read",
        json!({"path": "blocker/child.txt"}),
        &context,
    )
    .await;
    assert!(read.is_error);
    assert_eq!(
        read.text,
        "`blocker` is not a directory while resolving `blocker/child.txt`; \
         use `glob` to find the right path, or read `blocker` if that is the file you meant"
    );

    let edit = run(
        &registry,
        "edit",
        json!({"path": "blocker/child.txt", "old_string": "a", "new_string": "b"}),
        &context,
    )
    .await;
    assert!(edit.is_error);
    assert_eq!(
        edit.text,
        "`blocker` is not a directory while resolving `blocker/child.txt`; \
         choose a file to edit under a directory, or edit `blocker` instead"
    );
}

#[tokio::test]
async fn glob_and_search_name_a_file_that_should_be_a_directory() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("blocker"), "not a directory").unwrap();
    let registry = builtin::registry();
    let context = context(&directory);
    let expected = "`blocker` is not a directory while resolving `blocker/child`; \
                    use `glob` to find the right path, or read `blocker` if that is the file \
                    you meant";

    let glob = run(
        &registry,
        "glob",
        json!({"pattern": "*", "path": "blocker/child"}),
        &context,
    )
    .await;
    assert!(glob.is_error);
    assert_eq!(glob.text, expected);

    let search = run(
        &registry,
        "search",
        json!({"pattern": "x", "path": "blocker/child"}),
        &context,
    )
    .await;
    assert!(search.is_error);
    assert_eq!(search.text, expected);
}

#[cfg(unix)]
fn lock_directory(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root and some filesystems ignore mode bits. If the process can still
    // create a file here, the failure cannot be forced.
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.join(".keel-probe"))
        .is_ok()
    {
        let _ = std::fs::remove_file(path.join(".keel-probe"));
        unlock_directory(path);
        return false;
    }
    true
}

#[cfg(unix)]
fn unlock_directory(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Mark `path` read-only for the test user. Returns whether a plain write
/// open now fails; root and some filesystems ignore the mode, so the caller
/// skips in that case. On Unix this clears the write bits; on Windows it sets
/// the read-only attribute.
fn make_read_only(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444)).unwrap();
    }
    #[cfg(not(unix))]
    {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(path, permissions).unwrap();
    }
    std::fs::OpenOptions::new().write(true).open(path).is_err()
}

// `set_readonly(false)` is world-writable on Unix, which is why clippy warns;
// it only compiles in the Windows branch here, where it clears the read-only
// attribute and leaves the ACL alone.
#[allow(clippy::permissions_set_readonly_false)]
fn make_writable(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    #[cfg(not(unix))]
    {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}

fn has_temp_file(directory: &std::path::Path) -> bool {
    std::fs::read_dir(directory)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.file_name().to_string_lossy().starts_with(".keel-"))
}

/// Changes the process-wide umask and restores it on drop, including when an
/// assertion unwinds. Tests in this binary must not depend on the umask while
/// the guard is live.
#[cfg(unix)]
struct UmaskGuard(rustix::fs::Mode);

#[cfg(unix)]
impl UmaskGuard {
    fn set(mask: rustix::fs::Mode) -> Self {
        Self(rustix::process::umask(mask))
    }
}

#[cfg(unix)]
impl Drop for UmaskGuard {
    fn drop(&mut self) {
        rustix::process::umask(self.0);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_write_leaves_the_old_file_intact() {
    let directory = tempfile::tempdir().unwrap();
    let locked = directory.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("a.txt"), "old contents").unwrap();
    if !lock_directory(&locked) {
        eprintln!("skipping: the test user can create files in a 0555 directory");
        return;
    }
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "write",
        json!({"path": "locked/a.txt", "content": "new contents"}),
        &context,
    )
    .await;
    unlock_directory(&locked);

    assert!(outcome.is_error);
    assert!(
        outcome.text.contains("could not write `locked/a.txt`"),
        "{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("check permissions"),
        "{}",
        outcome.text
    );
    assert_eq!(
        std::fs::read_to_string(locked.join("a.txt")).unwrap(),
        "old contents"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_edit_leaves_the_old_file_intact() {
    let directory = tempfile::tempdir().unwrap();
    let locked = directory.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("a.txt"), "old contents").unwrap();
    if !lock_directory(&locked) {
        eprintln!("skipping: the test user can create files in a 0555 directory");
        return;
    }
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "edit",
        json!({"path": "locked/a.txt", "old_string": "old", "new_string": "new"}),
        &context,
    )
    .await;
    unlock_directory(&locked);

    assert!(outcome.is_error);
    assert!(
        outcome
            .text
            .contains("could not write `locked/a.txt` after editing"),
        "{}",
        outcome.text
    );
    assert_eq!(
        std::fs::read_to_string(locked.join("a.txt")).unwrap(),
        "old contents"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn writing_preserves_the_existing_file_mode() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("a.txt");
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "write",
        json!({"path": "a.txt", "content": "new"}),
        &context,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o640);
}

#[cfg(unix)]
#[tokio::test]
async fn editing_preserves_the_file_mode() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("a.txt");
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "edit",
        json!({"path": "a.txt", "old_string": "old", "new_string": "new"}),
        &context,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o640);
}

#[cfg(unix)]
#[tokio::test]
async fn writing_drops_special_mode_bits() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("a.txt");
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4755)).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o4755
    );
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "write",
        json!({"path": "a.txt", "content": "new"}),
        &context,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o755);
}

#[cfg(unix)]
#[tokio::test]
async fn a_new_file_gets_the_process_umask_mode() {
    use rustix::fs::Mode;
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let umask = UmaskGuard::set(Mode::from_bits_truncate(0o077));
    let outcome = run(
        &registry,
        "write",
        json!({"path": "new.txt", "content": "new"}),
        &context,
    )
    .await;
    drop(umask);

    assert!(!outcome.is_error, "{}", outcome.text);
    let mode = std::fs::metadata(directory.path().join("new.txt"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn a_successful_replace_leaves_no_temp_file() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let written = run(
        &registry,
        "write",
        json!({"path": "nested/a.txt", "content": "new"}),
        &context,
    )
    .await;
    assert!(!written.is_error, "{}", written.text);
    assert!(
        !has_temp_file(&directory.path().join("nested")),
        "a successful replace left a temp file"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_replace_leaves_no_temp_file() {
    let directory = tempfile::tempdir().unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let locked = directory.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("a.txt"), "old").unwrap();
    if !lock_directory(&locked) {
        eprintln!("skipping: the test user can write to a 0555 directory");
        return;
    }
    let failed = run(
        &registry,
        "write",
        json!({"path": "locked/a.txt", "content": "new"}),
        &context,
    )
    .await;
    unlock_directory(&locked);

    assert!(failed.is_error);
    assert!(!has_temp_file(&locked), "a failed replace left a temp file");
}

#[tokio::test]
async fn a_read_only_file_is_not_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("a.txt");
    std::fs::write(&path, "old contents").unwrap();
    if !make_read_only(&path) {
        make_writable(&path);
        eprintln!("skipping: the test user can open a read-only file for writing");
        return;
    }
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = run(
        &registry,
        "write",
        json!({"path": "a.txt", "content": "new contents"}),
        &context,
    )
    .await;
    make_writable(&path);

    assert!(outcome.is_error);
    assert!(
        outcome.text.contains("could not write `a.txt`"),
        "{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("check permissions"),
        "{}",
        outcome.text
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "old contents");
}

#[cfg(unix)]
#[tokio::test]
async fn write_refuses_a_fifo_without_blocking() {
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo failed");
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        run(
            &registry,
            "write",
            json!({"path": "pipe", "content": "x"}),
            &context,
        ),
    )
    .await
    .expect("write blocked on a FIFO");

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        "`pipe` is not a regular file; choose a file path"
    );
    assert!(fifo.exists(), "the FIFO was replaced");
}

#[cfg(unix)]
#[tokio::test]
async fn write_refuses_a_socket_without_replacing_it() {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("service.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        run(
            &registry,
            "write",
            json!({"path": "service.sock", "content": "x"}),
            &context,
        ),
    )
    .await
    .expect("write blocked on a socket");

    assert!(outcome.is_error);
    assert_eq!(
        outcome.text,
        "`service.sock` is not a regular file; choose a file path"
    );
    assert!(
        std::fs::symlink_metadata(&socket)
            .unwrap()
            .file_type()
            .is_socket(),
        "the socket was replaced"
    );
}

#[tokio::test]
async fn an_edit_that_fails_on_old_string_leaves_the_file_byte_identical() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("a.txt");
    let original = "same same\n";
    std::fs::write(&path, original).unwrap();
    let registry = builtin::registry();
    let context = context(&directory);

    let missing = run(
        &registry,
        "edit",
        json!({"path": "a.txt", "old_string": "absent", "new_string": "x"}),
        &context,
    )
    .await;
    assert!(missing.is_error);
    assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());

    let ambiguous = run(
        &registry,
        "edit",
        json!({"path": "a.txt", "old_string": "same", "new_string": "once"}),
        &context,
    )
    .await;
    assert!(ambiguous.is_error);
    assert!(
        ambiguous.text.contains("appears 2 times"),
        "{}",
        ambiguous.text
    );
    assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
}

#[test]
fn the_default_registry_includes_apply_patch_and_keeps_opt_in_tools_out() {
    let registry = builtin::registry();
    assert_eq!(
        registry.names(),
        vec![
            "apply_patch",
            "bash",
            "edit",
            "glob",
            "read",
            "search",
            "write"
        ]
    );
    assert!(registry.get("apply_patch").is_some());
    assert!(registry.get("terminal").is_none());
    assert!(registry.get("jobs").is_none());
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
