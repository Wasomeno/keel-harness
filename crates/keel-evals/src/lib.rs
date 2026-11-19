//! Behavioral evals for keel.
//!
//! The crate is dev/test-only. It holds a small Python fixture repository, the
//! task prompts and mechanical checks, and a runner that drives
//! [`keel_core::Harness`] directly (never the `keel` binary).
//!
//! Two modes share the same harness setup and the same checks:
//!
//! - **replay** (`tests/replay.rs`) loads one multi-interaction cassette per
//!   task, answers the real client/route/decoder from `keel-testkit`'s
//!   sequential cassette player, and executes the model's tool calls against a
//!   temp copy of the fixture. It runs offline in `cargo test`.
//! - **live** (`tests/live.rs`, `#[ignore]`) records each task through the
//!   existing loopback recorder against NeuralWatt `deepseek-v4.1-flash`, with a
//!   $10 hard cap checked before every model request. It writes the replay
//!   cassettes and a per-task report.
//!
//! Tool output is normalized for replay: the temp workspace root becomes
//! `<workspace>`, `search` result lines are sorted, and the eval shell rejects
//! commands that print machine-dependent filesystem metadata. The model still
//! sees ordinary tool results; only the unstable parts are abstracted, and the
//! product tools in `keel-tools` are unchanged.

pub mod budget;
pub mod check;
pub mod fixture;
pub mod live;
pub mod normalize;
pub mod replay;
pub mod report;
pub mod runner;
pub mod task;

/// Errors from fixture setup, route construction, the harness, and checks.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("eval io: {0}")]
    Io(#[from] std::io::Error),
    #[error("eval json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("eval cassette: {0}")]
    Cassette(#[from] keel_testkit::CassetteError),
    #[error("eval route: {0}")]
    Route(#[from] keel_llm_types::LlmError),
    #[error("eval tool: {0}")]
    Tool(#[from] keel_tools::ToolError),
    #[error("eval config: {0}")]
    Config(String),
    #[error("eval task `{0}` does not exist")]
    UnknownTask(String),
    #[error("eval check `{0}` is missing")]
    MissingCheck(String),
}

pub type Result<T> = std::result::Result<T, EvalError>;

pub use budget::{
    Budget, LIVE_BUDGET_MICROS, NEURALWATT_DEEPSEEK_V4_1_FLASH, Prices, TOKEN_PRICE_SOURCE,
};
pub use check::{CheckOutcome, run_checks};
pub use fixture::Fixture;
pub use report::{Mode, RunReport, TaskReport, TaskStatus};
pub use runner::{SuiteExecutor, run_suite};
pub use task::{Check, TASKS, Task, task_by_id};
