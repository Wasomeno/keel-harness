//! Mechanical checks: a command that must exit 0, or file assertions.
//!
//! Command checks point at `fixtures/checks/<script>` outside the workspace.
//! The workspace also gets a copy at `tests/check.py` so the agent can run it,
//! but grading always uses the copy the agent cannot edit.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::fixture::checks_dir;
use crate::task::{Check, Task};

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
        let outcome = match check {
            Check::Command { script } => run_command(script, workspace_root),
            Check::FileContains { path, needle } => {
                check_file_contains(path, needle, workspace_root)
            }
        };
        if !outcome.passed {
            return outcome;
        }
    }
    CheckOutcome::passed()
}

fn run_command(script: &str, workspace_root: &Path) -> CheckOutcome {
    let path = checks_dir().join(script);
    let output = match Command::new("python3")
        .arg(&path)
        .current_dir(workspace_root)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return CheckOutcome::failed(format!("{script}: could not start python3: {error}"));
        }
    };

    if output.status.success() {
        return CheckOutcome::passed();
    }

    let mut detail = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !stderr.is_empty() {
        if !detail.is_empty() {
            detail.push_str("; ");
        }
        detail.push_str(&stderr);
    }
    if detail.is_empty() {
        detail = format!("{script}: exit {:?}", output.status.code());
    }
    CheckOutcome::failed(trim_detail(&detail))
}

fn check_file_contains(path: &str, needle: &str, workspace_root: &Path) -> CheckOutcome {
    let full = workspace_root.join(path);
    match fs::read_to_string(&full) {
        Ok(text) if text.contains(needle) => CheckOutcome::passed(),
        Ok(_) => CheckOutcome::failed(format!("{path} does not contain {needle:?}")),
        Err(error) => CheckOutcome::failed(format!("could not read {path}: {error}")),
    }
}

fn trim_detail(detail: &str) -> String {
    const LIMIT: usize = 400;
    let mut trimmed: String = detail.chars().take(LIMIT).collect();
    if trimmed.len() < detail.len() {
        trimmed.push('…');
    }
    trimmed
}
