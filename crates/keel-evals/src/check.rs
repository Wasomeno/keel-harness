//! Mechanical checks: one Python command that must exit 0.
//!
//! Command checks point at `fixtures/checks/<script>` outside the workspace.
//! The workspace also gets a copy at `tests/check.py` so the agent can run it,
//! but grading always uses the copy the agent cannot edit.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::fixture::checks_dir;
use crate::task::{Check, Task};

/// A task check is a tiny script; anything slower than this is broken, not
/// busy. The deadline also bounds a fixture that imports something that hangs.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// Keep enough stdout/stderr for a useful failure, and drain the rest so a
/// chatty child cannot fill the pipe and deadlock.
const CHECK_OUTPUT_LIMIT: usize = 16 * 1024;
/// A normal exit should close the pipes immediately. A grandchild can keep
/// them open, so the reader joins are bounded rather than trusted.
const CHECK_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub passed: bool,
    /// "pass", or the first failure with a short command/file detail.
    pub detail: String,
}

impl CheckOutcome {
    fn passed() -> Self {
        Self {
            passed: true,
            detail: "pass".to_owned(),
        }
    }

    fn failed(detail: String) -> Self {
        Self {
            passed: false,
            detail,
        }
    }
}

/// Runs every check for `task` against `workspace_root`. All checks must pass.
pub fn run_checks(task: &Task, workspace_root: &Path) -> CheckOutcome {
    for check in task.checks {
        let Check::Command { script } = check;
        let outcome = run_command(script, workspace_root);
        if !outcome.passed {
            return outcome;
        }
    }
    CheckOutcome::passed()
}

fn run_command(script: &str, workspace_root: &Path) -> CheckOutcome {
    let path = checks_dir().join(script);
    run_command_at(&path, workspace_root, script, CHECK_TIMEOUT)
}

fn run_command_at(
    path: &Path,
    workspace_root: &Path,
    label: &str,
    timeout: Duration,
) -> CheckOutcome {
    let mut command = Command::new("python3");
    command
        .arg(path)
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Windows Python writes the ANSI code page to a pipe; keep failure details
    // and check output UTF-8 like the Unix runs.
    #[cfg(windows)]
    command
        .env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8");
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return CheckOutcome::failed(format!("{label}: could not start python3: {error}"));
        }
    };

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = std::thread::spawn(move || read_bounded(stdout, CHECK_OUTPUT_LIMIT));
    let stderr_reader = std::thread::spawn(move || read_bounded(stderr, CHECK_OUTPUT_LIMIT));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return CheckOutcome::failed(format!(
                    "{label}: could not wait for python3: {error}"
                ));
            }
        }
    };

    let Some(status) = status else {
        // Do not wait on the reader threads after a kill: a grandchild could
        // still hold the pipe open. Detaching them keeps the deadline real.
        return CheckOutcome::failed(format!("{label}: timed out after {timeout:?}"));
    };

    // On a normal exit the readers drain the pipes to EOF, so a child that
    // printed more than the limit can still exit; we keep the bounded prefix.
    // The joins are bounded because a grandchild can keep a pipe open.
    let stdout = join_bounded(stdout_reader, CHECK_DRAIN_TIMEOUT).unwrap_or_default();
    let stderr = join_bounded(stderr_reader, CHECK_DRAIN_TIMEOUT).unwrap_or_default();
    if status.success() {
        return CheckOutcome::passed();
    }

    let mut detail = String::from_utf8_lossy(&stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&stderr).trim().to_owned();
    if !stderr.is_empty() {
        if !detail.is_empty() {
            detail.push_str("; ");
        }
        detail.push_str(&stderr);
    }
    if detail.is_empty() {
        detail = format!("{label}: exit {:?}", status.code());
    }
    CheckOutcome::failed(trim_detail(&detail))
}

fn join_bounded<T>(handle: std::thread::JoinHandle<T>, timeout: Duration) -> Option<T> {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    handle.join().ok()
}

/// Reads until EOF, keeping at most `limit` bytes. Draining is the point: a
/// child that prints more than a pipe buffer would otherwise block forever.
fn read_bounded(mut reader: impl Read, limit: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let remaining = limit.saturating_sub(kept.len());
                if remaining > 0 {
                    kept.extend_from_slice(&buffer[..read.min(remaining)]);
                }
            }
            Err(_) => break,
        }
    }
    kept
}

fn trim_detail(detail: &str) -> String {
    const LIMIT: usize = 400;
    let mut trimmed: String = detail.chars().take(LIMIT).collect();
    if detail.chars().count() > LIMIT {
        trimmed.push('…');
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A loaded CI runner can take seconds to start Python and schedule the
    /// test threads. These limits only need to sit above the behavior under
    /// test, never close to normal startup time.
    const TEST_COMMAND_DEADLINE: Duration = Duration::from_secs(30);
    const TEST_ELAPSED_BOUND: Duration = Duration::from_secs(20);

    #[cfg(unix)]
    fn process_is_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(windows)]
    fn process_is_alive(pid: u32) -> bool {
        let filter = format!("PID eq {pid}");
        std::process::Command::new("tasklist")
            .args(["/FI", &filter, "/NH"])
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
    }

    #[cfg(unix)]
    fn kill_process(pid: u32) {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stderr(Stdio::null())
            .status();
        wait_until_gone(pid);
    }

    #[cfg(windows)]
    fn kill_process(pid: u32) {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        wait_until_gone(pid);
    }

    fn wait_until_gone(pid: u32) {
        for _ in 0..100 {
            if !process_is_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Best-effort cleanup so a passing test does not leave the sleeping
    /// grandchild behind, including when an assertion panics.
    struct GrandchildGuard(u32);

    impl Drop for GrandchildGuard {
        fn drop(&mut self) {
            kill_process(self.0);
        }
    }

    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).expect("test script writes");
        path
    }

    #[test]
    fn a_hanging_check_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path(), "hang.py", "import time\ntime.sleep(30)\n");

        let started = Instant::now();
        let outcome = run_command_at(&path, dir.path(), "hang.py", Duration::from_secs(1));
        let elapsed = started.elapsed();

        assert!(!outcome.passed);
        assert!(
            outcome.detail.contains("timed out"),
            "unexpected detail: {}",
            outcome.detail
        );
        assert!(
            elapsed < TEST_ELAPSED_BOUND,
            "the check did not stop at its deadline: took {elapsed:?}"
        );
    }

    #[test]
    fn a_check_with_large_output_is_drained_and_does_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(
            dir.path(),
            "loud.py",
            "import sys\nsys.stdout.write('x' * 200000)\nsys.exit(1)\n",
        );

        let started = Instant::now();
        let outcome = run_command_at(&path, dir.path(), "loud.py", TEST_COMMAND_DEADLINE);
        let elapsed = started.elapsed();

        assert!(!outcome.passed);
        assert!(outcome.detail.starts_with('x'), "{}", outcome.detail);
        assert!(
            elapsed < TEST_ELAPSED_BOUND,
            "the reader must drain the pipe instead of filling it: took {elapsed:?}"
        );
    }

    #[test]
    fn a_grandchild_holding_stdout_does_not_block_a_finished_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(
            dir.path(),
            "spawn.py",
            "import pathlib, subprocess, sys, tempfile\n\
             child = subprocess.Popen([sys.executable, \"-c\", \"import time; time.sleep(30)\"], \
             stdout=sys.stdout, stderr=sys.stderr, cwd=tempfile.gettempdir())\n\
             pathlib.Path(\"grandchild.pid\").write_text(str(child.pid))\n\
             sys.exit(0)\n",
        );

        let started = Instant::now();
        let outcome = run_command_at(&path, dir.path(), "spawn.py", TEST_COMMAND_DEADLINE);
        let elapsed = started.elapsed();
        let pid: u32 = fs::read_to_string(dir.path().join("grandchild.pid"))
            .expect("the grandchild writes its pid")
            .trim()
            .parse()
            .expect("the grandchild pid is a number");
        let grandchild = GrandchildGuard(pid);

        assert!(outcome.passed, "the check itself exited 0");
        assert!(
            elapsed >= CHECK_DRAIN_TIMEOUT,
            "the grandchild must keep the pipes open; the joins returned too quickly: {elapsed:?}"
        );
        assert!(
            elapsed < TEST_ELAPSED_BOUND,
            "the reader joins must be bounded when a grandchild holds the pipe: took {elapsed:?}"
        );
        assert!(
            process_is_alive(grandchild.0),
            "the grandchild must still be alive when the check returns; pid {pid} exited early"
        );
    }

    #[test]
    fn read_bounded_keeps_only_the_limit() {
        let data = vec![b'x'; 100_000];
        let kept = read_bounded(&data[..], 1000);
        assert_eq!(kept.len(), 1000);
    }
}
