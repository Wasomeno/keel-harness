//! A persistent shell for Unix, opt-in through the CLI.
//!
//! `bash` starts a fresh shell for every call. That is the right default, but
//! a terminal task usually needs the opposite: `cd`, exported variables,
//! activated environments and services should survive between calls. This
//! tool keeps one interactive bash process on a pseudo-terminal and sends each
//! command to it.
//!
//! The shell is interactive so bash handles Ctrl-C and job startup normally,
//! but job control is disabled (`set +m`). Every child therefore stays in the
//! shell's process group, which makes cleanup a single `killpg`. A command is
//! framed with a unique marker: its exit status and `$PWD` are printed after
//! the output, which is how the next call reads both results and the current
//! directory. Output is bytes until the marker, so non-UTF-8 output is
//! preserved and converted lossily at the end.
//!
//! Unix only. Windows keeps the one-shot `bash` path; ConPTY parity is out of
//! scope for this slice.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use rustix::process::{Pid, Signal, kill_process_group};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::tool::{
    Tool, ToolContext, ToolError, ToolFuture, ToolOutcome, ToolSpec, parse_arguments, tool_spec,
};
use crate::truncate::truncate_output;

/// The most command output retained before the harness truncates it. The
/// harness result is 64 KiB, so this only bounds memory while a chatty command
/// runs; the oldest bytes are dropped first.
const MAX_CAPTURE: usize = 4 * 1024 * 1024;

/// How long the shell gets to answer the initial prompt before the session is
/// abandoned.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// How long an interrupted command gets to return to the shell before the
/// session is restarted.
const INTERRUPT_GRACE: Duration = Duration::from_secs(2);

/// A record separator and unit separator frame the status marker. Neither is
/// a line delimiter, so the marker can follow output without a newline.
const RS: char = '\u{1e}';
const US: char = '\u{1f}';

static NONCE: AtomicU64 = AtomicU64::new(1);

/// A persistent, opt-in terminal session.
///
/// The session starts lazily on the first call, lives as long as this tool is
/// registered, and is killed when the tool is dropped. The harness treats the
/// tool as a barrier (`parallel_safe` is false), but an embedder that calls it
/// concurrently is serialized here as well.
pub struct TerminalTool {
    /// The mutex is also the call lock: one shell serves one command. A call
    /// future can be dropped while it holds the guard; that only releases the
    /// lock, so the session stays inside the tool and the next call can
    /// resynchronize it.
    session: tokio::sync::Mutex<Option<Session>>,
}

impl TerminalTool {
    pub fn new() -> Self {
        Self {
            session: tokio::sync::Mutex::new(None),
        }
    }

    /// Ends the session now. A later call starts a new one.
    pub async fn close(&self) {
        let mut slot = self.session.lock().await;
        if let Some(mut session) = slot.take() {
            session.shutdown();
        }
    }
}

impl Default for TerminalTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TerminalTool {
    fn drop(&mut self) {
        let session = self.session.get_mut().take();
        if let Some(mut session) = session {
            session.shutdown();
        }
    }
}

#[derive(Deserialize)]
struct TerminalArguments {
    command: String,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    reset: bool,
}

impl Tool for TerminalTool {
    fn name(&self) -> &str {
        "terminal"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            "terminal",
            "Use when: a task needs shell state to survive between calls, such as a \
             working directory, exported variables, an activated environment, or a service \
             started in an earlier call. This is the only tool whose shell is persistent; \
             `bash` starts a fresh shell every time.\n\
             Prefer: `bash` for one self-contained command, and `read`, `write`, `edit`, \
             `glob`, `search` for file work. Keep commands non-interactive: a command that \
             waits for input is interrupted at the timeout.\n\
             Returns: `exit: N`, the session's current `cwd`, then combined stdout and \
             stderr from the pseudo-terminal; `workdir` changes the persistent directory. \
             Output over 4 MiB is trimmed to the newest part before the harness limit.\n\
             On failure: a non-zero exit is an error result with the code and output; a \
             timeout or cancellation sends Ctrl-C, keeps the session when it recovers and \
             restarts it otherwise. Unix only; Windows keeps the one-shot `bash` path.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command line to run in the persistent shell."},
                    "workdir": {"type": "string", "description": "Working directory relative to the workspace root; changes the persistent directory for later calls."},
                    "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds, capped by the harness limit."},
                    "reset": {"type": "boolean", "description": "End the current session and start a fresh shell before running the command."}
                },
                "required": ["command"]
            }),
        )
    }

    fn summarize(&self, arguments: &Value) -> String {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let first_line = command.lines().next().unwrap_or("");
        if first_line.chars().count() > 120 {
            let short: String = first_line.chars().take(120).collect();
            format!("terminal: {short}…")
        } else {
            format!("terminal: {first_line}")
        }
    }

    fn execute<'a>(&'a self, arguments: Value, context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            // The session mutex serializes calls even though the harness
            // already treats the tool as a barrier. One shell cannot serve two
            // commands at once.
            let mut slot = self.session.lock().await;
            let result = run(arguments, context, &mut slot).await;
            match result {
                Ok(outcome) => outcome,
                Err(error) => error.into_outcome(),
            }
        })
    }
}

async fn run(
    arguments: Value,
    context: &ToolContext,
    slot: &mut Option<Session>,
) -> Result<ToolOutcome, ToolError> {
    let arguments: TerminalArguments = parse_arguments(arguments)?;
    if arguments.command.contains('\0') {
        return Err(ToolError::InvalidArguments(
            "the command contains a NUL byte; pass shell text without NUL".to_owned(),
        ));
    }
    let workdir = match arguments.workdir.as_deref() {
        Some(requested) => Some(context.workspace.resolve_dir(Some(requested))?),
        None => None,
    };
    let timeout = arguments
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(context.limits.timeout)
        .min(context.limits.timeout);

    if arguments.reset {
        if let Some(mut session) = slot.take() {
            session.shutdown();
        }
    }
    if slot.as_ref().is_some_and(|session| session.closed) {
        // A previous call interrupted a command the shell could not recover
        // from. Start clean instead of failing the next command.
        *slot = None;
    }

    if slot.is_none() {
        *slot = Some(Session::start(context.workspace.root(), &context.cancel).await?);
    }

    if let Some(session) = slot.as_mut() {
        if session.needs_resync.load(Ordering::SeqCst) {
            if session.resync().await {
                session.needs_resync.store(false, Ordering::SeqCst);
            } else {
                session.shutdown();
                *slot = None;
            }
        }
    }

    if slot.is_none() {
        *slot = Some(Session::start(context.workspace.root(), &context.cancel).await?);
    }

    let session = slot.as_mut().expect("the session was just created");
    session
        .command(
            &arguments.command,
            workdir.as_deref(),
            timeout,
            &context.cancel,
            context.limits.max_output_bytes,
        )
        .await
}

struct Session {
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: std_mpsc::Sender<Vec<u8>>,
    output: mpsc::UnboundedReceiver<Vec<u8>>,
    writer_thread: Option<JoinHandle<()>>,
    reader_thread: Option<JoinHandle<()>>,
    shell_pid: i32,
    /// Set by the interrupt guard when a call future is dropped, so the next
    /// call knows the shell may still be running the abandoned command.
    needs_resync: Arc<AtomicBool>,
    closed: bool,
}

impl Session {
    async fn start(root: &Path, cancel: &CancellationToken) -> Result<Self, ToolError> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 40,
                cols: 160,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| {
                ToolError::Io(format!(
                    "could not open a pseudo-terminal for the terminal tool: {error}"
                ))
            })?;

        let mut builder = CommandBuilder::new("bash");
        builder.args(["--noprofile", "--norc", "-i"]);
        builder.cwd(root);
        // A quiet, deterministic prompt: no banners, no history, no echo, and
        // no job control so every descendant stays in one process group.
        builder.env("PS1", "");
        builder.env("PS2", "");
        builder.env("TERM", "dumb");
        builder.env("HISTFILE", "/dev/null");
        builder.env("BASH_SILENCE_DEPRECATION_WARNING", "1");

        let mut child = pair.slave.spawn_command(builder).map_err(|error| {
            ToolError::Io(format!(
                "could not start the persistent bash shell: {error}; \
                 this tool needs bash on PATH"
            ))
        })?;
        drop(pair.slave);

        let Some(shell_pid) = child.process_id() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ToolError::Io(
                "the persistent bash shell did not report a process id".to_owned(),
            ));
        };
        let shell_pid = shell_pid as i32;

        let mut reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ToolError::Io(format!(
                    "could not read from the persistent shell: {error}"
                )));
            }
        };
        let mut writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ToolError::Io(format!(
                    "could not write to the persistent shell: {error}"
                )));
            }
        };

        let (out_tx, output) = mpsc::unbounded_channel();
        let (writer_tx, writer_rx) = std_mpsc::channel::<Vec<u8>>();

        let mut session = Self {
            child: Some(child),
            master: Some(pair.master),
            writer: writer_tx,
            output,
            writer_thread: None,
            reader_thread: None,
            shell_pid,
            needs_resync: Arc::new(AtomicBool::new(false)),
            closed: false,
        };

        let reader_thread = match std::thread::Builder::new()
            .name("keel-terminal-reader".to_owned())
            .spawn(move || {
                let mut buffer = [0u8; 16 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            if out_tx.send(buffer[..read].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            }) {
            Ok(thread) => thread,
            Err(error) => {
                session.shutdown();
                return Err(ToolError::Io(format!(
                    "could not start the persistent-shell reader: {error}"
                )));
            }
        };
        session.reader_thread = Some(reader_thread);

        let writer_thread = match std::thread::Builder::new()
            .name("keel-terminal-writer".to_owned())
            .spawn(move || {
                while let Ok(bytes) = writer_rx.recv() {
                    if writer
                        .write_all(&bytes)
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            }) {
            Ok(thread) => thread,
            Err(error) => {
                session.shutdown();
                return Err(ToolError::Io(format!(
                    "could not start the persistent-shell writer: {error}"
                )));
            }
        };
        session.writer_thread = Some(writer_thread);

        let nonce = next_nonce();
        let marker = marker_bytes(nonce);
        let init = format!(
            "stty -echo 2>/dev/null\n\
             PS1= PS2= PROMPT_COMMAND=; unset PROMPT_COMMAND\n\
             set +o history; set +m\n\
             shopt -s huponexit 2>/dev/null || true\n\
             command printf '\\n\\036KEEL-{nonce:016x}:0:%s\\037' \"$PWD\"\n"
        );
        if session.send(init.into_bytes()).is_err() {
            session.shutdown();
            return Err(ToolError::Io(
                "could not send the startup script to the persistent shell".to_owned(),
            ));
        }
        match session
            .read_marker(&marker, START_TIMEOUT, Some(cancel))
            .await
        {
            ReadResult::Completed(_) => Ok(session),
            ReadResult::Cancelled => {
                session.shutdown();
                Err(ToolError::Cancelled)
            }
            _ => {
                session.shutdown();
                Err(ToolError::Io(
                    "the persistent bash shell did not answer its startup marker; \
                     check that bash is installed and can start interactively"
                        .to_owned(),
                ))
            }
        }
    }

    fn send(&self, bytes: Vec<u8>) -> Result<(), ToolError> {
        self.writer.send(bytes).map_err(|_| {
            ToolError::Io(
                "the persistent shell's writer stopped; retry with reset=true to start a \
                 new session"
                    .to_owned(),
            )
        })
    }

    async fn command(
        &mut self,
        command: &str,
        workdir: Option<&Path>,
        timeout: Duration,
        cancel: &CancellationToken,
        max_output_bytes: usize,
    ) -> Result<ToolOutcome, ToolError> {
        let nonce = next_nonce();
        let marker = marker_bytes(nonce);
        let script = command_script(command, workdir, nonce);
        self.send(script)?;

        // If the harness drops this future on cancellation, the guard writes
        // Ctrl-C and tells the next call to resynchronize.
        let mut guard = InterruptGuard::new(&self.writer, Arc::clone(&self.needs_resync));
        let result = self.read_marker(&marker, timeout, Some(cancel)).await;
        match result {
            ReadResult::Completed(completed) => {
                guard.disarm();
                Ok(render_outcome(
                    &completed.output,
                    completed.status,
                    &completed.cwd,
                    completed.truncated,
                    max_output_bytes,
                ))
            }
            ReadResult::TimedOut { output } => {
                self.interrupt();
                let recovered = self.recover().await;
                guard.disarm();
                if !recovered {
                    self.shutdown();
                }
                let preview = tail_preview(&output, 160);
                let mut message = format!(
                    "the terminal command `{}` did not finish within {:?}; it may be \
                     waiting for input. Send complete non-interactive commands",
                    first_line(command),
                    timeout
                );
                if !preview.is_empty() {
                    message.push_str(&format!("; last output: {preview}"));
                }
                message.push_str(if recovered {
                    ". The session is still alive."
                } else {
                    ". The session was restarted; earlier shell state is gone."
                });
                Err(ToolError::Timeout {
                    after: timeout,
                    message,
                })
            }
            ReadResult::Cancelled => {
                self.interrupt();
                let recovered = self.recover().await;
                guard.disarm();
                if !recovered {
                    self.shutdown();
                }
                Err(ToolError::Cancelled)
            }
            ReadResult::Closed => {
                guard.disarm();
                let detail = self.exit_detail();
                self.shutdown();
                Err(ToolError::Io(format!(
                    "the persistent bash shell exited{detail}; the next call starts a \
                     new shell, so earlier cd and environment changes are gone"
                )))
            }
        }
    }

    fn interrupt(&self) {
        let _ = self.writer.send(vec![0x03]);
    }

    /// Interrupts whatever is running and waits for a fresh marker. Returns
    /// false when the shell does not answer; the caller then restarts it.
    async fn resync(&mut self) -> bool {
        while self.output.try_recv().is_ok() {}
        self.interrupt();
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.recover().await
    }

    async fn recover(&mut self) -> bool {
        while self.output.try_recv().is_ok() {}
        let nonce = next_nonce();
        let marker = marker_bytes(nonce);
        let script =
            format!("\ncommand printf '\\n\\036KEEL-{nonce:016x}:%s:%s\\037' \"$?\" \"$PWD\"\n");
        if self.send(script.into_bytes()).is_err() {
            return false;
        }
        matches!(
            self.read_marker(&marker, INTERRUPT_GRACE, None).await,
            ReadResult::Completed(_)
        )
    }

    async fn read_marker(
        &mut self,
        marker: &[u8],
        timeout: Duration,
        cancel: Option<&CancellationToken>,
    ) -> ReadResult {
        let mut buffer: Vec<u8> = Vec::new();
        let mut truncated = false;
        let mut search_from = 0usize;
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let cancelled = async {
            match cancel {
                Some(token) => token.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(cancelled);

        loop {
            if let Some(index) = find_from(&buffer, marker, search_from) {
                let payload_start = index + marker.len();
                if let Some(end) = buffer[payload_start..]
                    .iter()
                    .position(|byte| *byte == US as u8)
                {
                    let payload = &buffer[payload_start..payload_start + end];
                    let (status, cwd) = parse_marker(payload);
                    return ReadResult::Completed(Completed {
                        output: buffer[..index].to_vec(),
                        status,
                        cwd,
                        truncated,
                    });
                }
                search_from = index;
            } else {
                search_from = buffer.len().saturating_sub(marker.len().saturating_sub(1));
            }

            tokio::select! {
                biased;
                () = &mut deadline => return ReadResult::TimedOut { output: buffer },
                () = &mut cancelled => return ReadResult::Cancelled,
                chunk = self.output.recv() => {
                    match chunk {
                        Some(bytes) => {
                            let drained = append_bounded(&mut buffer, &bytes);
                            if drained {
                                truncated = true;
                                search_from = 0;
                            }
                        }
                        None => return ReadResult::Closed,
                    }
                }
            }
        }
    }

    fn exit_detail(&mut self) -> String {
        match self.child.as_mut().map(|child| child.try_wait()) {
            Some(Ok(Some(status))) => format!(" ({status})"),
            _ => String::new(),
        }
    }

    fn shutdown(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;

        // Interrupt the foreground command, then ask the shell to leave. Job
        // control is off, so the shell and all of its children share one
        // process group.
        self.interrupt();
        let _ = self.writer.send(b"exit\n".to_vec());
        let deadline = Instant::now() + Duration::from_millis(300);
        while let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if Instant::now() >= deadline => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            }
        }

        if let Some(pid) = Pid::from_raw(self.shell_pid) {
            let _ = kill_process_group(pid, Signal::TERM);
            std::thread::sleep(Duration::from_millis(20));
            let _ = kill_process_group(pid, Signal::KILL);
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        // Close the writer channel and the master descriptor. The reader sees
        // EOF after the slave side is gone; both threads are detached so a
        // command that escaped the process group cannot hang Drop.
        let (dead, _) = std_mpsc::channel();
        let writer = std::mem::replace(&mut self.writer, dead);
        drop(writer);
        drop(self.master.take());
        if let Some(handle) = self.writer_thread.take() {
            drop(handle);
        }
        if let Some(handle) = self.reader_thread.take() {
            drop(handle);
        }
        while self.output.try_recv().is_ok() {}
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct InterruptGuard {
    writer: std_mpsc::Sender<Vec<u8>>,
    needs_resync: Arc<AtomicBool>,
    armed: bool,
}

impl InterruptGuard {
    fn new(writer: &std_mpsc::Sender<Vec<u8>>, needs_resync: Arc<AtomicBool>) -> Self {
        Self {
            writer: writer.clone(),
            needs_resync,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.writer.send(vec![0x03]);
            self.needs_resync.store(true, Ordering::SeqCst);
        }
    }
}

struct Completed {
    output: Vec<u8>,
    status: i32,
    cwd: String,
    truncated: bool,
}

enum ReadResult {
    Completed(Completed),
    TimedOut { output: Vec<u8> },
    Cancelled,
    Closed,
}

fn next_nonce() -> u64 {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    time ^ ((std::process::id() as u64) << 32) ^ NONCE.fetch_add(1, Ordering::Relaxed)
}

fn marker_bytes(nonce: u64) -> Vec<u8> {
    format!("{RS}KEEL-{nonce:016x}:").into_bytes()
}

fn parse_marker(payload: &[u8]) -> (i32, String) {
    let text = String::from_utf8_lossy(payload);
    match text.split_once(':') {
        Some((status, cwd)) => (status.trim().parse().unwrap_or(-1), cwd.to_owned()),
        None => (-1, String::new()),
    }
}

fn command_script(command: &str, workdir: Option<&Path>, nonce: u64) -> Vec<u8> {
    let delimiter = format!("KEEL_{nonce:016x}");
    let status = format!("__keel_status_{nonce:016x}");
    let run = format!("eval \"$(cat <<'{delimiter}'\n{command}\n{delimiter}\n)\"");
    let body = match workdir {
        Some(directory) => format!(
            "cd -- {} && {run}",
            shell_quote(&directory.to_string_lossy())
        ),
        None => run,
    };
    format!(
        "{body}; {status}=$?; \
         command printf '\\n\\036KEEL-{nonce:016x}:%s:%s\\037' \"${status}\" \"$PWD\"\n"
    )
    .into_bytes()
}

fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'.' | b'_' | b'-' | b'+' | b',' | b':')
        });
    if safe {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn first_line(command: &str) -> &str {
    let line = command.lines().next().unwrap_or(command);
    match line.char_indices().nth(120) {
        Some((index, _)) => &line[..index],
        None => line,
    }
}

fn tail_preview(bytes: &[u8], limit: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

/// Appends a chunk and drops the oldest bytes beyond `MAX_CAPTURE`. Returns
/// true when bytes were dropped.
fn append_bounded(buffer: &mut Vec<u8>, bytes: &[u8]) -> bool {
    buffer.extend_from_slice(bytes);
    if buffer.len() > MAX_CAPTURE {
        let drain = buffer.len() - MAX_CAPTURE;
        buffer.drain(..drain);
        return true;
    }
    false
}

fn render_outcome(
    output: &[u8],
    status: i32,
    cwd: &str,
    truncated: bool,
    max_output_bytes: usize,
) -> ToolOutcome {
    let text = String::from_utf8_lossy(output).replace("\r\n", "\n");
    let mut rendered = format!("exit: {status}");
    if !cwd.is_empty() {
        rendered.push_str(&format!("\ncwd: {cwd}"));
    }
    let body = text.trim_end();
    if !body.is_empty() {
        rendered.push_str("\nstdout+stderr:");
        if truncated {
            rendered.push_str("\n[the oldest output was dropped at 4 MiB]");
        }
        rendered.push('\n');
        rendered.push_str(body);
    }
    if status != 0 {
        rendered.push_str(
            "\nIf this exit was not expected, read the output and fix the command or the \
             code, then retry",
        );
    }
    ToolOutcome {
        text: truncate_output(&rendered, max_output_bytes),
        is_error: status != 0,
    }
}

fn find_from(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let start = from.min(haystack.len());
    haystack[start..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|index| index + start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting_keeps_simple_paths_and_escapes_the_rest() {
        assert_eq!(shell_quote("/tmp/a-b_c"), "/tmp/a-b_c");
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn a_marker_splits_status_and_directory_on_the_first_colon() {
        assert_eq!(parse_marker(b"130:/tmp/a:b"), (130, "/tmp/a:b".to_owned()));
        assert_eq!(parse_marker(b"0:"), (0, String::new()));
        assert_eq!(parse_marker(b"not-a-status:/tmp"), (-1, "/tmp".to_owned()));
    }

    #[test]
    fn find_from_respects_the_start_offset() {
        assert_eq!(find_from(b"xxABxxAB", b"AB", 0), Some(2));
        assert_eq!(find_from(b"xxABxxAB", b"AB", 3), Some(6));
        assert_eq!(find_from(b"xxABxxAB", b"AB", 7), None);
    }

    #[test]
    fn rendering_keeps_non_utf8_lossy_and_reports_the_directory() {
        let outcome = render_outcome(b"ok\xff\xfeend\r\n", 0, "/tmp", false, 64 * 1024);
        assert!(!outcome.is_error);
        assert!(outcome.text.contains("exit: 0"), "{}", outcome.text);
        assert!(outcome.text.contains("cwd: /tmp"), "{}", outcome.text);
        assert!(
            outcome.text.contains("ok\u{fffd}\u{fffd}end"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn a_nonzero_exit_is_an_error_with_the_code() {
        let outcome = render_outcome(b"bad\n", 2, "/tmp", false, 64 * 1024);
        assert!(outcome.is_error);
        assert!(outcome.text.starts_with("exit: 2"), "{}", outcome.text);
        assert!(outcome.text.contains("then retry"), "{}", outcome.text);
    }

    #[test]
    fn a_long_line_is_cut_on_a_character_boundary() {
        let line = "é".repeat(100);
        let cut = first_line(&line);
        assert_eq!(cut, line.as_str());
        let long = "é".repeat(200);
        let cut = first_line(&long);
        assert_eq!(cut.chars().count(), 120);
        assert!(cut.is_char_boundary(cut.len()));
    }

    #[test]
    fn captured_output_is_bounded_with_the_newest_bytes_kept() {
        let mut buffer = vec![b'a'; MAX_CAPTURE];
        let dropped = append_bounded(&mut buffer, b"tail");
        assert!(dropped);
        assert_eq!(buffer.len(), MAX_CAPTURE);
        assert!(buffer.ends_with(b"tail"));
    }
}
