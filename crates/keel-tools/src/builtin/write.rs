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
    let path = context.workspace.resolve_for_write(&arguments.path)?;
    let name = context.workspace.relative(&path);

    if let Some(parent) = path.parent() {
        let parent_name = context.workspace.relative(parent);
        tokio::fs::create_dir_all(parent).await.map_err(|error| {
            ToolError::Io(format!(
                "could not create directory `{parent_name}` for `{name}`: {error}; \
                     check permissions, or write to a path whose parent already exists"
            ))
        })?;
    }

    tokio::fs::write(&path, arguments.content.as_bytes())
        .await
        .map_err(|error| {
            ToolError::Io(format!(
                "could not write `{name}`: {error}; check permissions and disk space, \
                 or choose another path in the workspace"
            ))
        })?;

    Ok(format!("wrote {} bytes to {name}", arguments.content.len()))
}
