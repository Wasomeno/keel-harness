use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{
    Tool, ToolContext, ToolError, ToolFuture, ToolOutcome, ToolSpec, parse_arguments, tool_spec,
};
use crate::truncate::truncate_output;

pub struct BashTool;

#[derive(Deserialize)]
struct BashArguments {
    command: String,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            "bash",
            "Use when: you need to run tests, builds, formatters, version control, or another command no structured tool covers.\n\
             Prefer: `read`, `write`, `edit`, `glob`, and `search` for file work; avoid `find`, `ls`, `stat`, `du`, and `tree` when `glob` can list files.\n\
             Returns: `exit: N` followed by stdout and stderr; large output is truncated in the middle by the harness limit.\n\
             On failure: a non-zero exit is an error result with the code and output plus a retry hint; a timeout names the command and limit; cancellation says the run was cancelled.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Command line to run."},
                    "workdir": {"type": "string", "description": "Working directory relative to the workspace root."},
                    "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds, capped by the harness limit."}
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
            format!("bash: {short}…")
        } else {
            format!("bash: {first_line}")
        }
    }

    fn execute<'a>(&'a self, arguments: Value, context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            match run(arguments, context).await {
                Ok(outcome) => outcome,
                Err(error) => error.into_outcome(),
            }
        })
    }
}

async fn run(arguments: Value, context: &ToolContext) -> Result<ToolOutcome, ToolError> {
    let arguments: BashArguments = parse_arguments(arguments)?;
    let directory = context
        .workspace
        .resolve_dir(arguments.workdir.as_deref())?;

    let timeout = arguments
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(context.limits.timeout)
        .min(context.limits.timeout);

    let mut command = tokio::process::Command::new("bash");
    command
        .arg("-lc")
        .arg(&arguments.command)
        .current_dir(&directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = command.spawn().map_err(|error| {
        ToolError::Io(format!(
            "could not start bash for command `{}`: {error}; check that bash is installed \
             and on PATH",
            arguments.command
        ))
    })?;

    let output = tokio::select! {
        _ = context.cancel.cancelled() => return Err(ToolError::Cancelled),
        result = tokio::time::timeout(timeout, child.wait_with_output()) => {
            match result {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => {
                    return Err(ToolError::Io(format!(
                        "bash failed while running `{}`: {error}; the command may have been interrupted",
                        arguments.command
                    )));
                }
                Err(_) => {
                    return Err(ToolError::Message(format!(
                        "bash timed out after {timeout:?} while running `{}`; shorten the \
                         command, or raise timeout_ms (the harness cap is {:?})",
                        arguments.command, context.limits.timeout
                    )));
                }
            }
        }
    };

    let code = output.status.code().unwrap_or(-1);
    let mut text = format!("exit: {code}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.trim().is_empty() {
        text.push_str("\nstdout:\n");
        text.push_str(stdout.trim_end());
    }
    if !stderr.trim().is_empty() {
        text.push_str("\nstderr:\n");
        text.push_str(stderr.trim_end());
    }
    if !output.status.success() {
        text.push_str("\ncommand failed; read stderr and fix the command or the code, then retry");
    }

    Ok(ToolOutcome {
        text: truncate_output(&text, context.limits.max_output_bytes),
        is_error: !output.status.success(),
    })
}
