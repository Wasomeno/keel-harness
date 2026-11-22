//! Per-task and per-run reports.

use std::path::Path;

use keel_core::RunStop;
use keel_llm_types::Usage;

use crate::budget::{Prices, TOKEN_PRICE_SOURCE};
use crate::check::CheckOutcome;
use crate::task::Task;
use crate::{EvalError, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Replay,
    Live,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Replay => "replay (offline)",
            Self::Live => "live recording",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Passed,
    Failed,
    /// The budget stopped the task before its next model request.
    StoppedBudget,
    /// The budget was already exhausted when this task's turn came.
    NotRunBudget,
}

impl TaskStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Passed => "pass",
            Self::Failed => "fail",
            Self::StoppedBudget => "stopped (budget)",
            Self::NotRunBudget => "not run (budget)",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TaskReport {
    pub id: String,
    pub status: TaskStatus,
    pub turns: usize,
    pub tool_calls: usize,
    pub usage: Usage,
    pub stop: Option<RunStop>,
    pub check: CheckOutcome,
    /// Spend attributed to this task, when the mode tracks spend.
    pub spend_micros: Option<u64>,
    pub cassette: Option<String>,
    /// A harness or cassette error, when there was one.
    pub error: Option<String>,
}

impl TaskReport {
    pub fn not_run_budget(task: &Task) -> Self {
        Self {
            id: task.id.to_owned(),
            status: TaskStatus::NotRunBudget,
            turns: 0,
            tool_calls: 0,
            usage: Usage::default(),
            stop: None,
            check: CheckOutcome {
                passed: false,
                detail: "not run (budget)".to_owned(),
            },
            spend_micros: Some(0),
            cassette: None,
            error: None,
        }
    }

    pub fn detail(&self) -> String {
        match (&self.error, self.check.passed) {
            (Some(error), _) => format!("error: {error}"),
            (None, true) => "pass".to_owned(),
            (None, false) => self.check.detail.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RunReport {
    pub mode: Mode,
    pub model: String,
    pub cap_micros: Option<u64>,
    pub prices: Prices,
    pub tasks: Vec<TaskReport>,
    pub spent_micros: Option<u64>,
}

impl RunReport {
    pub fn count(&self, status: TaskStatus) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.status == status)
            .count()
    }

    pub fn passed(&self) -> usize {
        self.count(TaskStatus::Passed)
    }

    pub fn total_tokens(&self) -> (u64, u64) {
        self.tasks.iter().fold((0, 0), |(input, output), task| {
            (
                input + task.usage.input_tokens,
                output + task.usage.output_tokens,
            )
        })
    }

    pub fn to_markdown(&self) -> String {
        let mut text = String::new();
        text.push_str("# keel M1 behavioral eval report\n\n");
        text.push_str(&format!("- Mode: {}\n", self.mode.label()));
        text.push_str(&format!("- Model: {}\n", self.model));
        match (self.cap_micros, self.spent_micros) {
            (Some(cap), Some(spent)) => {
                text.push_str(&format!(
                    "- Budget: {} hard cap; spent {}\n",
                    usd(cap),
                    usd(spent)
                ));
            }
            _ => text.push_str("- Budget: not applicable (replay)\n"),
        }
        text.push_str(&format!(
            "- Prices: {} input, {} cached input, {} output per 1M tokens\n",
            usd(self.prices.input_micros_per_million),
            usd(self.prices.cached_input_micros_per_million),
            usd(self.prices.output_micros_per_million),
        ));
        text.push_str(&format!("- Price source: {TOKEN_PRICE_SOURCE}\n"));
        let (input, output) = self.total_tokens();
        text.push_str(&format!(
            "- Result: {} passed, {} failed, {} stopped by budget, {} not run; \
             {input} input tokens, {output} output tokens\n\n",
            self.count(TaskStatus::Passed),
            self.count(TaskStatus::Failed),
            self.count(TaskStatus::StoppedBudget),
            self.count(TaskStatus::NotRunBudget),
        ));

        text.push_str(
            "| task | result | turns | tool calls | input tok | output tok | reasoning tok | stop | check | spend |\n",
        );
        text.push_str("|---|---|---:|---:|---:|---:|---:|---|---|---:|\n");
        for task in &self.tasks {
            text.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                task.id,
                task.status.label(),
                task.turns,
                task.tool_calls,
                task.usage.input_tokens,
                task.usage.output_tokens,
                task.usage.reasoning_tokens,
                stop_label(task.stop),
                escape_cell(&task.detail()),
                task.spend_micros
                    .map(usd)
                    .unwrap_or_else(|| "n/a".to_owned()),
            ));
        }

        text.push_str(
            "\nReplay is offline and comes from one cassette per task: the model \
             responses are recorded, while the tool calls execute for real against a \
             temp fixture copy. Live mode records through keel-testkit's loopback \
             recorder and stops before the request that would cross the cap.\n",
        );
        text
    }

    pub fn write_markdown(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_markdown())?;
        Ok(())
    }

    pub fn write_json(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::json!({
            "mode": match self.mode { Mode::Replay => "replay", Mode::Live => "live" },
            "model": self.model,
            "cap_micros": self.cap_micros,
            "spent_micros": self.spent_micros,
            "prices": {
                "input_micros_per_million": self.prices.input_micros_per_million,
                "cached_input_micros_per_million": self.prices.cached_input_micros_per_million,
                "output_micros_per_million": self.prices.output_micros_per_million,
            },
            "tasks": self.tasks.iter().map(|task| serde_json::json!({
                "id": task.id,
                "status": task.status.label(),
                "turns": task.turns,
                "tool_calls": task.tool_calls,
                "input_tokens": task.usage.input_tokens,
                "output_tokens": task.usage.output_tokens,
                "reasoning_tokens": task.usage.reasoning_tokens,
                "stop": stop_label(task.stop),
                "check": task.detail(),
                "spend_micros": task.spend_micros,
                "cassette": task.cassette,
            })).collect::<Vec<_>>(),
        });
        std::fs::write(path, serde_json::to_string_pretty(&json)?).map_err(EvalError::from)
    }
}

pub fn usd(micros: u64) -> String {
    format!("${:.6}", micros as f64 / 1_000_000.0)
}

fn stop_label(stop: Option<RunStop>) -> &'static str {
    match stop {
        Some(RunStop::EndTurn) => "end_turn",
        Some(RunStop::MaxTurns) => "max_turns",
        Some(RunStop::TaskTimeout) => "task_timeout",
        Some(RunStop::Stalled(signal)) => signal.stop_label(),
        Some(RunStop::PrematureFinish(reason)) => reason.stop_label(),
        Some(RunStop::Cancelled) => "cancelled",
        Some(RunStop::Failed) => "failed",
        None => "none",
    }
}

fn escape_cell(text: &str) -> String {
    let one_line = text.replace('\n', " ").replace('|', "\\|");
    let mut shortened: String = one_line.chars().take(120).collect();
    if shortened.len() < one_line.len() {
        shortened.push('…');
    }
    shortened
}
