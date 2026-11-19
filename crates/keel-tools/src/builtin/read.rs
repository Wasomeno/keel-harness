use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{
    Tool, ToolContext, ToolError, ToolFuture, ToolOutcome, ToolSpec, parse_arguments, tool_spec,
};
use crate::truncate::truncate_output;

pub struct ReadTool;

#[derive(Deserialize)]
struct ReadArguments {
    path: String,
    /// 0-based line offset.
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            "read",
            "Use when: you need the contents of a known file, before editing it, or after an edit failed to match.\n\
             Prefer: `search` to find text across files, `glob` to find paths, and `bash` only for commands these tools cannot run.\n\
             Returns: the file text; with offset and limit, that 0-based line range, followed by `[lines A-B of N]` when the range is partial. An empty file returns `[empty file]`.\n\
             On failure: the error says whether the path escaped the workspace (and names the root), was not found, is a directory, is binary, or could not be read, plus what to try next.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root."},
                    "offset": {"type": "integer", "description": "0-based line offset."},
                    "limit": {"type": "integer", "description": "Maximum number of lines to return."}
                },
                "required": ["path"]
            }),
        )
    }

    fn summarize(&self, arguments: &Value) -> String {
        format!(
            "read {}",
            arguments.get("path").and_then(Value::as_str).unwrap_or("?")
        )
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn execute<'a>(&'a self, arguments: Value, context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            match read(arguments, context).await {
                Ok(text) => ToolOutcome::ok(text),
                Err(error) => error.into_outcome(),
            }
        })
    }
}

async fn read(arguments: Value, context: &ToolContext) -> Result<String, ToolError> {
    let arguments: ReadArguments = parse_arguments(arguments)?;
    let path = context.workspace.resolve_existing(&arguments.path)?;
    let name = context.workspace.relative(&path);

    if path.is_dir() {
        return Err(ToolError::Message(format!(
            "`{name}` is a directory; use `glob` to list the files in it, then read one"
        )));
    }

    let bytes = tokio::fs::read(&path).await.map_err(|error| {
        ToolError::Io(format!(
            "could not read `{name}`: {error}; check that it is a readable text file, \
             or use `glob` to find the right path"
        ))
    })?;

    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return Err(ToolError::Message(format!(
            "`{name}` looks like a binary file; read a text file instead, or use `bash` \
             with an available inspection command if you need its type"
        )));
    }

    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();

    let start = arguments.offset.unwrap_or(0).min(total);
    let end = match arguments.limit {
        Some(limit) => (start + limit).min(total),
        None => total,
    };
    let mut selected = lines[start..end].join("\n");
    selected = truncate_output(&selected, context.limits.max_output_bytes);

    if start > 0 || end < total {
        selected.push_str(&format!("\n\n[lines {}-{} of {}]", start + 1, end, total));
    }
    if selected.is_empty() {
        return Ok("[empty file]".to_owned());
    }
    Ok(selected)
}
