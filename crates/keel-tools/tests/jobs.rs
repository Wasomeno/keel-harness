//! Background jobs are opt-in and Unix-only.
//!
//! These tests start real processes and drive the real tool. They need `bash`
//! and the usual coreutils; the server tests also need `python3` and skip when
//! it is absent.

#![cfg(unix)]

use std::path::Path;
use std::time::{Duration, Instant};

use keel_tools::builtin::jobs::JobsTool;
use keel_tools::builtin::terminal::TerminalTool;
use keel_tools::{Tool, ToolContext, ToolLimits, ToolOutcome, Workspace, builtin};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const SERVER: &str = r#"
import http.server
import pathlib
import socketserver

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"keel-ok\n"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass

with socketserver.TCPServer(("127.0.0.1", 0), Handler) as server:
    pathlib.Path("port.txt").write_text(str(server.server_address[1]))
    print("ready", flush=True)
    server.serve_forever()
"#;

fn context(directory: &tempfile::TempDir) -> ToolContext {
    ToolContext {
        workspace: Workspace::open(directory.path()).unwrap(),
        cancel: CancellationToken::new(),
        limits: ToolLimits::default(),
    }
}

fn context_with_limits(directory: &tempfile::TempDir, limits: ToolLimits) -> ToolContext {
    ToolContext {
        workspace: Workspace::open(directory.path()).unwrap(),
        cancel: CancellationToken::new(),
        limits,
    }
}

async fn run(tool: &JobsTool, arguments: Value, context: &ToolContext) -> ToolOutcome {
    tool.execute(arguments, context).await
}

fn job_id(outcome: &ToolOutcome) -> u64 {
    field_u64(&outcome.text, "job:")
}

fn field_u64(text: &str, prefix: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_else(|| panic!("no `{prefix}` value in:\n{text}"))
}

fn output_total(text: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix("output: offsets "))
        .and_then(|rest| rest.split(" of ").nth(1))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no output total in:\n{text}"))
}

fn pid_from(path: &Path) -> i32 {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
        .trim()
        .parse()
        .unwrap_or_else(|error| panic!("parsing {}: {error}", path.display()))
}

fn read_pid_when_present(path: &Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if path.is_file() {
            return pid_from(path);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("{} was never written", path.display());
}

fn alive(pid: i32) -> bool {
    rustix::process::Pid::from_raw(pid)
        .map(|pid| rustix::process::test_kill_process(pid).is_ok())
        .unwrap_or(false)
}

fn wait_until_gone(pid: i32) -> bool {
    for _ in 0..150 {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn python_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn skip_without_python() -> bool {
    if python_available() {
        false
    } else {
        eprintln!("skipping: python3 is not available");
        true
    }
}

async fn poll_until(tool: &JobsTool, context: &ToolContext, id: u64, needle: &str) -> ToolOutcome {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let outcome = run(
            tool,
            json!({"action": "poll", "job": id, "view": "tail", "max_bytes": 8192}),
            context,
        )
        .await;
        assert!(!outcome.is_error, "{}", outcome.text);
        if outcome.text.contains(needle) {
            return outcome;
        }
        assert!(
            Instant::now() < deadline,
            "job {id} never produced `{needle}`:\n{}",
            outcome.text
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_synthetic_server_task_completes_and_leaves_no_orphan() {
    if skip_without_python() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("server.py"), SERVER).unwrap();
    let jobs = JobsTool::new();
    let context = context(&directory);

    let started = run(
        &jobs,
        json!({"action": "start", "command": "python3 server.py", "timeout_ms": 30_000}),
        &context,
    )
    .await;
    assert!(!started.is_error, "{}", started.text);
    let id = job_id(&started);
    let server_pid = field_u64(&started.text, "pid:") as i32;

    // A Terminal-Bench-style task waits for the server, then runs a test
    // command that needs the response.
    poll_until(&jobs, &context, id, "ready").await;
    let port: u16 = std::fs::read_to_string(directory.path().join("port.txt"))
        .expect("the server wrote its port")
        .trim()
        .parse()
        .expect("the port is a number");
    std::fs::write(
        directory.path().join("check.sh"),
        format!(
            "#!/bin/sh\n\
             python3 - <<'PY'\n\
             import urllib.request\n\
             body = urllib.request.urlopen(\"http://127.0.0.1:{port}/\").read()\n\
             raise SystemExit(0 if b\"keel-ok\" in body else 1)\n\
             PY\n"
        ),
    )
    .unwrap();

    let terminal = TerminalTool::new();
    let test = terminal
        .execute(json!({"command": "bash check.sh"}), &context)
        .await;
    assert!(!test.is_error, "the server test failed:\n{}", test.text);
    assert!(test.text.contains("exit: 0"), "{}", test.text);

    let killed = run(&jobs, json!({"action": "kill", "job": id}), &context).await;
    assert!(!killed.is_error, "{}", killed.text);
    assert!(killed.text.contains("state: killed"), "{}", killed.text);
    assert!(
        wait_until_gone(server_pid),
        "the server survived kill:\n{}",
        killed.text
    );

    let address: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match std::net::TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            Err(_) => break,
            Ok(_) if Instant::now() >= deadline => {
                panic!("the server still accepts connections after kill")
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[tokio::test]
async fn ten_mib_of_output_is_bounded_and_readable_in_chunks() {
    let directory = tempfile::tempdir().unwrap();
    let jobs = JobsTool::new();
    let context = context(&directory);

    let started = run(
        &jobs,
        json!({
            "action": "start",
            "command": "dd if=/dev/zero bs=1048576 count=10 2>/dev/null | tr '\\0' 'x'; \
                        printf '\\nEND-OF-10M\\n'",
            "timeout_ms": 30_000
        }),
        &context,
    )
    .await;
    assert!(!started.is_error, "{}", started.text);
    let id = job_id(&started);

    let waited = run(
        &jobs,
        json!({
            "action": "wait",
            "job": id,
            "timeout_ms": 20_000,
            "view": "tail",
            "max_bytes": 4096
        }),
        &context,
    )
    .await;
    assert!(!waited.is_error, "{}", waited.text);
    assert!(waited.text.contains("state: exited"), "{}", waited.text);
    assert!(waited.text.contains("exit: 0"), "{}", waited.text);

    // The child can exit before the reader has drained the last pipe buffer,
    // so give the output a bounded moment to settle.
    let settle = Instant::now() + Duration::from_secs(3);
    let mut tail_text = waited.text.clone();
    while output_total(&tail_text) < 10 * 1024 * 1024 && Instant::now() < settle {
        tokio::time::sleep(Duration::from_millis(10)).await;
        tail_text = run(
            &jobs,
            json!({"action": "poll", "job": id, "view": "tail", "max_bytes": 4096}),
            &context,
        )
        .await
        .text;
    }
    assert!(
        tail_text.contains("END-OF-10M"),
        "the newest output is missing:\n{tail_text}"
    );
    let total = output_total(&tail_text);
    assert!(
        total >= 10 * 1024 * 1024,
        "the writer did not produce 10 MiB: {total}"
    );
    assert!(
        tail_text.contains("dropped ") && !tail_text.contains("dropped 0"),
        "the buffer was not bounded:\n{tail_text}"
    );

    let first = run(
        &jobs,
        json!({"action": "poll", "job": id, "offset": 0, "max_bytes": 32_768}),
        &context,
    )
    .await;
    assert!(!first.is_error, "{}", first.text);
    assert!(first.text.contains("state: exited"), "{}", first.text);
    let first_next = field_u64(&first.text, "next offset:");
    assert_eq!(first_next, 32_768);

    let second = run(
        &jobs,
        json!({"action": "poll", "job": id, "offset": first_next, "max_bytes": 32_768}),
        &context,
    )
    .await;
    let second_next = field_u64(&second.text, "next offset:");
    assert_eq!(second_next, 65_536);

    let after_head = run(
        &jobs,
        json!({"action": "poll", "job": id, "offset": second_next, "max_bytes": 4096}),
        &context,
    )
    .await;
    assert!(
        after_head.text.contains("skipped"),
        "a read past the retained head must report the gap:\n{}",
        after_head.text
    );
    assert!(
        field_u64(&after_head.text, "next offset:") > second_next,
        "the read did not resume at the retained tail:\n{}",
        after_head.text
    );
}

#[tokio::test]
async fn a_grandchild_that_keeps_a_pipe_open_does_not_block_wait() {
    let directory = tempfile::tempdir().unwrap();
    let jobs = JobsTool::new();
    let context = context(&directory);

    let started = run(
        &jobs,
        json!({
            "action": "start",
            "command": "sleep 30 & echo $! > child.pid; printf 'parent-done\\n'; exit 0",
            "timeout_ms": 30_000
        }),
        &context,
    )
    .await;
    assert!(!started.is_error, "{}", started.text);
    let id = job_id(&started);

    let began = Instant::now();
    let waited = run(
        &jobs,
        json!({
            "action": "wait",
            "job": id,
            "timeout_ms": 5000,
            "view": "tail",
            "max_bytes": 4096
        }),
        &context,
    )
    .await;
    assert!(!waited.is_error, "{}", waited.text);
    assert!(
        began.elapsed() < Duration::from_secs(3),
        "wait blocked on a grandchild's pipe for {:?}:\n{}",
        began.elapsed(),
        waited.text
    );
    assert!(waited.text.contains("state: exited"), "{}", waited.text);
    assert!(waited.text.contains("exit: 0"), "{}", waited.text);
    if !waited.text.contains("parent-done") {
        poll_until(&jobs, &context, id, "parent-done").await;
    }

    let child = read_pid_when_present(&directory.path().join("child.pid"));
    assert!(alive(child), "the grandchild exited early");
    let killed = run(&jobs, json!({"action": "kill", "job": id}), &context).await;
    assert!(killed.text.contains("state: exited"), "{}", killed.text);
    assert!(
        wait_until_gone(child),
        "the grandchild survived the job kill:\n{}",
        killed.text
    );
}

#[tokio::test]
async fn a_timeout_kills_the_process_group_and_leaves_an_unrelated_job_alone() {
    let directory = tempfile::tempdir().unwrap();
    let jobs = JobsTool::new();
    let context = context(&directory);

    let unrelated = run(
        &jobs,
        json!({"action": "start", "command": "sleep 30", "timeout_ms": 30_000}),
        &context,
    )
    .await;
    let unrelated_id = job_id(&unrelated);
    let unrelated_pid = field_u64(&unrelated.text, "pid:") as i32;

    let started = run(
        &jobs,
        json!({
            "action": "start",
            "command": "sleep 30 & echo $! > child.pid; sleep 30",
            "timeout_ms": 300
        }),
        &context,
    )
    .await;
    let id = job_id(&started);
    let shell_pid = field_u64(&started.text, "pid:") as i32;
    let child = read_pid_when_present(&directory.path().join("child.pid"));

    let waited = run(
        &jobs,
        json!({"action": "wait", "job": id, "timeout_ms": 5000}),
        &context,
    )
    .await;
    assert!(!waited.is_error, "{}", waited.text);
    assert!(waited.text.contains("state: timed out"), "{}", waited.text);
    assert!(
        waited.text.contains("timeout after: 300ms"),
        "{}",
        waited.text
    );
    assert!(wait_until_gone(shell_pid), "the shell survived its timeout");
    assert!(
        wait_until_gone(child),
        "the background child survived the process-group kill"
    );

    let unrelated_now = run(
        &jobs,
        json!({"action": "poll", "job": unrelated_id}),
        &context,
    )
    .await;
    assert!(
        unrelated_now.text.contains("state: running"),
        "the unrelated job was killed:\n{}",
        unrelated_now.text
    );
    assert!(alive(unrelated_pid), "the unrelated process is gone");

    let killed = run(
        &jobs,
        json!({"action": "kill", "job": unrelated_id}),
        &context,
    )
    .await;
    assert!(killed.text.contains("state: killed"), "{}", killed.text);
    assert!(wait_until_gone(unrelated_pid));
}

#[tokio::test]
async fn a_timeout_is_enforced_without_another_poll() {
    let directory = tempfile::tempdir().unwrap();
    let jobs = JobsTool::new();
    let context = context(&directory);

    let started = run(
        &jobs,
        json!({"action": "start", "command": "sleep 30", "timeout_ms": 250}),
        &context,
    )
    .await;
    let id = job_id(&started);
    let pid = field_u64(&started.text, "pid:") as i32;

    // The monitor thread, not this test, has to enforce the deadline.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let after = run(&jobs, json!({"action": "poll", "job": id}), &context).await;
    assert!(after.text.contains("state: timed out"), "{}", after.text);
    assert!(
        wait_until_gone(pid),
        "the process survived the monitor timeout:\n{}",
        after.text
    );
}

#[tokio::test]
async fn a_detached_service_survives_its_job_timeout() {
    if skip_without_python() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let jobs = JobsTool::new();
    let context = context(&directory);

    let started = run(
        &jobs,
        json!({
            "action": "start",
            "command": "python3 -c 'import os, time; os.setsid(); time.sleep(30)' \
                        > /dev/null 2>&1 & echo $! > detached.pid; sleep 30",
            "timeout_ms": 1500
        }),
        &context,
    )
    .await;
    let id = job_id(&started);
    let waited = run(
        &jobs,
        json!({"action": "wait", "job": id, "timeout_ms": 5000}),
        &context,
    )
    .await;
    assert!(waited.text.contains("state: timed out"), "{}", waited.text);

    let detached = read_pid_when_present(&directory.path().join("detached.pid"));
    assert!(
        alive(detached),
        "a service that called setsid did not survive the job timeout:\n{}",
        waited.text
    );
    if let Some(pid) = rustix::process::Pid::from_raw(detached) {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
    assert!(wait_until_gone(detached));
}

#[tokio::test]
async fn a_background_job_keeps_running_after_the_calls_that_started_it() {
    let directory = tempfile::tempdir().unwrap();
    let jobs = JobsTool::new();
    let context = context(&directory);

    let started = run(
        &jobs,
        json!({
            "action": "start",
            "command": "while :; do printf 'tick\\n'; sleep 0.05; done",
            "timeout_ms": 30_000
        }),
        &context,
    )
    .await;
    let id = job_id(&started);

    let before = poll_until(&jobs, &context, id, "tick").await;
    let before_ticks = before.text.matches("tick").count();

    let terminal = TerminalTool::new();
    let foreground = terminal
        .execute(
            json!({"command": "sleep 0.3; printf 'foreground-done\\n'"}),
            &context,
        )
        .await;
    assert!(!foreground.is_error, "{}", foreground.text);

    let after = run(
        &jobs,
        json!({"action": "poll", "job": id, "view": "tail", "max_bytes": 4096}),
        &context,
    )
    .await;
    assert!(after.text.contains("state: running"), "{}", after.text);
    assert!(
        after.text.matches("tick").count() > before_ticks,
        "the job stopped after the foreground call:\nbefore:\n{}\nafter:\n{}",
        before.text,
        after.text
    );

    let killed = run(&jobs, json!({"action": "kill", "job": id}), &context).await;
    assert!(killed.text.contains("state: killed"), "{}", killed.text);
}

#[tokio::test]
async fn dropping_the_tool_leaves_no_orphan() {
    let directory = tempfile::tempdir().unwrap();
    let jobs = JobsTool::new();
    let context = context(&directory);

    let started = run(
        &jobs,
        json!({
            "action": "start",
            "command": "sleep 30 & echo $! > child.pid; echo $$ > shell.pid; sleep 30",
            "timeout_ms": 30_000
        }),
        &context,
    )
    .await;
    let shell_pid = field_u64(&started.text, "pid:") as i32;
    let child = read_pid_when_present(&directory.path().join("child.pid"));
    assert!(alive(shell_pid));
    assert!(alive(child));

    drop(jobs);

    assert!(wait_until_gone(shell_pid), "the shell survived tool drop");
    assert!(wait_until_gone(child), "the child survived tool drop");
}

#[tokio::test]
async fn wait_returns_running_when_its_own_timeout_expires() {
    let directory = tempfile::tempdir().unwrap();
    let jobs = JobsTool::new();
    let context = context(&directory);

    let started = run(
        &jobs,
        json!({"action": "start", "command": "sleep 30", "timeout_ms": 30_000}),
        &context,
    )
    .await;
    let id = job_id(&started);

    let began = Instant::now();
    let waited = run(
        &jobs,
        json!({"action": "wait", "job": id, "timeout_ms": 300}),
        &context,
    )
    .await;
    assert!(!waited.is_error, "{}", waited.text);
    assert!(
        began.elapsed() >= Duration::from_millis(250),
        "wait returned before its bound"
    );
    assert!(waited.text.contains("state: running"), "{}", waited.text);

    let killed = run(&jobs, json!({"action": "kill", "job": id}), &context).await;
    assert!(killed.text.contains("state: killed"), "{}", killed.text);
}

#[tokio::test]
async fn a_job_timeout_is_capped_by_the_run_deadline_and_closed_runs_refuse_new_jobs() {
    let directory = tempfile::tempdir().unwrap();
    let jobs = JobsTool::new();
    let limits = ToolLimits {
        deadline: Some(Instant::now() + Duration::from_millis(500)),
        ..ToolLimits::default()
    };
    let context = context_with_limits(&directory, limits);

    let started = run(
        &jobs,
        json!({"action": "start", "command": "sleep 30", "timeout_ms": 60_000}),
        &context,
    )
    .await;
    let id = job_id(&started);
    let began = Instant::now();
    let waited = run(
        &jobs,
        json!({"action": "wait", "job": id, "timeout_ms": 5000}),
        &context,
    )
    .await;
    assert!(waited.text.contains("state: timed out"), "{}", waited.text);
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "the job outlived the run deadline by {:?}:\n{}",
        began.elapsed(),
        waited.text
    );

    let late = run(
        &jobs,
        json!({"action": "start", "command": "sleep 1"}),
        &context,
    )
    .await;
    assert!(late.is_error, "{}", late.text);
    assert!(late.text.contains("deadline"), "{}", late.text);

    jobs.close();
    let closed = run(
        &jobs,
        json!({"action": "start", "command": "sleep 1"}),
        &context,
    )
    .await;
    assert!(closed.is_error, "{}", closed.text);
    assert!(closed.text.contains("closed"), "{}", closed.text);
}

#[tokio::test]
async fn polling_cancelled_does_not_kill_the_job() {
    let directory = tempfile::tempdir().unwrap();
    let jobs = JobsTool::new();
    let context = context(&directory);

    let started = run(
        &jobs,
        json!({"action": "start", "command": "sleep 30", "timeout_ms": 30_000}),
        &context,
    )
    .await;
    let id = job_id(&started);

    let cancel = CancellationToken::new();
    let cancelled = ToolContext {
        workspace: Workspace::open(directory.path()).unwrap(),
        cancel: cancel.clone(),
        limits: ToolLimits::default(),
    };
    let waiter = tokio::spawn({
        let jobs = std::sync::Arc::new(jobs);
        let cancelled = cancelled.clone();
        async move {
            let outcome = run(
                &jobs,
                json!({"action": "wait", "job": id, "timeout_ms": 5000}),
                &cancelled,
            )
            .await;
            (std::sync::Arc::clone(&jobs), outcome)
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    cancel.cancel();
    let (shared_jobs, outcome) = waiter.await.expect("the wait task finished");
    assert!(outcome.is_error, "{}", outcome.text);
    assert!(outcome.text.contains("cancelled"), "{}", outcome.text);

    let still_running = run(&shared_jobs, json!({"action": "poll", "job": id}), &context).await;
    assert!(
        still_running.text.contains("state: running"),
        "cancelling wait killed the job:\n{}",
        still_running.text
    );
    let killed = run(&shared_jobs, json!({"action": "kill", "job": id}), &context).await;
    assert!(killed.text.contains("state: killed"), "{}", killed.text);
}

#[tokio::test]
async fn list_shows_every_job_and_ids_are_not_reused() {
    let directory = tempfile::tempdir().unwrap();
    let jobs = JobsTool::new();
    let context = context(&directory);

    let first = run(
        &jobs,
        json!({"action": "start", "command": "printf 'first\\n'"}),
        &context,
    )
    .await;
    let first_id = job_id(&first);
    let second = run(
        &jobs,
        json!({"action": "start", "command": "printf 'second\\n'"}),
        &context,
    )
    .await;
    let second_id = job_id(&second);
    assert_ne!(first_id, second_id);

    let listed = run(&jobs, json!({"action": "list"}), &context).await;
    assert!(!listed.is_error, "{}", listed.text);
    assert!(
        listed.text.contains(&format!("job {first_id}:")),
        "{}",
        listed.text
    );
    assert!(
        listed.text.contains(&format!("job {second_id}:")),
        "{}",
        listed.text
    );

    let third = run(
        &jobs,
        json!({"action": "start", "command": "printf 'third\\n'"}),
        &context,
    )
    .await;
    assert!(job_id(&third) > second_id);
}

#[tokio::test]
async fn the_default_registry_has_no_jobs() {
    let registry = builtin::registry();
    assert!(registry.get("jobs").is_none());
    assert!(registry.get("terminal").is_none());
}
