use std::process::Stdio;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{
    Tool, ToolContext, ToolError, ToolFuture, ToolOutcome, ToolSpec, parse_arguments, tool_spec,
};
use crate::truncate::truncate_output;

pub struct SearchTool;

/// Context is bounded: a large value must not turn one match into thousands
/// of lines.
const MAX_CONTEXT_LINES: usize = 20;

#[derive(Deserialize)]
struct SearchArguments {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    case_sensitive: Option<bool>,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default)]
    before: Option<usize>,
    #[serde(default)]
    after: Option<usize>,
}

impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            "search",
            "Use when: finding where a string, symbol, or regular expression appears in file contents.\n\
             Prefer: `glob` to find files by name; `bash` for formats ripgrep cannot read, such as archives or binary files. Narrow the search with path or glob.\n\
             Returns: `path:line:text` matches, up to max_results, honoring .gitignore. With context, before or after (0-20 lines each), surrounding lines appear as `path-line-text` and groups are separated by `--`, so a match still reads as `path:line:`. If nothing matches, says so and suggests checking the pattern, path, or glob filter.\n\
             On failure: the error names the pattern and the reason (bad regex, bad glob, missing path, timeout), and gives the next step.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Rust-style regular expression."},
                    "path": {"type": "string", "description": "File or directory to search, relative to the workspace root."},
                    "glob": {"type": "string", "description": "Only search files matching this glob."},
                    "case_sensitive": {"type": "boolean", "description": "Defaults to true."},
                    "max_results": {"type": "integer", "description": "Maximum matches to return."},
                    "context": {"type": "integer", "description": "Lines before and after each match, 0-20."},
                    "before": {"type": "integer", "description": "Lines before each match, 0-20; overrides context."},
                    "after": {"type": "integer", "description": "Lines after each match, 0-20; overrides context."}
                },
                "required": ["pattern"]
            }),
        )
    }

    fn summarize(&self, arguments: &Value) -> String {
        format!(
            "search /{}/",
            arguments
                .get("pattern")
                .and_then(Value::as_str)
                .unwrap_or("?")
        )
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn execute<'a>(&'a self, arguments: Value, context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            match search(arguments, context).await {
                Ok(text) => ToolOutcome::ok(text),
                Err(error) => error.into_outcome(),
            }
        })
    }
}

async fn search(arguments: Value, context: &ToolContext) -> Result<String, ToolError> {
    let arguments: SearchArguments = parse_arguments(arguments)?;
    if arguments.pattern.is_empty() {
        return Err(ToolError::InvalidArguments(
            "`pattern` must not be empty; pass a Rust-style regular expression".to_owned(),
        ));
    }

    let base = match arguments.path.as_deref() {
        Some(path) => context.workspace.resolve_existing(path)?,
        None => context.workspace.root().to_path_buf(),
    };
    let search_path = {
        let relative = base.strip_prefix(context.workspace.root()).unwrap_or(&base);
        if relative.as_os_str().is_empty() {
            ".".to_owned()
        } else {
            relative.display().to_string()
        }
    };

    let context_lines = arguments.context.unwrap_or(0).min(MAX_CONTEXT_LINES);
    let before = arguments
        .before
        .unwrap_or(context_lines)
        .min(MAX_CONTEXT_LINES);
    let after = arguments
        .after
        .unwrap_or(context_lines)
        .min(MAX_CONTEXT_LINES);

    let mut command = tokio::process::Command::new("rg");
    command
        .current_dir(context.workspace.root())
        .arg("--json")
        .arg("--color")
        .arg("never")
        .arg("--max-filesize")
        .arg("10M");
    if arguments.case_sensitive == Some(false) {
        command.arg("--ignore-case");
    }
    if let Some(glob) = &arguments.glob {
        command.arg("--glob").arg(glob);
    }
    if before > 0 {
        command.arg("--before-context").arg(before.to_string());
    }
    if after > 0 {
        command.arg("--after-context").arg(after.to_string());
    }
    command
        .arg("--")
        .arg(&arguments.pattern)
        .arg(&search_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ToolError::Message(
                "ripgrep (`rg`) was not found on PATH; install ripgrep, or use `bash` with \
                 grep instead"
                    .to_owned(),
            )
        } else {
            ToolError::Io(format!(
                "could not start ripgrep for pattern `{}`: {error}",
                arguments.pattern
            ))
        }
    })?;

    let output = tokio::select! {
        _ = context.cancel.cancelled() => return Err(ToolError::Cancelled),
        result = tokio::time::timeout(context.limits.timeout, child.wait_with_output()) => {
            match result {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => {
                    return Err(ToolError::Io(format!(
                        "ripgrep failed while searching for `{}`: {error}",
                        arguments.pattern
                    )));
                }
                Err(_) => {
                    return Err(ToolError::Timeout {
                        after: context.limits.timeout,
                        message: format!(
                            "search for pattern `{}` was still running; narrow `path` or \
                             `glob`, or use a simpler pattern",
                            arguments.pattern
                        ),
                    });
                }
            }
        }
    };

    match output.status.code() {
        Some(2) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ToolError::Message(format!(
                "ripgrep rejected the search for `{}`: {}; check the regular expression \
                 and the `glob` filter, and that `path` is readable",
                arguments.pattern,
                stderr.trim()
            )));
        }
        Some(1) => return Ok(no_matches(&arguments.pattern)),
        _ => {}
    }

    let max = arguments.max_results.unwrap_or(200).clamp(1, 2000);
    let wants_context = before > 0 || after > 0;
    let mut rendered = Vec::new();
    let mut rendered_matches = 0usize;
    let mut last_rendered: Option<(String, u64)> = None;
    let mut after_remaining: Option<usize> = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let is_match = match value.get("type").and_then(Value::as_str) {
            Some("match") => true,
            Some("context") => false,
            _ => continue,
        };
        if is_match {
            if rendered_matches >= max {
                break;
            }
            rendered_matches += 1;
            if rendered_matches == max {
                // Keep the after-context of the last reported match, but do
                // not let a later group add context for a match over the cap.
                after_remaining = Some(after);
            }
        } else if let Some(remaining) = after_remaining {
            if remaining == 0 {
                break;
            }
            after_remaining = Some(remaining - 1);
        }

        let path = display_match_path(
            value
                .pointer("/data/path/text")
                .and_then(Value::as_str)
                .unwrap_or("?"),
        );
        let line_number = value
            .pointer("/data/line_number")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let text = value
            .pointer("/data/lines/text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_end();

        if wants_context {
            if let Some((previous_path, previous_line)) = &last_rendered {
                if previous_path != &path || line_number > previous_line + 1 {
                    rendered.push("--".to_owned());
                }
            }
        }
        if is_match {
            rendered.push(format!("{path}:{line_number}:{text}"));
        } else {
            rendered.push(format!("{path}-{line_number}-{text}"));
        }
        last_rendered = Some((path, line_number));
    }

    if rendered_matches == 0 {
        return Ok(no_matches(&arguments.pattern));
    }
    Ok(truncate_output(
        &rendered.join("\n"),
        context.limits.max_output_bytes,
    ))
}

/// Ripgrep reports paths with the platform separator and, for a search rooted
/// at `.`, prefixes them with `./`. Keep that spelling so recorded transcripts
/// still replay, but use `/` on Windows too.
fn display_match_path(path: &str) -> String {
    #[cfg(windows)]
    {
        path.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.to_owned()
    }
}

fn no_matches(pattern: &str) -> String {
    format!(
        "no matches for `{pattern}`; check the pattern, `path`, and `glob` filter, or use \
         `glob` to find the file first"
    )
}
