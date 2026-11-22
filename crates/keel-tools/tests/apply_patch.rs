use keel_tools::{ToolContext, ToolLimits, Workspace, builtin};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn context(directory: &tempfile::TempDir) -> ToolContext {
    ToolContext {
        workspace: Workspace::open(directory.path()).unwrap(),
        cancel: CancellationToken::new(),
        limits: ToolLimits::default(),
    }
}

async fn run_patch(directory: &tempfile::TempDir, patch: &str) -> keel_tools::ToolOutcome {
    let registry = builtin::registry();
    let context = context(directory);
    registry
        .get("apply_patch")
        .expect("apply_patch is registered")
        .execute(json!({"patch": patch}), &context)
        .await
}

#[tokio::test]
async fn a_single_hunk_updates_the_file_and_reports_the_summary() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("hello.txt"), "first\nsecond\nthird\n").unwrap();

    let outcome = run_patch(
        &directory,
        r#"--- a/hello.txt
+++ b/hello.txt
@@ -1,3 +1,3 @@
 first
-second
+SECOND
 third
"#,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(
        outcome.text,
        "applied 1 hunk to 1 file: +1 -1\n- hello.txt: 1 hunk, +1 -1"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("hello.txt")).unwrap(),
        "first\nSECOND\nthird\n"
    );
}

#[tokio::test]
async fn several_hunks_in_one_file_apply_in_one_call() {
    let directory = tempfile::tempdir().unwrap();
    let original = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
    std::fs::write(directory.path().join("a.txt"), original).unwrap();

    let outcome = run_patch(
        &directory,
        r#"--- a/a.txt
+++ b/a.txt
@@ -2,2 +2,3 @@
 two
+inserted
 three
@@ -8,2 +9,2 @@
 eight
-nine
+NINE
"#,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(
        outcome.text,
        "applied 2 hunks to 1 file: +2 -1\n- a.txt: 2 hunks, +2 -1"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "one\ntwo\ninserted\nthree\nfour\nfive\nsix\nseven\neight\nNINE\nten\n"
    );
}

#[tokio::test]
async fn several_files_in_one_patch_change_together() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "a1\na2\n").unwrap();
    std::fs::write(directory.path().join("b.txt"), "b1\nb2\n").unwrap();

    let outcome = run_patch(
        &directory,
        r#"--- a/a.txt
+++ b/a.txt
@@ -1,1 +1,1 @@
-a1
+A1
--- a/b.txt
+++ b/b.txt
@@ -2,1 +2,1 @@
-b2
+B2
"#,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(
        outcome.text,
        "applied 2 hunks to 2 files: +2 -2\n\
         - a.txt: 1 hunk, +1 -1\n\
         - b.txt: 1 hunk, +1 -1"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "A1\na2\n"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("b.txt")).unwrap(),
        "b1\nB2\n"
    );
}

#[tokio::test]
async fn a_mismatch_names_the_file_hunk_expected_line_and_closest_line() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "apple\nbanana\ncherry\n").unwrap();

    let outcome = run_patch(
        &directory,
        r#"--- a/a.txt
+++ b/a.txt
@@ -1,1 +1,1 @@
-apple
+APPLE
@@ -3,1 +3,1 @@
-blueberry
+CHERRY
"#,
    )
    .await;

    assert!(outcome.is_error);
    assert!(outcome.text.contains("a.txt"), "{}", outcome.text);
    assert!(outcome.text.contains("hunk 2"), "{}", outcome.text);
    assert!(outcome.text.contains("expected line 3"), "{}", outcome.text);
    assert!(
        outcome.text.contains("closest actual line 3: `cherry`"),
        "{}",
        outcome.text
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "apple\nbanana\ncherry\n",
        "the first hunk must not land when the second fails"
    );
}

#[tokio::test]
async fn a_path_that_escapes_the_workspace_is_rejected_before_any_write() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "old\n").unwrap();
    let outside = directory
        .path()
        .parent()
        .unwrap()
        .join("keel-patch-escape.txt");
    let _ = std::fs::remove_file(&outside);

    let outcome = run_patch(
        &directory,
        r#"--- a/a.txt
+++ b/a.txt
@@ -1,1 +1,1 @@
-old
+new
--- a/../keel-patch-escape.txt
+++ b/../keel-patch-escape.txt
@@ -1,1 +1,1 @@
-absent
+created
"#,
    )
    .await;

    assert!(outcome.is_error, "{}", outcome.text);
    assert!(
        outcome.text.contains("escapes the workspace root"),
        "{}",
        outcome.text
    );
    assert!(!outside.exists(), "the escaping path was created");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "old\n",
        "no file may change when a later path is rejected"
    );
}

#[tokio::test]
async fn a_failing_hunk_leaves_every_target_file_untouched() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "a\n").unwrap();
    std::fs::write(directory.path().join("b.txt"), "b\n").unwrap();

    let outcome = run_patch(
        &directory,
        r#"--- a/a.txt
+++ b/a.txt
@@ -1,1 +1,1 @@
-a
+A
--- a/b.txt
+++ b/b.txt
@@ -1,1 +1,1 @@
-missing
+B
"#,
    )
    .await;

    assert!(outcome.is_error, "{}", outcome.text);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "a\n"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("b.txt")).unwrap(),
        "b\n"
    );
}

#[tokio::test]
async fn trailing_whitespace_in_context_is_accepted_but_content_is_strict() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "context   \nold\ncontent\n").unwrap();

    let outcome = run_patch(
        &directory,
        r#"--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,3 @@
 context
-old
+new
 content
"#,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "context   \nnew\ncontent\n"
    );

    let mismatched = run_patch(
        &directory,
        r#"--- a/a.txt
+++ b/a.txt
@@ -1,1 +1,1 @@
-Context
+changed
"#,
    )
    .await;
    assert!(mismatched.is_error, "{}", mismatched.text);
}

#[tokio::test]
async fn a_new_file_can_be_created_from_dev_null() {
    let directory = tempfile::tempdir().unwrap();

    let outcome = run_patch(
        &directory,
        r#"--- /dev/null
+++ b/nested/new.txt
@@ -0,0 +1,2 @@
+alpha
+beta
"#,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert!(outcome.text.contains("(new file)"), "{}", outcome.text);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("nested/new.txt")).unwrap(),
        "alpha\nbeta\n"
    );
}

#[tokio::test]
async fn a_no_newline_marker_is_preserved() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "a\nb").unwrap();

    let outcome = run_patch(
        &directory,
        r#"--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
 a
-b
\ No newline at end of file
+B
\ No newline at end of file
"#,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(
        std::fs::read(directory.path().join("a.txt")).unwrap(),
        b"a\nB"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_write_failure_rolls_back_earlier_files_and_removes_a_new_file() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "a\n").unwrap();
    std::fs::write(directory.path().join("b.txt"), "b\n").unwrap();
    std::fs::set_permissions(
        directory.path().join("b.txt"),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();

    let outcome = run_patch(
        &directory,
        r#"--- a/a.txt
+++ b/a.txt
@@ -1,1 +1,1 @@
-a
+A
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,1 @@
+created
--- a/b.txt
+++ b/b.txt
@@ -1,1 +1,1 @@
-b
+B
"#,
    )
    .await;

    assert!(outcome.is_error, "{}", outcome.text);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "a\n",
        "the first file was restored"
    );
    assert!(
        !directory.path().join("new.txt").exists(),
        "the new file was removed"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("b.txt")).unwrap(),
        "b\n"
    );
}

#[tokio::test]
async fn the_summary_counts_lines_across_several_hunks() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("a.txt"),
        "one\ntwo\nthree\nfour\nfive\nsix\n",
    )
    .unwrap();

    let outcome = run_patch(
        &directory,
        r#"--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,4 @@
-one
-two
+ONE
+TWO
+EXTRA
 three
@@ -5,2 +6,2 @@
 five
-six
+SIX
"#,
    )
    .await;

    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(
        outcome.text,
        "applied 2 hunks to 1 file: +4 -3\n- a.txt: 2 hunks, +4 -3"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "ONE\nTWO\nEXTRA\nthree\nfour\nfive\nSIX\n"
    );
}

#[tokio::test]
async fn a_path_in_two_file_sections_is_rejected_before_any_write() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "one\ntwo\n").unwrap();

    let outcome = run_patch(
        &directory,
        r#"--- a/a.txt
+++ b/a.txt
@@ -1,1 +1,1 @@
-one
+ONE
--- a/a.txt
+++ b/a.txt
@@ -2,1 +2,1 @@
-two
+TWO
"#,
    )
    .await;

    assert!(outcome.is_error, "{}", outcome.text);
    assert!(
        outcome
            .text
            .contains("appears in more than one file section"),
        "{}",
        outcome.text
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "one\ntwo\n",
        "nothing was written"
    );
}

#[tokio::test]
async fn a_gutter_bearing_hunk_fails_with_a_remove_the_prefix_hint() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "alpha\nbeta\n").unwrap();

    let outcome = run_patch(
        &directory,
        r#"--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
      1→alpha
-     2→beta
+beta
"#,
    )
    .await;

    assert!(outcome.is_error, "{}", outcome.text);
    assert!(
        outcome
            .text
            .contains("the hunk's context and removed lines carry read line-number prefixes"),
        "{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("remove the number and `→`"),
        "{}",
        outcome.text
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
        "alpha\nbeta\n",
        "the patch still failed without touching the file"
    );
}
