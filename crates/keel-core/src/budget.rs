//! Progress accounting, stall detection and the soft turn budget.
//!
//! Benchmark runs spend most of their turns on observation. This module makes
//! that spending visible and stops a run that is not converging instead of
//! letting it burn the whole turn cap. It is deliberately deterministic:
//!
//! - progress is one of a small set of observable events, never a judgement
//!   about the model's plan;
//! - every threshold is a count with a documented default;
//! - a stall has one signal, one last observation and one next step;
//! - the soft budget only speaks when no file has changed, so a run that is
//!   already testing an edit is never interrupted.
//!
//! Normal interactive `keel chat` leaves all of this off. A benchmark runner
//! opts in with [`BudgetConfig::benchmark`] (CLI `--budget-discipline`) and may
//! override individual thresholds with `KEEL_STALL_*` or the builder.
//!
//! Phases: a run starts in [`ProgressPhase::Exploring`] and moves to
//! [`ProgressPhase::Implementing`] after the first successful `write`, `edit`
//! or `apply_patch`. Counters are kept for the whole run and for each phase.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::test_loop::{TestOutcome, TestRun};

/// Prefix of the one soft-budget user message.
pub const RESERVE_PROMPT_PREFIX: &str = "Budget check:";

/// Prefix of the user message that carries a stall diagnosis into history so a
/// `--resume` retry can see it.
pub const STALL_PROMPT_PREFIX: &str = "[keel stall]";

/// At most this many distinct tool results are remembered. Older hashes are
/// deliberately forgotten rather than letting the set grow without bound; a
/// result that is new after the cap is treated as not new, which can only make
/// the no-progress stall more eager, never less.
const MAX_DISTINCT_RESULTS: usize = 512;

/// At most this many workspace entries are inspected per scan. A change past
/// the cap is not observed by the scan, so the mutation counters remain the
/// primary signal. Skipping these directories keeps SCM and dependency trees
/// from dominating the walk.
const MAX_SCAN_ENTRIES: usize = 12_000;
const SKIP_DIRECTORIES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".nox",
];

/// A count-based stall rule. `0` disables the rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BudgetConfig {
    /// N: consecutive completed turns with no mutation, workspace change,
    /// test improvement or new tool result. Default 8 in the benchmark preset.
    pub no_progress_turns: usize,
    /// K: consecutive identical tool calls (name + raw arguments). Default 3.
    pub repeated_calls: usize,
    /// M: visible test runs without the failing-name set shrinking. Default 3.
    pub unshrunk_test_runs: usize,
    /// Turns left below which one soft-budget message is injected when no
    /// mutation exists yet. `0` disables the message. Default 5.
    pub reserve_turns: usize,
    /// Stop a model `EndTurn` that has no mutation, or has a mutation and no
    /// visible test while `expect_tests` is set. Default on in the preset.
    pub premature_finish: bool,
    /// The adapter discovered a visible test command in the repository.
    pub expect_tests: bool,
}

impl BudgetConfig {
    /// The benchmark preset. `keel chat --budget-discipline` starts here and
    /// then applies `KEEL_STALL_*` overrides.
    pub const fn benchmark() -> Self {
        Self {
            no_progress_turns: 8,
            repeated_calls: 3,
            unshrunk_test_runs: 3,
            reserve_turns: 5,
            premature_finish: true,
            expect_tests: false,
        }
    }

    /// Whether any guard or the soft-budget message is active. `expect_tests`
    /// alone is not a guard; it only narrows the premature-finish check.
    pub const fn is_enabled(&self) -> bool {
        self.no_progress_turns > 0
            || self.repeated_calls > 0
            || self.unshrunk_test_runs > 0
            || self.reserve_turns > 0
            || self.premature_finish
    }

    /// Reads `KEEL_STALL_*` from the process environment and applies the
    /// values on top of `self`. A present variable enables its rule by setting
    /// a non-zero threshold; the caller decides whether the baseline is the
    /// benchmark preset or [`BudgetConfig::default`].
    pub fn from_env() -> Result<Self, String> {
        Self::default().with_env(|name| std::env::var(name).ok())
    }

    /// Testable form of [`BudgetConfig::from_env`]. `lookup` returns `None` for
    /// an absent variable.
    pub fn with_env<F>(mut self, lookup: F) -> Result<Self, String>
    where
        F: Fn(&str) -> Option<String>,
    {
        if let Some(value) = env_usize(&lookup, "KEEL_STALL_NO_PROGRESS_TURNS")? {
            self.no_progress_turns = value;
        }
        if let Some(value) = env_usize(&lookup, "KEEL_STALL_REPEATED_CALLS")? {
            self.repeated_calls = value;
        }
        if let Some(value) = env_usize(&lookup, "KEEL_STALL_UNSHRUNK_TEST_RUNS")? {
            self.unshrunk_test_runs = value;
        }
        if let Some(value) = env_usize(&lookup, "KEEL_STALL_RESERVE_TURNS")? {
            self.reserve_turns = value;
        }
        if let Some(value) = env_bool(&lookup, "KEEL_STALL_PREMATURE_FINISH")? {
            self.premature_finish = value;
        }
        if let Some(value) = env_bool(&lookup, "KEEL_STALL_EXPECT_TESTS")? {
            self.expect_tests = value;
        }
        Ok(self)
    }
}

fn env_value<F>(lookup: &F, name: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(name).map(|value| value.trim().to_owned())
}

fn env_usize<F>(lookup: &F, name: &str) -> Result<Option<usize>, String>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(value) = env_value(lookup, name) else {
        return Ok(None);
    };
    value.parse::<usize>().map(Some).map_err(|_| {
        format!("{name} must be a non-negative integer number of turns, got `{value}`")
    })
}

fn env_bool<F>(lookup: &F, name: &str) -> Result<Option<bool>, String>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(value) = env_value(lookup, name) else {
        return Ok(None);
    };
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" => Ok(Some(false)),
        _ => Err(format!("{name} must be 0 or 1, got `{value}`")),
    }
}

/// Which half of the run a counter belongs to. A run starts exploring and
/// moves to implementing after the first successful file mutation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressPhase {
    #[default]
    Exploring,
    Implementing,
}

impl ProgressPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exploring => "exploring",
            Self::Implementing => "implementing",
        }
    }
}

/// Bounded counters for one phase or for the whole run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseProgress {
    pub turns: usize,
    pub mutations: usize,
    pub workspace_changes: usize,
    pub test_runs: usize,
    pub test_improvements: usize,
    pub new_information: usize,
}

/// The public snapshot emitted in [`RunEvent::Progress`](crate::RunEvent) and
/// in [`RunOutcome`](crate::RunOutcome). Every field saturates rather than
/// wrapping, and the set of remembered results is capped.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressCounters {
    pub run: PhaseProgress,
    pub exploring: PhaseProgress,
    pub implementing: PhaseProgress,
    pub phase: ProgressPhase,
    /// Current consecutive completed turns with no progress signal.
    pub idle_turns: usize,
    /// Total identical tool calls observed after the first in a streak.
    pub repeated_calls: usize,
    /// The current identical-call streak.
    pub repeated_call_streak: usize,
    /// Visible test runs since the failing-name set last shrank or passed.
    pub test_runs_without_shrink: usize,
    /// Distinct tool results remembered, capped at `MAX_DISTINCT_RESULTS`.
    pub distinct_results: usize,
    /// Soft-budget messages injected by the harness.
    pub soft_budget_prompts: usize,
    /// Pre-final verification prompts injected by the test loop.
    pub verify_prompts: usize,
}

impl ProgressCounters {
    /// Every nudge the harness injected: the soft-budget message and the
    /// test-loop verification message.
    pub const fn nudges(&self) -> usize {
        self.soft_budget_prompts.saturating_add(self.verify_prompts)
    }
}

/// A detected stall shape. The label is part of the run's stop reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallSignal {
    NoProgress,
    RepeatedCall,
    TestFailuresUnshrunk,
}

impl StallSignal {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoProgress => "no_progress",
            Self::RepeatedCall => "repeated_call",
            Self::TestFailuresUnshrunk => "test_failures_unshrunk",
        }
    }

    /// The stop label used by the CLI status line and the benchmark trace.
    pub const fn stop_label(self) -> &'static str {
        match self {
            Self::NoProgress => "stalled_no_progress",
            Self::RepeatedCall => "stalled_repeated_call",
            Self::TestFailuresUnshrunk => "stalled_test_failures_unshrunk",
        }
    }
}

/// Why an `EndTurn` was rejected as a premature finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrematureReason {
    /// The run produced no successful file mutation.
    NoChanges,
    /// The run mutated a file but never ran a visible test while the adapter
    /// had discovered a test command.
    NoVisibleTest,
}

impl PrematureReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoChanges => "no_changes",
            Self::NoVisibleTest => "no_visible_test",
        }
    }

    pub const fn stop_label(self) -> &'static str {
        match self {
            Self::NoChanges => "premature_finish_no_changes",
            Self::NoVisibleTest => "premature_finish_no_test",
        }
    }
}

/// One stall diagnosis: which rule fired, what it last saw, and the cheapest
/// concrete next step for the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StallDetail {
    pub signal: StallSignal,
    pub observed: String,
    pub next_step: String,
}

impl StallDetail {
    /// The one-sentence diagnosis stored in `RunOutcome::stop_detail`, printed
    /// by the CLI and recorded in the benchmark trace.
    pub fn message(&self) -> String {
        format!(
            "stall ({}): {}. Cheapest next step: {}",
            self.signal.as_str(),
            self.observed,
            self.next_step
        )
    }

    /// The message deliberately placed in history, so a `--resume` retry sees
    /// the same diagnosis the trace does.
    pub fn history_message(&self) -> String {
        format!("{STALL_PROMPT_PREFIX} {}", self.message())
    }
}

/// Mutable per-run accounting. One instance lives on the harness and is reset
/// at the start of every `run`.
#[derive(Debug)]
pub(crate) struct BudgetState {
    config: BudgetConfig,
    counters: ProgressCounters,
    idle_turns: usize,
    last_call_key: Option<u64>,
    last_call_name: String,
    last_call_summary: String,
    repeat_streak: usize,
    last_failing: BTreeSet<String>,
    unshrunk_runs: usize,
    seen_results: HashSet<u64>,
    reserve_sent: bool,
    workspace_before: Option<u64>,
    turn_progress: bool,
}

impl BudgetState {
    pub(crate) fn new(config: BudgetConfig) -> Self {
        Self {
            config,
            counters: ProgressCounters::default(),
            idle_turns: 0,
            last_call_key: None,
            last_call_name: String::new(),
            last_call_summary: String::new(),
            repeat_streak: 0,
            last_failing: BTreeSet::new(),
            unshrunk_runs: 0,
            seen_results: HashSet::new(),
            reserve_sent: false,
            workspace_before: None,
            turn_progress: false,
        }
    }

    pub(crate) const fn config(&self) -> &BudgetConfig {
        &self.config
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.config.is_enabled()
    }

    pub(crate) fn counters(&self) -> &ProgressCounters {
        &self.counters
    }

    pub(crate) fn mutations(&self) -> usize {
        self.counters.run.mutations
    }

    pub(crate) fn test_runs(&self) -> usize {
        self.counters.run.test_runs
    }

    /// Resets all per-run state. Configuration is kept.
    pub(crate) fn reset(&mut self) {
        self.counters = ProgressCounters::default();
        self.idle_turns = 0;
        self.last_call_key = None;
        self.last_call_name.clear();
        self.last_call_summary.clear();
        self.repeat_streak = 0;
        self.last_failing.clear();
        self.unshrunk_runs = 0;
        self.seen_results.clear();
        self.reserve_sent = false;
        self.workspace_before = None;
        self.turn_progress = false;
    }

    /// Starts a turn and takes the workspace baseline.
    pub(crate) fn begin_turn(&mut self, root: &Path) {
        self.turn_progress = false;
        self.workspace_before = if self.is_enabled() {
            workspace_fingerprint(root)
        } else {
            None
        };
    }

    /// A fresh workspace scan, used after a turn that executed tools.
    pub(crate) fn scan(&self, root: &Path) -> Option<u64> {
        if self.is_enabled() {
            workspace_fingerprint(root)
        } else {
            None
        }
    }

    /// Records one finished tool call. `test_run` is the parsed visible-test
    /// result when the call was a recognised test command.
    pub(crate) fn observe_call(
        &mut self,
        name: &str,
        arguments: &str,
        summary: &str,
        result: &str,
        is_error: bool,
        test_run: Option<&TestRun>,
    ) {
        if !self.is_enabled() {
            return;
        }

        let key = call_key(name, arguments);
        if self.last_call_key == Some(key) {
            self.repeat_streak = self.repeat_streak.saturating_add(1);
            saturating_add(&mut self.counters.repeated_calls, 1);
        } else {
            self.repeat_streak = 1;
            self.last_call_key = Some(key);
        }
        self.last_call_name.clear();
        self.last_call_name.push_str(name);
        self.last_call_summary.clear();
        self.last_call_summary.push_str(&short(summary, 140));
        self.counters.repeated_call_streak = self.repeat_streak;

        if is_mutation_tool(name) && !is_error {
            bump_phase(&mut self.counters, |phase| &mut phase.mutations, 1);
            self.counters.phase = ProgressPhase::Implementing;
            self.turn_progress = true;
            // A mutation is real progress even if the model immediately repeats
            // the same test; the retry is no longer an identical loop.
            self.repeat_streak = 0;
            self.counters.repeated_call_streak = 0;
        }

        let result_key = result_key(name, result);
        if self.seen_results.contains(&result_key) {
            // A repeat is not new information.
        } else if self.seen_results.len() < MAX_DISTINCT_RESULTS {
            self.seen_results.insert(result_key);
            self.counters.distinct_results = self.seen_results.len();
            bump_phase(&mut self.counters, |phase| &mut phase.new_information, 1);
            self.turn_progress = true;
        } else {
            self.counters.distinct_results = self.seen_results.len();
        }

        if let Some(run) = test_run {
            self.observe_test_run(run);
        }
    }

    fn observe_test_run(&mut self, run: &TestRun) {
        bump_phase(&mut self.counters, |phase| &mut phase.test_runs, 1);

        let new_set: BTreeSet<String> = run.failing_tests.iter().cloned().collect();
        let improved = match run.outcome {
            TestOutcome::Passed => true,
            TestOutcome::Failed => {
                // Only a strict, non-empty subset is a real shrink. An empty
                // set on a failed run usually means the output could not be
                // parsed, which is not evidence of improvement.
                !new_set.is_empty()
                    && !self.last_failing.is_empty()
                    && new_set.len() < self.last_failing.len()
                    && new_set.is_subset(&self.last_failing)
            }
        };

        if improved {
            bump_phase(&mut self.counters, |phase| &mut phase.test_improvements, 1);
            self.unshrunk_runs = 0;
            self.counters.test_runs_without_shrink = 0;
            self.turn_progress = true;
            self.repeat_streak = 0;
            self.counters.repeated_call_streak = 0;
        } else if run.outcome == TestOutcome::Failed {
            self.unshrunk_runs = self.unshrunk_runs.saturating_add(1);
            self.counters.test_runs_without_shrink = self.unshrunk_runs;
        }

        self.last_failing = new_set;
    }

    /// Completes the turn. `workspace_after` is `None` when no tools ran, in
    /// which case the workspace cannot have changed and no second scan is
    /// needed.
    pub(crate) fn finish_turn(&mut self, workspace_after: Option<u64>) {
        if !self.is_enabled() {
            return;
        }
        if let (Some(before), Some(after)) = (self.workspace_before, workspace_after) {
            if before != after {
                bump_phase(&mut self.counters, |phase| &mut phase.workspace_changes, 1);
                self.turn_progress = true;
            }
        }

        saturating_add(&mut self.counters.run.turns, 1);
        match self.counters.phase {
            ProgressPhase::Exploring => {
                saturating_add(&mut self.counters.exploring.turns, 1);
            }
            ProgressPhase::Implementing => {
                saturating_add(&mut self.counters.implementing.turns, 1);
            }
        }

        if self.turn_progress {
            self.idle_turns = 0;
        } else {
            self.idle_turns = self.idle_turns.saturating_add(1);
        }
        self.counters.idle_turns = self.idle_turns;
        self.turn_progress = false;
    }

    pub(crate) fn note_verify_prompt(&mut self) {
        if self.is_enabled() {
            saturating_add(&mut self.counters.verify_prompts, 1);
        }
    }

    /// The stall rule that fires now, if any. Order is specific to general:
    /// repeated call, then unshrunk test failures, then no progress.
    pub(crate) fn stall(&self) -> Option<StallDetail> {
        if !self.is_enabled() {
            return None;
        }
        if self.config.repeated_calls > 0 && self.repeat_streak >= self.config.repeated_calls {
            return Some(StallDetail {
                signal: StallSignal::RepeatedCall,
                observed: format!(
                    "the same tool call `{} {}` was made {} times in a row with identical arguments",
                    self.last_call_name, self.last_call_summary, self.repeat_streak
                ),
                next_step: "change the request or edit the file under test before running it again; repeating it cannot produce new information".to_owned(),
            });
        }
        if self.config.unshrunk_test_runs > 0
            && self.unshrunk_runs >= self.config.unshrunk_test_runs
        {
            let failing = if self.last_failing.is_empty() {
                "no failing test names could be parsed".to_owned()
            } else {
                self.last_failing
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            return Some(StallDetail {
                signal: StallSignal::TestFailuresUnshrunk,
                observed: format!(
                    "the visible failing-test set has not shrunk across {} test runs ({}); last failing: {failing}",
                    self.unshrunk_runs,
                    self.counters.run.test_runs
                ),
                next_step: "fix one failing test or narrow the command to the single test that covers the change, then rerun only that test".to_owned(),
            });
        }
        if self.config.no_progress_turns > 0 && self.idle_turns >= self.config.no_progress_turns {
            return Some(StallDetail {
                signal: StallSignal::NoProgress,
                observed: format!(
                    "{} consecutive completed turns produced no successful write, edit or apply_patch, no workspace change, no visible-test improvement and no new tool result",
                    self.idle_turns
                ),
                next_step: "stop reading and make one concrete edit with write, edit or apply_patch in the file under test, then run the narrowest visible test".to_owned(),
            });
        }
        None
    }

    /// The one soft-budget message, or `None`. It fires only when no mutation
    /// exists, strictly below the reserve, and at most once per run.
    pub(crate) fn reserve_prompt(&mut self, turns_left: usize) -> Option<String> {
        if !self.is_enabled() || self.config.reserve_turns == 0 {
            return None;
        }
        if self.reserve_sent || turns_left == 0 || turns_left >= self.config.reserve_turns {
            return None;
        }
        if self.counters.run.mutations > 0 {
            return None;
        }
        self.reserve_sent = true;
        saturating_add(&mut self.counters.soft_budget_prompts, 1);
        Some(format!(
            "{RESERVE_PROMPT_PREFIX} only {turns_left} turn(s) remain before the harness stops. \
             No file has changed yet. Land the smallest correct edit now with write, edit or \
             apply_patch; stop reading files. Run one narrow visible test only if a turn remains. \
             The harness stops hard at the turn cap even if the workspace is unchanged."
        ))
    }
}

fn bump_phase(
    counters: &mut ProgressCounters,
    field: impl Fn(&mut PhaseProgress) -> &mut usize,
    amount: usize,
) {
    saturating_add(field(&mut counters.run), amount);
    match counters.phase {
        ProgressPhase::Exploring => saturating_add(field(&mut counters.exploring), amount),
        ProgressPhase::Implementing => saturating_add(field(&mut counters.implementing), amount),
    }
}

fn saturating_add(value: &mut usize, amount: usize) {
    *value = value.saturating_add(amount);
}

fn is_mutation_tool(name: &str) -> bool {
    matches!(name, "write" | "edit" | "apply_patch")
}

fn call_key(name: &str, arguments: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    arguments.hash(&mut hasher);
    hasher.finish()
}

fn result_key(name: &str, result: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    result.hash(&mut hasher);
    hasher.finish()
}

fn short(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(max_chars).collect();
    short.push('…');
    short
}

/// A deterministic, order-independent fingerprint of the workspace's file
/// metadata. It is a scan, not a watcher: no filesystem events, no dependency,
/// and no assumption that the workspace is a git repository.
///
/// Paths, file lengths and modification times (or symlink targets) are mixed
/// commutatively, so `read_dir` order does not matter. Errors on individual
/// entries are skipped; a failure to read the workspace root returns `None`.
/// The walk is capped at `MAX_SCAN_ENTRIES`, so a very large tree is only
/// partially observed; the mutation counters remain the primary evidence.
pub(crate) fn workspace_fingerprint(root: &Path) -> Option<u64> {
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut fingerprint = 0u64;
    let mut entries = 0usize;
    let mut root_read = false;

    while let Some(directory) = stack.pop() {
        let Ok(read) = fs::read_dir(&directory) else {
            continue;
        };
        if directory == root {
            root_read = true;
        }
        for entry in read.flatten() {
            entries = entries.saturating_add(1);
            if entries > MAX_SCAN_ENTRIES {
                break;
            }
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(&path);
            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            let mut hasher = DefaultHasher::new();
            relative.hash(&mut hasher);
            if file_type.is_dir() {
                if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                    if SKIP_DIRECTORIES.contains(&name) {
                        continue;
                    }
                }
                stack.push(path);
            } else if file_type.is_symlink() {
                if let Ok(target) = fs::read_link(&path) {
                    target.hash(&mut hasher);
                }
            } else if let Ok(metadata) = entry.metadata() {
                metadata.len().hash(&mut hasher);
                if let Ok(modified) = metadata.modified() {
                    if let Ok(since_epoch) = modified.duration_since(UNIX_EPOCH) {
                        since_epoch.as_nanos().hash(&mut hasher);
                    }
                }
            } else {
                continue;
            }
            fingerprint = fingerprint.wrapping_add(hasher.finish());
        }
        if entries > MAX_SCAN_ENTRIES {
            break;
        }
    }

    if !root_read {
        return None;
    }
    Some(fingerprint ^ (entries as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BudgetConfig {
        BudgetConfig {
            no_progress_turns: 2,
            repeated_calls: 2,
            unshrunk_test_runs: 2,
            reserve_turns: 2,
            premature_finish: false,
            expect_tests: false,
        }
    }

    fn failed(names: &[&str]) -> TestRun {
        TestRun {
            command: "pytest".to_owned(),
            outcome: TestOutcome::Failed,
            failing_tests: names.iter().map(|name| (*name).to_owned()).collect(),
            excerpt: None,
        }
    }

    fn passed() -> TestRun {
        TestRun {
            command: "pytest".to_owned(),
            outcome: TestOutcome::Passed,
            failing_tests: Vec::new(),
            excerpt: None,
        }
    }

    #[test]
    fn the_benchmark_preset_enables_every_rule_and_plain_chat_does_not() {
        assert!(BudgetConfig::benchmark().is_enabled());
        assert!(BudgetConfig::benchmark().premature_finish);
        assert!(!BudgetConfig::default().is_enabled());
        assert!(!BudgetConfig::default().premature_finish);
    }

    #[test]
    fn environment_values_override_the_baseline_and_bad_values_fail() {
        let values = |name: &str| match name {
            "KEEL_STALL_NO_PROGRESS_TURNS" => Some("4".to_owned()),
            "KEEL_STALL_REPEATED_CALLS" => Some("0".to_owned()),
            "KEEL_STALL_UNSHRUNK_TEST_RUNS" => Some("5".to_owned()),
            "KEEL_STALL_RESERVE_TURNS" => Some("7".to_owned()),
            "KEEL_STALL_PREMATURE_FINISH" => Some("0".to_owned()),
            "KEEL_STALL_EXPECT_TESTS" => Some("1".to_owned()),
            _ => None,
        };
        let config = BudgetConfig::benchmark().with_env(values).unwrap();
        assert_eq!(config.no_progress_turns, 4);
        assert_eq!(config.repeated_calls, 0);
        assert_eq!(config.unshrunk_test_runs, 5);
        assert_eq!(config.reserve_turns, 7);
        assert!(!config.premature_finish);
        assert!(config.expect_tests);

        let bad = |name: &str| (name == "KEEL_STALL_NO_PROGRESS_TURNS").then(|| "soon".to_owned());
        assert!(BudgetConfig::default().with_env(bad).is_err());
        let bad_bool =
            |name: &str| (name == "KEEL_STALL_PREMATURE_FINISH").then(|| "maybe".to_owned());
        assert!(BudgetConfig::default().with_env(bad_bool).is_err());
    }

    #[test]
    fn a_repeat_stall_counts_identical_calls_and_resets_after_a_mutation() {
        let mut state = BudgetState::new(config());
        state.observe_call("read", "{\"path\":\"a\"}", "a", "same", false, None);
        state.observe_call("read", "{\"path\":\"a\"}", "a", "same", false, None);
        assert_eq!(state.stall().unwrap().signal, StallSignal::RepeatedCall);

        state.observe_call(
            "write",
            "{\"path\":\"a\",\"content\":\"x\"}",
            "a",
            "wrote a",
            false,
            None,
        );
        assert!(
            state.stall().is_none(),
            "a mutation breaks the identical-call loop"
        );
    }

    #[test]
    fn unshrunk_failures_reset_on_a_strict_subset_or_a_pass() {
        let mut state = BudgetState::new(config());
        state.observe_test_run(&failed(&["a", "b"]));
        state.observe_test_run(&failed(&["a", "b"]));
        assert_eq!(
            state.stall().unwrap().signal,
            StallSignal::TestFailuresUnshrunk
        );

        state.observe_test_run(&failed(&["a"]));
        assert!(state.stall().is_none(), "a strict subset is progress");
        assert_eq!(state.counters().test_runs_without_shrink, 0);
        state.observe_test_run(&passed());
        assert!(state.stall().is_none());
    }

    #[test]
    fn an_empty_failed_set_is_not_treated_as_a_shrink() {
        let mut state = BudgetState::new(config());
        state.observe_test_run(&failed(&["a", "b"]));
        state.observe_test_run(&failed(&[]));
        state.observe_test_run(&failed(&[]));
        assert_eq!(
            state.stall().unwrap().signal,
            StallSignal::TestFailuresUnshrunk
        );
    }

    #[test]
    fn the_reserve_prompt_waits_below_the_reserve_and_needs_no_mutation() {
        let mut state = BudgetState::new(config());
        assert!(state.reserve_prompt(2).is_none(), "2 is not below 2");
        assert!(state.reserve_prompt(1).is_some());
        assert!(state.reserve_prompt(1).is_none(), "it fires once");
        assert_eq!(state.counters().soft_budget_prompts, 1);

        let mut after_mutation = BudgetState::new(config());
        after_mutation.observe_call(
            "write",
            "{\"path\":\"a\",\"content\":\"x\"}",
            "a",
            "wrote a",
            false,
            None,
        );
        assert!(
            after_mutation.reserve_prompt(1).is_none(),
            "a run that already mutated is not interrupted"
        );
    }

    #[test]
    fn a_workspace_scan_sees_a_new_file() {
        let directory = tempfile::tempdir().unwrap();
        let before = workspace_fingerprint(directory.path()).unwrap();
        std::fs::write(directory.path().join("new.txt"), "hello").unwrap();
        let after = workspace_fingerprint(directory.path()).unwrap();
        assert_ne!(before, after);
    }
}
