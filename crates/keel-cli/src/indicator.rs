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
        match self {
            Phase::Waiting => "waiting for model".to_owned(),
            Phase::Tool(name) => format!("running {name}"),
            Phase::Retry { attempt } => format!("retrying (attempt {attempt})"),
        }
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
    }

    /// Records a phase change and restarts the silence window.
    pub fn set_phase(&mut self, phase: Phase, now: Instant) {
        self.phase = phase;
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

    /// Returns whether the visible line already terminated the last text line.
    pub fn broke_line(&self) -> bool {
        self.broke_line
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
    state: Arc<Mutex<IndicatorState>>,
    ticker: Option<tokio::task::JoinHandle<()>>,
    interrupt: Option<tokio::task::JoinHandle<()>>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, IndicatorState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A drop guard for the ticker task and the visible line.
impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(ticker) = &self.ticker {
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

impl Default for Indicator {
    fn default() -> Self {
        Self::new()
    }
}

impl Indicator {
    /// Creates the indicator, gated on stderr being a terminal.
    ///
    /// The ticker and the Ctrl-C handler only start on a terminal, so piped
    /// runs do no extra work.
    pub fn new() -> Self {
        let enabled = io::stderr().is_terminal();
        let line_terminal = io::stdout().is_terminal();
        let state = Arc::new(Mutex::new(IndicatorState::new(
            enabled,
            line_terminal,
            Instant::now(),
        )));
        let ticker = enabled.then(|| spawn_ticker(Arc::clone(&state)));
        let interrupt = enabled.then(|| spawn_interrupt(Arc::clone(&state)));
        Self {
            inner: Arc::new(Inner {
                state,
                ticker,
                interrupt,
            }),
        }
    }

    /// Marks the start of a turn.
    pub fn begin_turn(&self) {
        let mut state = self.inner.lock();
        let now = Instant::now();
        if let Some(clear) = state.clear() {
            write_stderr(clear);
        }
        state.begin_turn(now);
    }

    /// Names the phase the turn is waiting in.
    pub fn set_phase(&self, phase: Phase) {
        self.inner.lock().set_phase(phase, Instant::now());
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
        if !state.broke_line() {
            write_stdout("\n");
        }
        state.note_line(Instant::now());
    }

    /// Writes one complete line to stderr, clearing the line first.
    pub fn stderr_line(&self, line: fmt::Arguments<'_>) {
        let mut state = self.inner.lock();
        if let Some(clear) = state.clear() {
            write_stderr(clear);
        }
        let mut stderr = io::stderr().lock();
        let _ = stderr.write_fmt(line);
        let _ = stderr.write_all(b"\n");
        let _ = stderr.flush();
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
        let _ = stdout.write_fmt(line);
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
        drop(stdout);
        state.note_line(Instant::now());
    }

    /// Stops the line at the end of a turn, clearing it if visible.
    pub fn finish(&self) {
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

fn write_stdout(text: &str) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

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
        tool.set_phase(Phase::Tool("bash".to_owned()), start);
        let line = tool.tick(start + QUIET_AFTER).unwrap();
        assert!(line.contains("running bash"), "{line:?}");

        let mut retry = state_at(start);
        retry.set_phase(Phase::Retry { attempt: 2 }, start);
        let line = retry.tick(start + QUIET_AFTER).unwrap();
        assert!(line.contains("retrying (attempt 2)"), "{line:?}");
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
        assert!(state.broke_line(), "the indicator opened a fresh line");

        assert_eq!(state.clear(), Some(CLEAR));
        state.note_text("more", start + QUIET_AFTER);
        assert!(!state.broke_line());
    }

    #[test]
    fn a_redirected_stdout_does_not_mark_a_terminal_line() {
        let start = Instant::now();
        let mut state = IndicatorState::new(true, false, start);
        state.begin_turn(start);
        state.note_text("partial", start);

        let line = state.tick(start + QUIET_AFTER).unwrap();
        assert!(line.starts_with('\r'), "{line:?}");
        assert!(!state.broke_line());
    }
}
