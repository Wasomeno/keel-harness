//! A small TTY-only activity indicator for `keel chat`.
//!
//! A model turn can be silent for tens of seconds before the first token, and a
//! tool can run just as long. On a terminal the CLI paints one line on stderr
//! after a quiet spell and clears it again before real output. When stderr is
//! not a terminal every method writes nothing, so redirected runs keep their
//! exact bytes.
//!
//! [`IndicatorState`] is the testable state machine: it decides when the line
//! is due, which frame and label it shows, and how to clear it. [`Indicator`]
//! owns the shared state and the tokio tasks that repaint it. Its methods write
//! while holding the state lock, so a frame can never land inside a text delta.

use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const FRAMES: [&str; 4] = ["-", "\\", "|", "/"];
/// How long output must be silent before the line appears.
const QUIET_AFTER: Duration = Duration::from_millis(400);
/// Repaint cadence while the line is visible.
const TICK: Duration = Duration::from_millis(100);
/// Longest label painted, including the phase text but not the frame and
/// elapsed seconds. Tool names are the only unbounded part, and a label that
/// wraps would make erase-to-end leave the first row behind.
const MAX_LABEL: usize = 40;
/// `\r` plus erase-to-end-of-line, the CLI's clearing contract.
const CLEAR: &str = "\r\x1b[K";

/// What the turn is doing while the line is shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The request is in flight or the model is thinking.
    Waiting,
    /// A tool announced by `ToolStarted` is still running.
    Tool(String),
    /// The harness is waiting out a retry backoff.
    Retry { attempt: u32 },
}

impl Phase {
    fn label(&self) -> String {
        let label = match self {
            Phase::Waiting => "waiting for model".to_owned(),
            Phase::Tool(name) => format!("running {name}"),
            Phase::Retry { attempt } => format!("retrying (attempt {attempt})"),
        };
        if label.chars().count() <= MAX_LABEL {
            return label;
        }
        let mut short: String = label.chars().take(MAX_LABEL - 3).collect();
        short.push_str("...");
        short
    }
}

/// The indicator's testable state.
///
/// `enabled` is the stderr TTY gate. `line_terminal` says whether text output
/// shares that terminal; when stdout is redirected there is no partial line on
/// screen for the indicator to protect.
pub struct IndicatorState {
    enabled: bool,
    line_terminal: bool,
    phase: Phase,
    last_activity: Instant,
    frame: usize,
    visible: bool,
    active: bool,
    paused: bool,
    line_open: bool,
    broke_line: bool,
    running_tools: Vec<String>,
}

impl IndicatorState {
    pub fn new(enabled: bool, line_terminal: bool, now: Instant) -> Self {
        Self {
            enabled,
            line_terminal,
            phase: Phase::Waiting,
            last_activity: now,
            frame: 0,
            visible: false,
            active: false,
            paused: false,
            line_open: false,
            broke_line: false,
            running_tools: Vec::new(),
        }
    }

    /// Starts a turn with the waiting label and a fresh silence window.
    pub fn begin_turn(&mut self, now: Instant) {
        self.phase = Phase::Waiting;
        self.last_activity = now;
        self.frame = 0;
        self.visible = false;
        self.active = true;
        self.paused = false;
        self.line_open = false;
        self.broke_line = false;
        self.running_tools.clear();
    }

    /// Records that a new model request opened, including a retry's attempt.
    pub fn model_started(&mut self, now: Instant) {
        self.phase = Phase::Waiting;
        self.last_activity = now;
    }

    /// Records a phase change and restarts the silence window.
    pub fn set_phase(&mut self, phase: Phase, now: Instant) {
        self.phase = phase;
        self.last_activity = now;
    }

    /// Records a tool that announced itself and is now running.
    pub fn tool_started(&mut self, name: &str, now: Instant) {
        self.running_tools.push(name.to_owned());
        self.phase = Phase::Tool(name.to_owned());
        self.last_activity = now;
    }

    /// Records a tool result. The phase stays on a tool while any sibling from
    /// the same parallel batch is still running.
    pub fn tool_finished(&mut self, name: &str, now: Instant) {
        if let Some(index) = self.running_tools.iter().position(|active| active == name) {
            self.running_tools.remove(index);
        }
        self.phase = match self.running_tools.last() {
            Some(name) => Phase::Tool(name.clone()),
            None => Phase::Waiting,
        };
        self.last_activity = now;
    }

    /// Records the text that was just written to stdout.
    pub fn note_text(&mut self, delta: &str, now: Instant) {
        self.phase = Phase::Waiting;
        if !delta.is_empty() {
            self.line_open = self.line_terminal && !delta.ends_with('\n');
            self.broke_line = false;
        }
        self.last_activity = now;
    }

    /// Records a complete line written to stdout or stderr.
    pub fn note_line(&mut self, now: Instant) {
        self.line_open = false;
        self.broke_line = false;
        self.last_activity = now;
    }

    /// Ends a streamed text line and reports whether its newline is still due.
    ///
    /// The indicator may already have opened a fresh line after partial text;
    /// in that case the previous line is terminated and the caller must not add
    /// another newline. A text line that was never broken keeps the old
    /// `println!()` behavior, including its blank line when the run already
    /// ended with a newline.
    pub fn end_text_line(&mut self, now: Instant) -> bool {
        let newline = !self.broke_line;
        self.line_open = false;
        self.broke_line = false;
        self.last_activity = now;
        newline
    }

    /// Returns the bytes to paint, if the line is due or already visible.
    ///
    /// The first paint after a quiet spell starts on a fresh line when real
    /// text is still open, so the indicator never overwrites partial output.
    /// Every paint starts with `\r` and ends by erasing the rest of the line.
    pub fn tick(&mut self, now: Instant) -> Option<String> {
        if !self.enabled || !self.active || self.paused {
            return None;
        }
        if self.visible {
            self.frame = self.frame.wrapping_add(1);
            return Some(self.render(now));
        }
        if now.saturating_duration_since(self.last_activity) < QUIET_AFTER {
            return None;
        }
        self.visible = true;
        self.frame = 0;
        let mut line = String::new();
        if self.line_open {
            line.push('\n');
            self.line_open = false;
            self.broke_line = true;
        }
        line.push_str(&self.render(now));
        Some(line)
    }

    /// Clears a visible line; safe to call when nothing is shown.
    pub fn clear(&mut self) -> Option<&'static str> {
        if self.visible {
            self.visible = false;
            Some(CLEAR)
        } else {
            None
        }
    }

    /// Stops the indicator for the rest of the turn.
    pub fn finish(&mut self) -> Option<&'static str> {
        self.active = false;
        self.paused = false;
        self.clear()
    }

    /// Hides the line while a prompt owns the terminal.
    pub fn suspend(&mut self) -> Option<&'static str> {
        self.paused = true;
        self.clear()
    }

    /// Returns control to the turn after a prompt.
    pub fn resume(&mut self, now: Instant) {
        if self.active {
            self.paused = false;
            self.line_open = false;
            self.broke_line = false;
            self.last_activity = now;
        }
    }

    fn render(&self, now: Instant) -> String {
        let elapsed = now.saturating_duration_since(self.last_activity).as_secs();
        format!(
            "\r{} {} ({}s)\x1b[K",
            FRAMES[self.frame % FRAMES.len()],
            self.phase.label(),
            elapsed
        )
    }
}

struct Inner {
    enabled: bool,
    state: Arc<Mutex<IndicatorState>>,
    ticker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    interrupt: Option<tokio::task::JoinHandle<()>>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, IndicatorState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn take_ticker(&self) -> Option<tokio::task::JoinHandle<()>> {
        self.ticker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

/// A drop guard for the ticker task and the visible line.
impl Drop for Inner {
    fn drop(&mut self) {
        let ticker = self
            .ticker
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(ticker) = ticker.take() {
            ticker.abort();
        }
        if let Some(interrupt) = &self.interrupt {
            interrupt.abort();
        }
        let mut state = self.lock();
        if let Some(clear) = state.finish() {
            write_stderr(clear);
        }
    }
}

/// The CLI's handle to the activity line.
///
/// Cloning is cheap and shares one line. All output methods lock the same
/// state the ticker uses, so clearing and writing cannot interleave with a
/// repaint.
#[derive(Clone)]
pub struct Indicator {
    inner: Arc<Inner>,
}

impl Indicator {
    /// Creates the indicator, gated on stderr being a terminal.
    ///
    /// `mcp_requested` says whether this run may start MCP servers.
    /// `keel-mcp` writes KEEL_MCP_DEBUG lines straight to stderr during a
    /// turn, where a repaint would erase them, so the line stays off only when
    /// both are true. The Ctrl-C handler only starts when the line is enabled,
    /// and each turn starts its own ticker; piped runs do no extra work.
    pub fn new(mcp_requested: bool) -> Self {
        let mcp_debug_lines = mcp_requested && std::env::var_os("KEEL_MCP_DEBUG").is_some();
        let enabled = indicator_enabled(io::stderr().is_terminal(), mcp_debug_lines);
        let line_terminal = io::stdout().is_terminal();
        let state = Arc::new(Mutex::new(IndicatorState::new(
            enabled,
            line_terminal,
            Instant::now(),
        )));
        let interrupt = enabled.then(|| spawn_interrupt(Arc::clone(&state)));
        Self {
            inner: Arc::new(Inner {
                enabled,
                state,
                ticker: Mutex::new(None),
                interrupt,
            }),
        }
    }

    /// Marks the start of a turn.
    pub fn begin_turn(&self) {
        {
            let mut state = self.inner.lock();
            let now = Instant::now();
            if let Some(clear) = state.clear() {
                write_stderr(clear);
            }
            state.begin_turn(now);
        }
        if self.inner.enabled {
            let mut slot = self
                .inner
                .ticker
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(previous) = slot.take() {
                previous.abort();
            }
            *slot = Some(spawn_ticker(Arc::clone(&self.inner.state)));
        }
    }

    /// Records that a new model request opened, including a retry's attempt.
    pub fn model_started(&self) {
        self.inner.lock().model_started(Instant::now());
    }

    /// Names the phase the turn is waiting in.
    pub fn set_phase(&self, phase: Phase) {
        self.inner.lock().set_phase(phase, Instant::now());
    }

    /// Records a tool that announced itself and is now running.
    pub fn tool_started(&self, name: &str) {
        self.inner.lock().tool_started(name, Instant::now());
    }

    /// Records a tool result, staying on a sibling that is still running.
    pub fn tool_finished(&self, name: &str) {
        self.inner.lock().tool_finished(name, Instant::now());
    }

    /// Writes a text delta to stdout, clearing the line first.
    pub fn text(&self, delta: &str) {
        let mut state = self.inner.lock();
        if let Some(clear) = state.clear() {
            write_stderr(clear);
        }
        write_stdout(delta);
        state.note_text(delta, Instant::now());
    }

    /// Ends a streamed text line the way `println!()` did before.
    ///
    /// When the indicator started on a line of its own after partial text, the
    /// text line is already terminated and no extra newline is written.
    pub fn end_text_line(&self) {
        let mut state = self.inner.lock();
        if let Some(clear) = state.clear() {
            write_stderr(clear);
        }
        if state.end_text_line(Instant::now()) {
            write_stdout("\n");
        }
    }

    /// Writes one complete line to stderr, clearing the line first.
    pub fn stderr_line(&self, line: fmt::Arguments<'_>) {
        let mut state = self.inner.lock();
        if let Some(clear) = state.clear() {
            write_stderr(clear);
        }
        let mut stderr = io::stderr().lock();
        stderr.write_fmt(line).expect("failed printing to stderr");
        stderr.write_all(b"\n").expect("failed printing to stderr");
        stderr.flush().expect("failed printing to stderr");
        drop(stderr);
        state.note_line(Instant::now());
    }

    /// Writes one complete line to stdout, clearing the line first.
    pub fn stdout_line(&self, line: fmt::Arguments<'_>) {
        let mut state = self.inner.lock();
        if let Some(clear) = state.clear() {
            write_stderr(clear);
        }
        let mut stdout = io::stdout().lock();
        stdout.write_fmt(line).expect("failed printing to stdout");
        stdout.write_all(b"\n").expect("failed printing to stdout");
        stdout.flush().expect("failed printing to stdout");
        drop(stdout);
        state.note_line(Instant::now());
    }

    /// Stops the line at the end of a turn, clearing it if visible.
    pub fn finish(&self) {
        if let Some(ticker) = self.inner.take_ticker() {
            ticker.abort();
        }
        let mut state = self.inner.lock();
        if let Some(clear) = state.finish() {
            write_stderr(clear);
        }
    }

    /// Hides the line while another prompt reads from the terminal.
    pub fn suspend(&self) {
        let mut state = self.inner.lock();
        if let Some(clear) = state.suspend() {
            write_stderr(clear);
        }
    }

    /// Picks the line back up after a prompt.
    pub fn resume(&self) {
        self.inner.lock().resume(Instant::now());
    }
}

fn spawn_ticker(state: Arc<Mutex<IndicatorState>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let mut state = lock(&state);
            if let Some(line) = state.tick(Instant::now()) {
                write_stderr(&line);
            }
        }
    })
}

/// Clears the line before the process exits on Ctrl-C, the one path where no
/// destructor runs.
fn spawn_interrupt(state: Arc<Mutex<IndicatorState>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let mut state = lock(&state);
            if let Some(clear) = state.finish() {
                write_stderr(clear);
            }
            std::process::exit(130);
        }
    })
}

fn lock(state: &Mutex<IndicatorState>) -> MutexGuard<'_, IndicatorState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The stderr TTY gate, kept separate from the environment read so the truth
/// table is testable without a terminal or a debug run.
fn indicator_enabled(stderr_is_terminal: bool, mcp_debug_lines: bool) -> bool {
    stderr_is_terminal && !mcp_debug_lines
}

fn write_stdout(text: &str) {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .expect("failed printing to stdout");
    // A closed pipe must stop the turn instead of quietly spending tokens
    // until the next complete line. The old `print!` path only noticed on a
    // newline write; checking the explicit flush makes the failure immediate.
    stdout.flush().expect("failed printing to stdout");
}

/// Best effort: a missing or closed terminal must not take down a turn, and
/// the ticker itself must not panic over a decorative line.
fn write_stderr(text: &str) {
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_at(start: Instant) -> IndicatorState {
        let mut state = IndicatorState::new(true, true, start);
        state.begin_turn(start);
        state
    }

    fn first_char(line: &str) -> char {
        line.trim_start_matches('\r').chars().next().unwrap()
    }

    #[test]
    fn stays_silent_until_the_threshold() {
        let start = Instant::now();
        let mut state = state_at(start);

        assert_eq!(
            state.tick(start + QUIET_AFTER - Duration::from_millis(1)),
            None,
            "nothing is painted before the quiet spell ends"
        );

        let line = state
            .tick(start + QUIET_AFTER)
            .expect("the line is due at the threshold");
        assert!(line.starts_with('\r'), "{line:?}");
        assert!(line.ends_with("\x1b[K"), "{line:?}");
        assert!(line.contains("waiting for model"), "{line:?}");
    }

    #[test]
    fn frames_cycle_in_order() {
        let start = Instant::now();
        let mut state = state_at(start);

        let first = state.tick(start + QUIET_AFTER).unwrap();
        let mut frames = vec![first_char(&first)];
        for step in 1..=4u32 {
            let line = state
                .tick(start + QUIET_AFTER + TICK * step)
                .expect("the visible line repaints");
            frames.push(first_char(&line));
        }

        assert_eq!(frames, vec!['-', '\\', '|', '/', '-'], "{frames:?}");
    }

    #[test]
    fn label_follows_the_phase() {
        let start = Instant::now();

        let mut waiting = state_at(start);
        let line = waiting.tick(start + QUIET_AFTER).unwrap();
        assert!(line.contains("waiting for model"), "{line:?}");

        let mut tool = state_at(start);
        tool.tool_started("bash", start);
        let line = tool.tick(start + QUIET_AFTER).unwrap();
        assert!(line.contains("running bash"), "{line:?}");

        let mut retry = state_at(start);
        retry.set_phase(Phase::Retry { attempt: 2 }, start);
        let line = retry.tick(start + QUIET_AFTER).unwrap();
        assert!(line.contains("retrying (attempt 2)"), "{line:?}");
    }

    #[test]
    fn a_new_attempt_resets_the_retry_label_and_elapsed() {
        let start = Instant::now();
        let mut state = state_at(start);
        state.set_phase(Phase::Retry { attempt: 1 }, start);
        let retry = state.tick(start + QUIET_AFTER).unwrap();
        assert!(retry.contains("retrying (attempt 1)"), "{retry:?}");

        let restarted = start + Duration::from_secs(5);
        state.model_started(restarted);
        let line = state.tick(restarted + QUIET_AFTER).unwrap();
        assert!(line.contains("waiting for model"), "{line:?}");
        assert!(!line.contains("retrying"), "{line:?}");
        assert!(line.contains("(0s)"), "the window restarts too: {line:?}");
    }

    #[test]
    fn a_parallel_batch_stays_on_a_tool_until_the_last_result() {
        let start = Instant::now();
        let mut state = state_at(start);
        state.tool_started("read", start);
        state.tool_started("glob", start);
        state.tool_finished("read", start);

        let line = state.tick(start + QUIET_AFTER).unwrap();
        assert!(line.contains("running glob"), "{line:?}");

        state.tool_finished("glob", start);
        let line = state.tick(start + QUIET_AFTER + TICK).unwrap();
        assert!(line.contains("waiting for model"), "{line:?}");
    }

    #[test]
    fn long_tool_names_are_clamped() {
        let name = "x".repeat(200);
        let label = Phase::Tool(name.clone()).label();
        assert_eq!(label.chars().count(), MAX_LABEL, "{label:?}");
        assert!(label.ends_with("..."), "{label:?}");

        let start = Instant::now();
        let mut state = state_at(start);
        state.tool_started(&name, start);
        let line = state.tick(start + QUIET_AFTER).unwrap();
        assert!(line.contains("running "), "{line:?}");
        assert!(line.contains("..."), "{line:?}");
    }

    #[test]
    fn activity_restarts_the_quiet_window() {
        let start = Instant::now();
        let mut state = state_at(start);
        let almost = start + QUIET_AFTER - Duration::from_millis(100);

        assert_eq!(state.tick(almost), None);
        state.set_phase(Phase::Waiting, almost);
        assert_eq!(
            state.tick(start + QUIET_AFTER),
            None,
            "the phase change was activity too"
        );
        assert!(state.tick(almost + QUIET_AFTER).is_some());
    }

    #[test]
    fn finish_clears_and_stays_silent() {
        let start = Instant::now();
        let mut state = state_at(start);
        assert!(state.tick(start + QUIET_AFTER).is_some());

        assert_eq!(state.finish(), Some(CLEAR));
        assert_eq!(
            state.tick(start + QUIET_AFTER + Duration::from_secs(30)),
            None,
            "a finished turn never paints again"
        );
        assert_eq!(state.finish(), None, "clearing a finished line is a no-op");
    }

    #[test]
    fn enabled_requires_a_terminal_and_no_mcp_debug_lines() {
        assert!(indicator_enabled(true, false));
        assert!(!indicator_enabled(false, false));
        assert!(!indicator_enabled(true, true));
        assert!(!indicator_enabled(false, true));
    }

    #[test]
    fn disabled_state_never_paints() {
        let start = Instant::now();
        let mut state = IndicatorState::new(false, true, start);
        state.begin_turn(start);

        assert_eq!(state.tick(start + Duration::from_secs(30)), None);
    }

    #[test]
    fn a_partial_line_is_not_overwritten() {
        let start = Instant::now();
        let mut state = state_at(start);
        state.note_text("partial", start);

        let line = state.tick(start + QUIET_AFTER).unwrap();
        assert!(line.starts_with("\n\r"), "{line:?}");
        assert!(state.broke_line, "the indicator opened a fresh line");

        assert_eq!(state.clear(), Some(CLEAR));
        state.note_text("more", start + QUIET_AFTER);
        assert!(!state.broke_line);
    }

    #[test]
    fn a_redirected_stdout_does_not_mark_a_terminal_line() {
        let start = Instant::now();
        let mut state = IndicatorState::new(true, false, start);
        state.begin_turn(start);
        state.note_text("partial", start);

        let line = state.tick(start + QUIET_AFTER).unwrap();
        assert!(line.starts_with('\r'), "{line:?}");
        assert!(!state.broke_line);
    }

    #[test]
    fn end_text_line_skips_the_newline_only_after_a_fresh_line() {
        let start = Instant::now();
        let at = start + Duration::from_millis(1);

        let mut partial = state_at(start);
        partial.note_text("partial", start);
        assert!(
            partial.end_text_line(at),
            "a plain partial line still needs its newline"
        );

        let mut complete = state_at(start);
        complete.note_text("partial\n", start);
        assert!(
            complete.end_text_line(at),
            "the old path keeps its newline even after a completed line"
        );

        let mut broken = state_at(start);
        broken.note_text("partial", start);
        broken.tick(start + QUIET_AFTER).unwrap();
        assert!(
            !broken.end_text_line(start + QUIET_AFTER),
            "the indicator already ended the text line"
        );
    }

    #[test]
    fn suspend_clears_and_resume_restarts_the_window() {
        let start = Instant::now();
        let mut state = state_at(start);
        assert!(state.tick(start + QUIET_AFTER).is_some());

        assert_eq!(state.suspend(), Some(CLEAR));
        assert_eq!(state.tick(start + Duration::from_secs(5)), None);

        let resumed = start + Duration::from_secs(5);
        state.resume(resumed);
        assert_eq!(
            state.tick(resumed + QUIET_AFTER - Duration::from_millis(1)),
            None
        );
        assert!(state.tick(resumed + QUIET_AFTER).is_some());

        state.finish();
        state.resume(start + Duration::from_secs(10));
        assert_eq!(
            state.tick(start + Duration::from_secs(20)),
            None,
            "resume cannot revive a finished turn"
        );
    }

    #[test]
    fn begin_turn_resets_the_previous_phase_and_line_state() {
        let start = Instant::now();
        let mut state = state_at(start);
        state.set_phase(Phase::Retry { attempt: 3 }, start);
        state.note_text("partial", start);
        assert!(state.tick(start + QUIET_AFTER).is_some());
        state.finish();

        let restarted = start + Duration::from_secs(10);
        state.begin_turn(restarted);
        let line = state.tick(restarted + QUIET_AFTER).unwrap();
        assert!(line.contains("waiting for model"), "{line:?}");
        assert!(!line.starts_with('\n'), "the old partial line is forgotten");
        assert_eq!(state.suspend(), Some(CLEAR));
    }
}
