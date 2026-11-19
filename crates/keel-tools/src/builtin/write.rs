use std::io::Write;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{
    Tool, ToolContext, ToolError, ToolFuture, ToolOutcome, ToolSpec, parse_arguments, tool_spec,
};

pub struct WriteTool;

#[derive(Deserialize)]
struct WriteArguments {
    path: String,
    content: String,
}

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            "write",
            "Use when: creating a file or replacing its whole contents.\n\
             Prefer: `edit` for a targeted change to an existing file; `write` rewrites the file, so anything omitted from content is lost.\n\
             Returns: the byte count written and the workspace-relative path; missing parent directories are created.\n\
             On failure: the error names the path or parent directory, gives the OS reason, and says whether to check permissions, disk space, or the parent path.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root."},
                    "content": {"type": "string", "description": "Full file content."}
                },
                "required": ["path", "content"]
            }),
        )
    }

    fn summarize(&self, arguments: &Value) -> String {
        let path = arguments.get("path").and_then(Value::as_str).unwrap_or("?");
        let bytes = arguments
            .get("content")
            .and_then(Value::as_str)
            .map(str::len)
            .unwrap_or(0);
        format!("write {path} ({bytes} bytes)")
    }

    fn execute<'a>(&'a self, arguments: Value, context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            match write(arguments, context).await {
                Ok(text) => ToolOutcome::ok(text),
                Err(error) => error.into_outcome(),
            }
        })
    }
}

async fn write(arguments: Value, context: &ToolContext) -> Result<String, ToolError> {
    let arguments: WriteArguments = parse_arguments(arguments)?;
    let workspace = context.workspace.clone();
    let requested = arguments.path.clone();
    let name = workspace.display_path(&requested);
    let content = arguments.content;
    let bytes = content.len();
    let write_name = name.clone();
    tokio::task::spawn_blocking(move || {
        let mut file = workspace.open_for_write(&requested)?;
        file.write_all(content.as_bytes()).map_err(|error| {
            ToolError::Io(format!(
                "could not write `{write_name}`: {error}; check permissions and disk space, \
                 or choose another path in the workspace"
            ))
        })
    })
    .await
    .map_err(|error| {
        ToolError::Io(format!(
            "could not write `{name}`: the blocking writer failed: {error}"
        ))
    })??;

    Ok(format!("wrote {bytes} bytes to {name}"))
}
