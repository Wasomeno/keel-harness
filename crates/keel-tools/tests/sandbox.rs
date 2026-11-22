//! Landlock confinement for `bash` and `jobs`.
//!
//! The flag is off unless a context sets `sandbox`. These tests are the
//! checks the OS sandbox has to keep: workspace read-write, no `/tmp` escape,
//! no symlink escape, the same rules for jobs, and unchanged behavior when
//! the flag is off.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::{Path, PathBuf};
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct RemoveFile(PathBuf);

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for RemoveFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn shell_path(path: &Path) -> String {
    format!("'{}'", path.display())
}

async fn bash(command: &str, context: &ToolContext) -> ToolOutcome {
    BashTool::new()
        .execute(json!({ "command": command }), context)
        .await
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[tokio::test]
async fn sandbox_flag_refuses_on_unsupported_os() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(&directory, true);

    let outcome = bash("printf inside", &context).await;

    assert!(outcome.is_error, "{}", outcome.text);
    assert!(
        outcome.text.contains("OS sandboxing is not available"),
        "the refusal should say why:\n{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("Refusing to start the command"),
        "{}",
        outcome.text
    );
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    use keel_tools::builtin::jobs::JobsTool;

    /// Returns false when the test runner itself is already inside a Seatbelt
    /// sandbox, because macOS refuses nested `sandbox_apply` calls.
    fn seatbelt_available() -> bool {
        std::process::Command::new("/usr/bin/sandbox-exec")
            .args(["-p", "(version 1)(allow default)", "--", "/usr/bin/true"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[tokio::test]
    async fn sandboxed_bash_can_use_workspace_dev_null_and_private_tmpdir() {
        if !seatbelt_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, true);

        let outcome = bash(
            "printf 'hello\\n' > note.txt && cat note.txt \
             && echo dropped > /dev/null 2>/dev/null \
             && head -c 8 /dev/urandom > /dev/null \
             && test -n \"$TMPDIR\" \
             && printf 'scratch\\n' > \"$TMPDIR/probe.txt\" \
             && cat \"$TMPDIR/probe.txt\"",
            &context,
        )
        .await;

        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.contains("hello"), "{}", outcome.text);
        assert!(outcome.text.contains("scratch"), "{}", outcome.text);
        assert_eq!(
            std::fs::read(directory.path().join("note.txt")).unwrap(),
            b"hello\n"
        );
    }

    #[tokio::test]
    async fn sandboxed_bash_blocks_outside_writes_and_protects_git_hooks_and_keel() {
        if !seatbelt_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".git/hooks")).unwrap();
        std::fs::write(directory.path().join(".git/config"), b"[core]\n").unwrap();
        std::fs::create_dir_all(directory.path().join(".keel")).unwrap();
        std::fs::write(directory.path().join(".keel/mcp.toml"), b"# safe\n").unwrap();

        let context = context(&directory, true);
        let escape = scratch("macos-escape");
        let _escape = RemoveFile(escape.clone());

        let outside = bash(
            &format!("printf 'no\\n' > {}", shell_path(&escape)),
            &context,
        )
        .await;
        assert!(outside.is_error, "{}", outside.text);
        assert!(!escape.exists(), "sandboxed bash wrote outside workspace");

        let hook = bash("printf 'pwn\\n' > .git/hooks/pre-commit", &context).await;
        assert!(hook.is_error, "{}", hook.text);
        assert!(!directory.path().join(".git/hooks/pre-commit").exists());

        let git_cfg = bash("printf 'pwn\\n' >> .git/config", &context).await;
        assert!(git_cfg.is_error, "{}", git_cfg.text);
        assert_eq!(
            std::fs::read(directory.path().join(".git/config")).unwrap(),
            b"[core]\n"
        );

        let keel_cfg = bash("printf 'pwn\\n' > .keel/mcp.toml", &context).await;
        assert!(keel_cfg.is_error, "{}", keel_cfg.text);
        assert_eq!(
            std::fs::read(directory.path().join(".keel/mcp.toml")).unwrap(),
            b"# safe\n"
        );
    }

    #[tokio::test]
    async fn sandboxed_bash_remembers_working_directory_and_scrubs_credentials() {
        if !seatbelt_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, true);
        let tool = BashTool::new();

        let first = tool
            .execute(
                json!({
                    "command": "mkdir -p sub && cd sub && printf 'key=%s\\n' \"${OPENAI_API_KEY:-unset}\""
                }),
                &context,
            )
            .await;
        assert!(!first.is_error, "{}", first.text);
        assert!(first.text.contains("key=unset"), "{}", first.text);

        let second = tool.execute(json!({"command": "pwd"}), &context).await;
        assert!(!second.is_error, "{}", second.text);
        assert!(second.text.contains("/sub"), "{}", second.text);
    }

    #[tokio::test]
    async fn sandboxed_jobs_have_the_same_confinement() {
        if !seatbelt_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, true);
        let jobs = JobsTool::new();
        let escape = scratch("macos-job-escape");
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

        let finished = jobs
            .execute(
                json!({"action": "wait", "job": id, "timeout_ms": 5000}),
                &context,
            )
            .await;
        assert!(!finished.is_error, "{}", finished.text);
        assert_eq!(
            std::fs::read(directory.path().join("note.txt")).unwrap(),
            b"inside\n"
        );
        assert!(!escape.exists(), "sandboxed job wrote outside workspace");
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    use keel_tools::builtin::jobs::JobsTool;

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

    #[tokio::test]
    async fn sandboxed_bash_can_use_dev_null_dev_urandom_and_private_tmpdir() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, true);

        let outcome = bash(
            "echo dropped > /dev/null 2>/dev/null \
             && head -c 8 /dev/urandom > /dev/null \
             && test -n \"$TMPDIR\" \
             && printf 'scratch\\n' > \"$TMPDIR/probe.txt\" \
             && cat \"$TMPDIR/probe.txt\"",
            &context,
        )
        .await;

        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(
            outcome.text.contains("scratch"),
            "private TMPDIR should be writable:\n{}",
            outcome.text
        );
    }

    #[tokio::test]
    async fn sandboxed_bash_scrubs_provider_credentials_and_blocks_state_fd_poisoning() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, true);
        let tool = BashTool::new();

        // A command writing a fake path to fd 253 must not override the EXIT
        // trap's pwd -P or move the remembered directory outside the workspace.
        let first = tool
            .execute(
                json!({
                    "command": "mkdir -p sub && cd sub && printf '/etc\\n' >&253; printf 'key=%s\\n' \"${OPENAI_API_KEY:-unset}\""
                }),
                &context,
            )
            .await;
        assert!(!first.is_error, "{}", first.text);
        assert!(
            first.text.contains("key=unset"),
            "provider keys must be scrubbed from sandboxed children:\n{}",
            first.text
        );

        let second = tool.execute(json!({"command": "pwd"}), &context).await;
        assert!(!second.is_error, "{}", second.text);
        assert!(
            second.text.contains("/sub"),
            "state fd write poisoned the remembered directory:\n{}",
            second.text
        );
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
