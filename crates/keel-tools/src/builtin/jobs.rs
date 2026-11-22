//! Background jobs for Unix, opt-in through the CLI's `--jobs` /
//! `KEEL_JOBS=1`.
//!
//! A benchmark task spends much of its time waiting: a build, a training run
//! or a test server starts, then the agent does other work. The one-shot
//! `bash` tool serves one foreground command at a time, so a server started
//! there would block the next call. This tool starts each command in its own
//! process group, returns a job id, and keeps a bounded record of its output.
//!
//! Jobs are owned by the tool, not by the call that started them: `start`
//! returns immediately, later calls `poll`, `wait` or `kill` the job, and the
//! job keeps running until it exits, is killed, or the tool is dropped. Drop
//! signals every job's process group, which covers a normal chat end, an
//! error, a cancelled run and a panic. A child that deliberately calls
//! `setsid` leaves its job's group and survives; that is the escape hatch for
//! a service the task still needs.
//!
//! Output is kept as a 64 KiB head plus the newest 1 MiB, so a 10 MiB log is
//! bounded and still readable from either end. `poll` returns absolute byte
//! offsets and chunks; a `new` read that asks for an offset older than the
//! retained tail says how many bytes it skipped instead of failing.
//!
//! Unix only. Windows keeps the one-shot `bash` path and has no job objects in
//! this slice.

use std::collections::{BTreeMap, VecDeque};
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{
    Tool, ToolContext, ToolError, ToolFuture, ToolLimits, ToolOutcome, ToolSpec, parse_arguments,
    tool_spec,
};
use crate::truncate::truncate_output;

/// The oldest bytes retained for the head view.
const HEAD_CAP: usize = 64 * 1024;
/// The newest bytes retained for chunks and the tail view.
const TAIL_CAP: usize = 1024 * 1024;
/// A poll returns at most this much output by default.
const DEFAULT_READ_BYTES: usize = 16 * 1024;
/// A poll never returns more than this; the harness result is 64 KiB.
const MAX_READ_BYTES: usize = 32 * 1024;
/// A job without an explicit timeout gets this long.
const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(600);
/// No job may outlive this, even if the caller asks for more.
const MAX_JOB_TIMEOUT: Duration = Duration::from_secs(1800);
/// A session keeps at most this many jobs, running or finished.
const MAX_JOBS: usize = 64;
/// How often `wait` rechecks a running job.
const WAIT_TICK: Duration = Duration::from_millis(25);
/// How long a process group gets between SIGTERM and SIGKILL.
const KILL_GRACE: Duration = Duration::from_millis(150);

/// A session-scoped set of background jobs.
///
/// The tool is registered only by the CLI's `--jobs` / `KEEL_JOBS=1` opt-in
/// and only on Unix. Dropping it closes every job, so an embedder that
/// drops the registry ends the session's jobs with it.
pub struct JobsTool {
    jobs: Mutex<Jobs>,
}

impl JobsTool {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(Jobs::default()),
        }
    }

    /// Ends every job now and refuses later `start` calls. Dropping the tool
    /// calls this; calling it early is safe and idempotent.
    pub fn close(&self) {
        let entries = {
            let mut jobs = lock(&self.jobs);
            if jobs.closed {
                return;
            }
            jobs.closed = true;
            std::mem::take(&mut jobs.entries)
        };
        shutdown_jobs(entries.into_values().collect());
    }
}

impl Default for JobsTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for JobsTool {
    fn drop(&mut self) {
        self.close();
    }
}

impl Tool for JobsTool {
    fn name(&self) -> &str {
        "jobs"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            "jobs",
            "Use when: a command must keep running while you do other work, such as a build, \
             training run, development server, or the test server of a benchmark task. `bash` \
             serves one foreground command at a time; each `jobs` command gets its own process \
             group and returns a job id immediately.\n\
             Prefer: `bash` for one command whose output you need now, and `jobs` for servers \
             and long runs. Start a server with `jobs`, poll until its ready line appears, run \
             requests with `bash`, then `kill` the job when done.\n\
             Returns: `start` gives `job: N`; `poll` returns output from an absolute `offset` (or \
             the `head` / `tail` view), `wait` blocks up to `timeout_ms`, `kill` signals the \
             job's process group, and `list` shows every job. Output keeps 64 KiB from the start \
             and the newest 1 MiB; `next offset` advances so a long log is read in chunks.\n\
             On failure: a missing command, a missing job id, an unknown job, or a full job table \
             is an error result; a non-zero exit is reported as the job's `state` and `exit`, not \
             as a tool error. Jobs are killed when the session ends; a service that calls `setsid` \
             leaves the job's group and survives. Unix only; Windows keeps the one-shot `bash` \
             path.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["start", "poll", "wait", "kill", "list"],
                        "description": "What to do with the job table."
                    },
                    "command": {
                        "type": "string",
                        "description": "Shell command line for action=start."
                    },
                    "workdir": {
                        "type": "string",
                        "description": "Working directory relative to the workspace root for action=start."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Per-command timeout for action=start, or wait bound for action=wait; capped by the run deadline and the harness limit."
                    },
                    "job": {
                        "type": "integer",
                        "description": "Job id returned by action=start; required for poll, wait and kill."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Absolute byte offset for action=poll or action=wait with view=new (default 0)."
                    },
                    "view": {
                        "type": "string",
                        "enum": ["new", "head", "tail"],
                        "description": "Output view: new bytes from offset (default), the oldest retained bytes, or the newest retained bytes."
                    },
                    "max_bytes": {
                        "type": "integer",
                        "description": "Maximum bytes returned by this call; default 16384, capped at 32768."
                    }
                },
                "required": ["action"]
            }),
        )
    }

    fn summarize(&self, arguments: &Value) -> String {
        let action = arguments
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("?");
        if action == "start" {
            let command = arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let first_line = command.lines().next().unwrap_or("");
            if first_line.chars().count() > 120 {
                let short: String = first_line.chars().take(120).collect();
                return format!("jobs start: {short}…");
            }
            return format!("jobs start: {first_line}");
        }
        let job = arguments
            .get("job")
            .and_then(Value::as_u64)
            .map(|id| format!(" {id}"))
            .unwrap_or_default();
        format!("jobs {action}{job}")
    }

    fn execute<'a>(&'a self, arguments: Value, context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            match run(arguments, context, &self.jobs).await {
                Ok(outcome) => outcome,
                Err(error) => error.into_outcome(),
            }
        })
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Action {
    Start,
    Poll,
    Wait,
    Kill,
    List,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum View {
    #[default]
    New,
    Head,
    Tail,
}

#[derive(Deserialize)]
struct JobsArguments {
    action: Action,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default, alias = "job_id")]
    job: Option<u64>,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    view: Option<View>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

async fn run(
    arguments: Value,
    context: &ToolContext,
    jobs: &Mutex<Jobs>,
) -> Result<ToolOutcome, ToolError> {
    let arguments: JobsArguments = parse_arguments(arguments)?;
    match arguments.action {
        Action::Start => start(&arguments, context, jobs),
        Action::Poll => poll(&arguments, context, jobs),
        Action::Wait => wait(&arguments, context, jobs).await,
        Action::Kill => kill(&arguments, jobs),
        Action::List => list(jobs),
    }
}

#[derive(Default)]
struct Jobs {
    next: u64,
    entries: BTreeMap<u64, Arc<Job>>,
    closed: bool,
}

fn start(
    arguments: &JobsArguments,
    context: &ToolContext,
    jobs: &Mutex<Jobs>,
) -> Result<ToolOutcome, ToolError> {
    let command = arguments.command.as_deref().ok_or_else(|| {
        ToolError::InvalidArguments(
            "action=start needs `command`; pass the shell command line to run in the background"
                .to_owned(),
        )
    })?;
    if command.trim().is_empty() {
        return Err(ToolError::InvalidArguments(
            "the command is empty; pass the shell command line to run in the background".to_owned(),
        ));
    }
    if command.contains('\0') {
        return Err(ToolError::InvalidArguments(
            "the command contains a NUL byte; pass shell text without NUL".to_owned(),
        ));
    }
    let directory = context
        .workspace
        .resolve_dir(arguments.workdir.as_deref())?;
    if lock(jobs).closed {
        return Err(ToolError::Message(
            "the jobs session is closed; start a new session to run background commands".to_owned(),
        ));
    }
    let timeout = job_timeout(arguments.timeout_ms, &context.limits)?;

    let mut guard = lock(jobs);
    if guard.closed {
        return Err(ToolError::Message(
            "the jobs session is closed; start a new session to run background commands".to_owned(),
        ));
    }
    if guard.entries.len() >= MAX_JOBS {
        return Err(ToolError::Message(format!(
            "the job table already holds {MAX_JOBS} jobs; wait for one to finish and reuse its id, \
             or start a new session"
        )));
    }
    let id = guard.next.max(1);
    guard.next = id + 1;
    let job = Job::spawn(id, command, &directory, timeout)?;
    guard.entries.insert(id, Arc::clone(&job));
    drop(guard);

    job.watch_timeout();
    Ok(ToolOutcome::ok(render_start(&job)))
}

fn poll(
    arguments: &JobsArguments,
    context: &ToolContext,
    jobs: &Mutex<Jobs>,
) -> Result<ToolOutcome, ToolError> {
    let job = find_job(jobs, require_job(arguments)?)?;
    job.refresh();
    job.enforce_timeout();
    Ok(render_job(&job, arguments, context.limits.max_output_bytes))
}

async fn wait(
    arguments: &JobsArguments,
    context: &ToolContext,
    jobs: &Mutex<Jobs>,
) -> Result<ToolOutcome, ToolError> {
    let job = find_job(jobs, require_job(arguments)?)?;
    let requested = arguments
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(5))
        .min(context.limits.timeout);
    let wait_for = match context.limits.deadline {
        Some(deadline) => requested.min(
            deadline
                .saturating_duration_since(Instant::now())
                .saturating_add(KILL_GRACE),
        ),
        None => requested,
    };
    let started = Instant::now();

    loop {
        job.refresh();
        if job.status().is_some() {
            break;
        }
        if Instant::now() >= job.deadline {
            job.terminate(JobStatus::TimedOut { after: job.timeout });
            break;
        }
        let elapsed = started.elapsed();
        if elapsed >= wait_for {
            break;
        }
        let until_job = job.deadline.saturating_duration_since(Instant::now());
        let until_wait = wait_for.saturating_sub(elapsed);
        let sleep_for = until_job.min(until_wait).min(WAIT_TICK);
        if sleep_for.is_zero() {
            continue;
        }
        tokio::select! {
            () = context.cancel.cancelled() => return Err(ToolError::Cancelled),
            () = tokio::time::sleep(sleep_for) => {}
        }
    }

    job.refresh();
    job.enforce_timeout();
    Ok(render_job(&job, arguments, context.limits.max_output_bytes))
}

fn kill(arguments: &JobsArguments, jobs: &Mutex<Jobs>) -> Result<ToolOutcome, ToolError> {
    let job = find_job(jobs, require_job(arguments)?)?;
    job.refresh();
    job.terminate(JobStatus::Killed);
    job.refresh();
    Ok(render_job(&job, arguments, 64 * 1024))
}

fn list(jobs: &Mutex<Jobs>) -> Result<ToolOutcome, ToolError> {
    let entries: Vec<Arc<Job>> = {
        let jobs = lock(jobs);
        if jobs.entries.is_empty() {
            return Ok(ToolOutcome::ok(
                "jobs: 0\nthere are no background jobs; start one with \
                 {\"action\":\"start\",\"command\":\"...\"}",
            ));
        }
        jobs.entries.values().cloned().collect()
    };

    let mut lines = vec![format!("jobs: {}", entries.len())];
    for job in entries {
        job.refresh();
        job.enforce_timeout();
        let status = job.status();
        lines.push(format!(
            "job {}: {} (pid {}, elapsed {}, timeout {}) `{}`",
            job.id,
            status_label(status.as_ref()),
            job.pid,
            human(job.started.elapsed()),
            human(job.timeout),
            short_command(&job.command)
        ));
    }
    Ok(ToolOutcome::ok(lines.join("\n")))
}

fn require_job(arguments: &JobsArguments) -> Result<u64, ToolError> {
    arguments.job.ok_or_else(|| {
        ToolError::InvalidArguments(
            "this action needs `job`; use the id from action=start or action=list".to_owned(),
        )
    })
}

fn find_job(jobs: &Mutex<Jobs>, id: u64) -> Result<Arc<Job>, ToolError> {
    lock(jobs).entries.get(&id).cloned().ok_or_else(|| {
        ToolError::Message(format!(
            "there is no background job {id}; action=list shows the jobs this session started"
        ))
    })
}

fn job_timeout(requested: Option<u64>, limits: &ToolLimits) -> Result<Duration, ToolError> {
    if requested == Some(0) {
        return Err(ToolError::InvalidArguments(
            "timeout_ms must be at least 1; omit it for the 10-minute default".to_owned(),
        ));
    }
    let requested = requested
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_JOB_TIMEOUT);
    let mut timeout = requested.min(MAX_JOB_TIMEOUT);
    if let Some(deadline) = limits.deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ToolError::Message(
                "the run's wall-clock deadline has passed; a background job cannot start"
                    .to_owned(),
            ));
        }
        timeout = timeout.min(remaining);
    }
    Ok(timeout)
}

struct Job {
    id: u64,
    command: String,
    pid: u32,
    pgid: Pid,
    started: Instant,
    timeout: Duration,
    deadline: Instant,
    output: Arc<Mutex<JobOutput>>,
    state: Mutex<JobState>,
}

#[derive(Default)]
struct JobState {
    child: Option<Child>,
    status: Option<JobStatus>,
}

impl Job {
    fn spawn(
        id: u64,
        command: &str,
        directory: &std::path::Path,
        timeout: Duration,
    ) -> Result<Arc<Self>, ToolError> {
        let mut process = Command::new("bash");
        process
            .arg("-lc")
            .arg(command)
            .current_dir(directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A separate process group makes cleanup one `killpg`, and keeps
            // the harness's Ctrl-C away from a job that is supposed to outlive
            // the foreground call.
            .process_group(0);
        let mut child = process.spawn().map_err(|error| {
            ToolError::Io(format!(
                "could not start the background command `{}`: {error}; check that bash is \
                 installed and on PATH",
                short_command(command)
            ))
        })?;
        let pid = child.id();
        let Some(pgid) = Pid::from_raw(pid as i32) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ToolError::Io(
                "the background command did not report a usable process id".to_owned(),
            ));
        };

        let output = Arc::new(Mutex::new(JobOutput::default()));
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        if let Some(reader) = stdout {
            spawn_reader(reader, Arc::clone(&output));
        }
        if let Some(reader) = stderr {
            spawn_reader(reader, Arc::clone(&output));
        }

        let started = Instant::now();
        Ok(Arc::new(Self {
            id,
            command: command.to_owned(),
            pid,
            pgid,
            started,
            timeout,
            deadline: started + timeout,
            output,
            state: Mutex::new(JobState {
                child: Some(child),
                status: None,
            }),
        }))
    }

    /// Reaps a finished direct child. A grandchild that keeps a pipe open does
    /// not delay this: the status comes from the child, not from EOF.
    fn refresh(&self) {
        let mut state = lock(&self.state);
        if state.status.is_some() {
            return;
        }
        let Some(child) = state.child.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                state.status = Some(JobStatus::from_exit(status));
                state.child = None;
            }
            Ok(None) => {}
            Err(error) => {
                state.status = Some(JobStatus::Lost(error.to_string()));
                state.child = None;
            }
        }
    }

    fn status(&self) -> Option<JobStatus> {
        lock(&self.state).status.clone()
    }

    fn enforce_timeout(&self) {
        if self.status().is_none() && Instant::now() >= self.deadline {
            self.terminate(JobStatus::TimedOut {
                after: self.timeout,
            });
        }
    }

    fn terminate(&self, reason: JobStatus) {
        self.mark(reason);
        self.signal(Signal::TERM);
        thread::sleep(KILL_GRACE);
        self.signal(Signal::KILL);
        self.reap();
    }

    fn mark(&self, reason: JobStatus) {
        let mut state = lock(&self.state);
        if state.status.is_none() {
            state.status = Some(reason);
        }
    }

    fn signal(&self, signal: Signal) {
        let _ = kill_process_group(self.pgid, signal);
    }

    fn reap(&self) {
        let mut state = lock(&self.state);
        if let Some(mut child) = state.child.take() {
            match child.wait() {
                Ok(status) => {
                    if state.status.is_none() {
                        state.status = Some(JobStatus::from_exit(status));
                    }
                }
                Err(error) => {
                    if state.status.is_none() {
                        state.status = Some(JobStatus::Lost(error.to_string()));
                    }
                }
            }
        }
    }

    /// Kills the job when its per-command timeout expires, even if the model
    /// never polls it again.
    fn watch_timeout(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let _ = thread::Builder::new()
            .name(format!("keel-jobs-timeout-{}", self.id))
            .spawn(move || {
                let Some(job) = weak.upgrade() else {
                    return;
                };
                loop {
                    job.refresh();
                    if job.status().is_some() {
                        return;
                    }
                    let now = Instant::now();
                    if now >= job.deadline {
                        job.terminate(JobStatus::TimedOut { after: job.timeout });
                        return;
                    }
                    thread::sleep((job.deadline - now).min(Duration::from_millis(200)));
                }
            });
    }
}

fn shutdown_jobs(jobs: Vec<Arc<Job>>) {
    for job in &jobs {
        job.mark(JobStatus::Killed);
        job.signal(Signal::TERM);
    }
    if !jobs.is_empty() {
        thread::sleep(KILL_GRACE);
    }
    for job in &jobs {
        job.signal(Signal::KILL);
    }
    for job in jobs {
        job.reap();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum JobStatus {
    Exited(i32),
    Signalled(i32),
    TimedOut { after: Duration },
    Killed,
    Lost(String),
}

impl JobStatus {
    fn from_exit(status: ExitStatus) -> Self {
        match status.code() {
            Some(code) => Self::Exited(code),
            None => Self::Signalled(status.signal().unwrap_or(0)),
        }
    }
}

fn status_lines(status: Option<&JobStatus>) -> Vec<String> {
    match status {
        None => vec!["state: running".to_owned()],
        Some(JobStatus::Exited(code)) => {
            vec!["state: exited".to_owned(), format!("exit: {code}")]
        }
        Some(JobStatus::Signalled(signal)) => {
            vec!["state: signalled".to_owned(), format!("signal: {signal}")]
        }
        Some(JobStatus::TimedOut { after }) => vec![
            "state: timed out".to_owned(),
            format!("timeout after: {}", human(*after)),
        ],
        Some(JobStatus::Killed) => vec!["state: killed".to_owned()],
        Some(JobStatus::Lost(detail)) => {
            vec!["state: lost".to_owned(), format!("detail: {detail}")]
        }
    }
}

fn status_label(status: Option<&JobStatus>) -> String {
    match status {
        None => "running".to_owned(),
        Some(JobStatus::Exited(code)) => format!("exited {code}"),
        Some(JobStatus::Signalled(signal)) => format!("signalled {signal}"),
        Some(JobStatus::TimedOut { .. }) => "timed out".to_owned(),
        Some(JobStatus::Killed) => "killed".to_owned(),
        Some(JobStatus::Lost(_)) => "lost".to_owned(),
    }
}

#[derive(Default)]
struct JobOutput {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: u64,
}

impl JobOutput {
    fn append(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        let room = HEAD_CAP.saturating_sub(self.head.len());
        if room > 0 {
            let take = room.min(bytes.len());
            self.head.extend_from_slice(&bytes[..take]);
        }
        self.tail.extend(bytes.iter().copied());
        if self.tail.len() > TAIL_CAP {
            let drop = self.tail.len() - TAIL_CAP;
            self.tail.drain(..drop);
        }
    }

    fn retained(&self) -> u64 {
        let tail_start = self.total.saturating_sub(self.tail.len() as u64);
        self.tail.len() as u64 + (self.head.len() as u64).min(tail_start)
    }

    fn view(&self, view: View, offset: u64, max_bytes: usize) -> OutputView {
        let max_bytes = max_bytes.clamp(1, MAX_READ_BYTES);
        let total = self.total;
        let tail_start = total.saturating_sub(self.tail.len() as u64);
        let mut skipped = 0;
        let (start, data) = match view {
            View::Head => {
                let take = self.head.len().min(max_bytes);
                (0, self.head[..take].to_vec())
            }
            View::Tail => {
                let take = self.tail.len().min(max_bytes);
                let start = total - take as u64;
                (
                    start,
                    self.tail
                        .iter()
                        .skip(self.tail.len() - take)
                        .copied()
                        .collect(),
                )
            }
            View::New => {
                let mut start = offset.min(total);
                if start < self.head.len() as u64 {
                    let start_index = start as usize;
                    let take = (self.head.len() - start_index).min(max_bytes);
                    (start, self.head[start_index..start_index + take].to_vec())
                } else {
                    if start < tail_start {
                        skipped = tail_start - start;
                        start = tail_start;
                    }
                    let take = (total - start).min(max_bytes as u64) as usize;
                    let index = (start - tail_start) as usize;
                    (
                        start,
                        self.tail.iter().skip(index).take(take).copied().collect(),
                    )
                }
            }
        };
        let end = start + data.len() as u64;
        let retained = self.retained();
        OutputView {
            data,
            start,
            end,
            total,
            retained,
            dropped: total.saturating_sub(retained),
            skipped,
        }
    }
}

struct OutputView {
    data: Vec<u8>,
    start: u64,
    end: u64,
    total: u64,
    retained: u64,
    dropped: u64,
    skipped: u64,
}

fn spawn_reader<R: Read + Send + 'static>(mut reader: R, output: Arc<Mutex<JobOutput>>) {
    let _ = thread::Builder::new()
        .name("keel-jobs-reader".to_owned())
        .spawn(move || {
            let mut buffer = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => lock(&output).append(&buffer[..read]),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
}

fn render_start(job: &Job) -> String {
    job.refresh();
    let mut text = format!("job: {}\npid: {}\n", job.id, job.pid);
    for line in status_lines(job.status().as_ref()) {
        text.push_str(&line);
        text.push('\n');
    }
    text.push_str(&format!(
        "elapsed: {}\ntimeout: {}\nnext offset: 0\n",
        human(job.started.elapsed()),
        human(job.timeout)
    ));
    text.push_str(&format!(
        "Use `jobs` with {{\"action\":\"poll\",\"job\":{},\"offset\":0}} to read output, \
         {{\"action\":\"wait\",\"job\":{}}} to wait, or {{\"action\":\"kill\",\"job\":{}}} to stop it.",
        job.id, job.id, job.id
    ));
    text
}

fn render_job(job: &Job, arguments: &JobsArguments, max_output_bytes: usize) -> ToolOutcome {
    let view = arguments.view.unwrap_or_default();
    let offset = arguments.offset.unwrap_or(0);
    let max_bytes = arguments.max_bytes.unwrap_or(DEFAULT_READ_BYTES);
    let output = lock(&job.output).view(view, offset, max_bytes);

    let mut text = format!("job: {}\npid: {}\n", job.id, job.pid);
    for line in status_lines(job.status().as_ref()) {
        text.push_str(&line);
        text.push('\n');
    }
    text.push_str(&format!(
        "elapsed: {}\ntimeout: {}\n",
        human(job.started.elapsed()),
        human(job.timeout)
    ));
    text.push_str(&format!(
        "output: offsets {}..{} of {} total; retained {}; dropped {}",
        output.start, output.end, output.total, output.retained, output.dropped
    ));
    if output.skipped > 0 {
        text.push_str(&format!(
            "; skipped {} bytes that are no longer retained",
            output.skipped
        ));
    }
    text.push_str(&format!("\nnext offset: {}\n", output.end));
    if !output.data.is_empty() {
        text.push_str("---\n");
        text.push_str(&String::from_utf8_lossy(&output.data));
        if !text.ends_with('\n') {
            text.push('\n');
        }
    }
    ToolOutcome::ok(truncate_output(&text, max_output_bytes))
}

fn human(duration: Duration) -> String {
    if duration < Duration::from_secs(1) {
        format!("{}ms", duration.as_millis())
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

fn short_command(command: &str) -> String {
    let first = command.lines().next().unwrap_or(command);
    match first.char_indices().nth(120) {
        Some((index, _)) => format!("{}…", &first[..index]),
        None => first.to_owned(),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append_ten_mib(output: &mut JobOutput) {
        for _ in 0..10 {
            output.append(&vec![b'x'; 1024 * 1024]);
        }
        output.append(b"\nEND-OF-10M\n");
    }

    #[test]
    fn bounded_output_keeps_a_head_and_the_newest_tail() {
        let mut output = JobOutput::default();
        append_ten_mib(&mut output);

        assert_eq!(output.head.len(), HEAD_CAP);
        assert_eq!(output.tail.len(), TAIL_CAP);
        assert!(output.total > 10 * 1024 * 1024);
        assert!(output.retained() < output.total);
        assert_eq!(output.retained(), HEAD_CAP as u64 + TAIL_CAP as u64);

        let head = output.view(View::Head, 0, DEFAULT_READ_BYTES);
        assert_eq!(head.start, 0);
        assert_eq!(head.data.len(), DEFAULT_READ_BYTES);
        assert_eq!(head.end, DEFAULT_READ_BYTES as u64);

        let tail = output.view(View::Tail, 0, MAX_READ_BYTES);
        assert_eq!(tail.end, output.total);
        assert!(
            String::from_utf8_lossy(&tail.data).contains("END-OF-10M"),
            "the newest bytes must stay readable"
        );
    }

    #[test]
    fn reading_from_zero_walks_the_head_then_skips_to_the_tail() {
        let mut output = JobOutput::default();
        append_ten_mib(&mut output);

        let first = output.view(View::New, 0, MAX_READ_BYTES);
        assert_eq!(first.start, 0);
        assert_eq!(first.end, MAX_READ_BYTES as u64);
        assert_eq!(first.skipped, 0);

        let after_head = output.view(View::New, HEAD_CAP as u64, MAX_READ_BYTES);
        assert_eq!(
            after_head.skipped,
            output.total - TAIL_CAP as u64 - HEAD_CAP as u64
        );
        assert_eq!(
            after_head.start,
            output.total - TAIL_CAP as u64,
            "the read resumes at the oldest retained tail byte"
        );
        assert!(after_head.data.len() <= MAX_READ_BYTES);
    }

    #[test]
    fn an_empty_job_reads_as_an_empty_view() {
        let output = JobOutput::default();
        let view = output.view(View::New, 0, DEFAULT_READ_BYTES);
        assert!(view.data.is_empty());
        assert_eq!(view.start, 0);
        assert_eq!(view.end, 0);
        assert_eq!(view.total, 0);
    }

    #[test]
    fn commands_are_shortened_on_a_character_boundary() {
        let long = "x".repeat(200);
        let short = short_command(&long);
        assert_eq!(short.chars().count(), 121);
        assert!(short.ends_with('…'));
    }
}
