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

    let program = bash_program();
    let mut command = tokio::process::Command::new(&program);
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
            "could not start bash ({}) for command `{}`: {error}; check that bash is \
             installed and on PATH",
            program.to_string_lossy(),
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
                    let hint = if timeout < context.limits.timeout {
                        format!(
                            "shorten the command, or raise timeout_ms (the harness cap is {:?})",
                            context.limits.timeout
                        )
                    } else {
                        "this is the harness cap, so shorten the command \
                         (raising timeout_ms will not help)"
                            .to_owned()
                    };
                    return Err(ToolError::Timeout {
                        after: timeout,
                        message: format!("bash was running `{}`; {hint}", arguments.command),
                    });
                }
            }
        }
    };

    let code = output.status.code().unwrap_or(-1);
    let mut text = format!("exit: {code}");

    let stdout = normalize_newlines(&output.stdout);
    let stderr = normalize_newlines(&output.stderr);
    if !stdout.trim().is_empty() {
        text.push_str("\nstdout:\n");
        text.push_str(stdout.trim_end());
    }
    if !stderr.trim().is_empty() {
        text.push_str("\nstderr:\n");
        text.push_str(stderr.trim_end());
    }
    if !output.status.success() {
        text.push_str(
            "\nIf this exit was not expected, read stderr and fix the command or the code, \
             then retry",
        );
    }

    Ok(ToolOutcome {
        text: truncate_output(&text, context.limits.max_output_bytes),
        is_error: !output.status.success(),
    })
}

/// Command output keeps LF on every platform. Git Bash passes through what a
/// Windows program writes, and Python's text mode turns `\n` into `\r\n`;
/// transcript text should not depend on that.
fn normalize_newlines(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// The shell the `bash` tool runs.
///
/// On Windows, `bash` on PATH is usually `C:\Windows\System32\bash.exe`, the
/// WSL launcher, which fails when no distribution is installed. Git for
/// Windows ships a real bash, so prefer it and let `KEEL_BASH` override it.
fn bash_program() -> std::ffi::OsString {
    #[cfg(windows)]
    {
        windows_bash::program()
    }
    #[cfg(not(windows))]
    {
        std::ffi::OsString::from("bash")
    }
}

#[cfg(windows)]
mod windows_bash {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    pub(super) fn program() -> OsString {
        if let Some(explicit) = std::env::var_os("KEEL_BASH").filter(|value| !value.is_empty()) {
            return explicit;
        }
        if let Some(bash) = candidates().into_iter().find(|path| path.is_file()) {
            return bash.into_os_string();
        }
        OsString::from("bash")
    }

    fn candidates() -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        for (variable, relative) in [
            ("ProgramFiles", "Git/bin/bash.exe"),
            ("ProgramFiles(x86)", "Git/bin/bash.exe"),
            ("LOCALAPPDATA", "Programs/Git/bin/bash.exe"),
        ] {
            if let Some(root) = std::env::var_os(variable) {
                candidates.push(PathBuf::from(root).join(relative));
            }
        }
        // A portable Git may live elsewhere; find `git.exe` on PATH and use the
        // `bin/bash.exe` beside its install root (`Git/cmd/git.exe`,
        // `Git/bin/git.exe` or `Git/mingw64/bin/git.exe`).
        if let Some(path) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&path) {
                for name in ["git.exe", "git"] {
                    let git = directory.join(name);
                    if !git.is_file() {
                        continue;
                    }
                    if let Some(install) = git.parent().and_then(Path::parent) {
                        candidates.push(install.join("bin").join("bash.exe"));
                    }
                }
            }
        }
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolLimits;
    use crate::workspace::Workspace;
    use serde_json::json;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn context(directory: &tempfile::TempDir, timeout: Duration) -> ToolContext {
        ToolContext {
            workspace: Workspace::open(directory.path()).unwrap(),
            cancel: CancellationToken::new(),
            limits: ToolLimits {
                timeout,
                ..ToolLimits::default()
            },
        }
    }

    #[test]
    fn command_output_uses_lf_on_every_platform() {
        assert_eq!(
            normalize_newlines(b"first\r\nsecond\nthird\rfour"),
            "first\nsecond\nthird\rfour"
        );
    }

    #[tokio::test]
    async fn a_timeout_is_a_typed_tool_error() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, Duration::from_millis(100));

        let error = run(json!({"command": "sleep 5"}), &context)
            .await
            .expect_err("the command outlives the limit");

        assert!(matches!(error, ToolError::Timeout { .. }), "{error:?}");
    }
}
