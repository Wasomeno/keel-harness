//! Landlock confinement for `bash` and `jobs`.
//!
//! The flag is off unless a context sets `sandbox`. These tests are the
//! checks the OS sandbox has to keep: workspace read-write, no `/tmp` escape,
//! no symlink escape, the same rules for jobs, and unchanged behavior when
//! the flag is off.

use std::time::Duration;

use keel_tools::builtin::bash::BashTool;
use keel_tools::{Tool, ToolContext, ToolLimits, ToolOutcome, Workspace};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn context(directory: &tempfile::TempDir, sandbox: bool) -> ToolContext {
    ToolContext {
        workspace: Workspace::open(directory.path()).unwrap(),
        cancel: CancellationToken::new(),
        limits: ToolLimits {
            timeout: Duration::from_secs(10),
            ..ToolLimits::default()
        },
        session_state: None,
        sandbox,
    }
}

async fn bash(command: &str, context: &ToolContext) -> ToolOutcome {
    BashTool::new()
        .execute(json!({ "command": command }), context)
        .await
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn sandbox_flag_refuses_when_landlock_is_missing() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(&directory, true);

    let outcome = bash("printf inside", &context).await;

    assert!(outcome.is_error, "{}", outcome.text);
    assert!(
        outcome.text.contains("Landlock is not available"),
        "the refusal should say why:\n{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("Refusing to start the command"),
        "{}",
        outcome.text
    );
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};

    use super::*;
    use keel_tools::builtin::jobs::JobsTool;

    fn scratch(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "keel-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    struct RemoveFile(PathBuf);

    impl Drop for RemoveFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn shell_path(path: &Path) -> String {
        format!("'{}'", path.display())
    }

    #[tokio::test]
    async fn sandboxed_bash_can_read_and_write_inside_the_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, true);

        let outcome = bash(
            "printf 'hello\\n' > note.txt && cat note.txt && test -r /etc/passwd && printf 'etc-ok\\n'",
            &context,
        )
        .await;

        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.contains("hello"), "{}", outcome.text);
        assert!(
            outcome.text.contains("etc-ok"),
            "read-only /etc should stay readable:\n{}",
            outcome.text
        );
        assert_eq!(
            std::fs::read(directory.path().join("note.txt")).unwrap(),
            b"hello\n"
        );
    }

    #[tokio::test]
    async fn sandboxed_bash_cannot_create_tmp_keel_escape_or_read_outside() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, true);
        let secret = scratch("outside");
        let _secret = RemoveFile(secret.clone());
        std::fs::write(&secret, b"top-secret\n").unwrap();
        let escape = PathBuf::from("/tmp/keel-escape");
        let _escape = RemoveFile(escape.clone());
        let _ = std::fs::remove_file(&escape);

        let outcome = bash(
            &format!(
                "printf 'nope\\n' > /tmp/keel-escape; cat {}",
                shell_path(&secret)
            ),
            &context,
        )
        .await;

        assert!(outcome.is_error, "{}", outcome.text);
        assert!(
            outcome.text.contains("/tmp/keel-escape"),
            "the tool error should name the denied path:\n{}",
            outcome.text
        );
        assert!(
            outcome.text.contains(&secret.display().to_string()),
            "the tool error should name the outside file:\n{}",
            outcome.text
        );
        assert!(
            !outcome.text.contains("top-secret"),
            "the outside file was readable:\n{}",
            outcome.text
        );
        assert!(!escape.exists(), "sandboxed bash created /tmp/keel-escape");
    }

    #[tokio::test]
    async fn sandboxed_bash_does_not_follow_a_symlink_out_of_the_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, true);
        let outside = scratch("symlink-target");
        let _outside = RemoveFile(outside.clone());
        std::fs::write(&outside, b"original\n").unwrap();
        let link = directory.path().join("escape-link");

        let outcome = bash(
            &format!(
                "ln -s {} escape-link && printf 'pwned\\n' > escape-link",
                shell_path(&outside)
            ),
            &context,
        )
        .await;

        assert!(outcome.is_error, "{}", outcome.text);
        assert!(
            outcome.text.contains("escape-link"),
            "the tool error should name the symlink:\n{}",
            outcome.text
        );
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"original\n",
            "the write followed the symlink out of the workspace"
        );
        assert!(
            link.is_symlink(),
            "the symlink was not created in the workspace"
        );
    }

    #[tokio::test]
    async fn sandboxed_bash_remembers_the_working_directory() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, true);
        let tool = BashTool::new();

        let first = tool
            .execute(json!({"command": "mkdir -p sub && cd sub"}), &context)
            .await;
        assert!(!first.is_error, "{}", first.text);

        let second = tool.execute(json!({"command": "pwd"}), &context).await;
        assert!(!second.is_error, "{}", second.text);
        assert!(
            second.text.contains("/sub"),
            "the sandboxed shell lost its working directory:\n{}",
            second.text
        );
    }

    #[tokio::test]
    async fn sandboxed_jobs_have_the_same_confinement() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, true);
        let jobs = JobsTool::new();
        let escape = scratch("job-escape");
        let _escape = RemoveFile(escape.clone());
        let command = format!(
            "printf 'inside\\n' > note.txt; printf 'no\\n' > {}",
            shell_path(&escape)
        );

        let started = jobs
            .execute(
                json!({"action": "start", "command": command, "timeout_ms": 5000}),
                &context,
            )
            .await;
        assert!(!started.is_error, "{}", started.text);
        let id = started
            .text
            .lines()
            .find_map(|line| line.strip_prefix("job: "))
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap();

        let mut finished = jobs
            .execute(
                json!({"action": "wait", "job": id, "timeout_ms": 5000}),
                &context,
            )
            .await;
        assert!(!finished.is_error, "{}", finished.text);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !finished.text.contains(&escape.display().to_string())
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
            finished = jobs
                .execute(
                    json!({"action": "poll", "job": id, "view": "tail"}),
                    &context,
                )
                .await;
        }
        assert!(
            finished.text.contains(&escape.display().to_string()),
            "the job output should name the denied path:\n{}",
            finished.text
        );
        assert_eq!(
            std::fs::read(directory.path().join("note.txt")).unwrap(),
            b"inside\n"
        );
        assert!(
            !escape.exists(),
            "a sandboxed job created {}",
            escape.display()
        );
    }

    #[tokio::test]
    async fn sandbox_off_still_reaches_outside_the_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, false);
        let outside = scratch("flag-off");
        let _outside = RemoveFile(outside.clone());
        let target = scratch("flag-off-target");
        let _target = RemoveFile(target.clone());
        std::fs::write(&target, b"before\n").unwrap();

        let written = bash(
            &format!("printf 'yes\\n' > {}", shell_path(&outside)),
            &context,
        )
        .await;
        assert!(!written.is_error, "{}", written.text);
        assert_eq!(std::fs::read(&outside).unwrap(), b"yes\n");

        let linked = bash(
            &format!(
                "ln -s {} escape-link && printf 'after\\n' > escape-link",
                shell_path(&target)
            ),
            &context,
        )
        .await;
        assert!(!linked.is_error, "{}", linked.text);
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"after\n",
            "flag off should still follow a symlink, which is the current behavior"
        );
    }

    #[tokio::test]
    async fn the_keel_process_is_not_sandboxed_by_a_child() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, true);
        let marker = scratch("parent-after");
        let _marker = RemoveFile(marker.clone());

        let outcome = bash("printf 'child\\n' > note.txt", &context).await;
        assert!(!outcome.is_error, "{}", outcome.text);

        std::fs::write(&marker, b"parent\n").unwrap();
        assert_eq!(std::fs::read(&marker).unwrap(), b"parent\n");
    }

    #[tokio::test]
    async fn sandboxed_bash_cannot_read_proc_or_escape_through_proc_root() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, true);
        let secret = scratch("proc-secret");
        let _secret = RemoveFile(secret.clone());
        std::fs::write(&secret, b"hidden\n").unwrap();

        // Open the files. `test -r` uses access(2), which Landlock does not
        // mediate. Writes go to the workspace so the check does not need
        // `/dev/null`.
        let outcome = bash(
            &format!(
                "if cat /proc/self/status > self-status.txt; then printf 'self-yes\\n'; else printf 'self-no\\n'; fi; \
                 if cat /proc/1/status > pid1-status.txt; then printf 'pid1-yes\\n'; else printf 'pid1-no\\n'; fi; \
                 cat /proc/self/root{} || true",
                secret.display()
            ),
            &context,
        )
        .await;

        assert!(
            outcome.text.contains("self-no"),
            "/proc should stay unreachable:\n{}",
            outcome.text
        );
        assert!(
            outcome.text.contains("pid1-no"),
            "other processes' proc entries should stay unreachable:\n{}",
            outcome.text
        );
        assert!(
            outcome.text.contains(&secret.display().to_string()),
            "the proc-root denial should name the path:\n{}",
            outcome.text
        );
        assert!(
            !outcome.text.contains("hidden"),
            "/proc/self/root must not reopen a path outside the ruleset:\n{}",
            outcome.text
        );
    }

    #[tokio::test]
    async fn sandboxed_bash_keeps_network_open() {
        if !python3_can_connect() {
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, true);

        let outcome = bash(
            &format!(
                "python3 -c 'import socket; socket.create_connection((\"127.0.0.1\", {port})).sendall(b\"ping\")' \
                 && printf 'net-ok\\n'"
            ),
            &context,
        )
        .await;

        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.contains("net-ok"), "{}", outcome.text);
        let _ = listener;
    }
}

#[cfg(target_os = "linux")]
fn python3_can_connect() -> bool {
    std::process::Command::new("python3")
        .arg("-c")
        .arg("import socket")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
