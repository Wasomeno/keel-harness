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
            "Run a shell command with `bash -lc` in the workspace. Returns the exit code, stdout and stderr.",
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

    let child = command
        .spawn()
        .map_err(|error| ToolError::Io(format!("failed to start bash: {error}")))?;

    let output = tokio::select! {
        _ = context.cancel.cancelled() => return Err(ToolError::Cancelled),
        result = tokio::time::timeout(timeout, child.wait_with_output()) => {
            match result {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => return Err(ToolError::Io(format!("bash failed: {error}"))),
                Err(_) => return Err(ToolError::Timeout(timeout)),
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

    Ok(ToolOutcome {
        text: truncate_output(&text, context.limits.max_output_bytes),
        is_error: !output.status.success(),
    })
}
