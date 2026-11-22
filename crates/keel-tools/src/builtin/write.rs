use serde::Deserialize;
use serde_json::{Value, json};

use crate::builtin::line_numbers::strip_numbered_block;
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
             Returns: the byte count written and the workspace-relative path; missing parent directories are created, and the replacement is atomic, so hard links keep the old contents. A block copied from `read` whose lines all carry a uniform, consecutive line-number gutter is written without the gutter, and the result says so.\n\
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
    let mut content = arguments.content;
    // A wholesale rewrite often starts from a read result. Accept the same
    // conservative gutter shape `edit` accepts; anything else is literal.
    let gutter_stripped = match strip_numbered_block(&content) {
        Some(stripped) => {
            content = stripped;
            true
        }
        None => false,
    };
    let bytes = content.len();
    tokio::task::spawn_blocking(move || workspace.write_file(&requested, content.as_bytes()))
        .await
        .map_err(|error| {
            ToolError::Io(format!(
                "could not write `{name}`: the blocking writer failed: {error}"
            ))
        })??;

    let note = if gutter_stripped {
        " (removed the read line-number gutter)"
    } else {
        ""
    };
    Ok(format!("wrote {bytes} bytes to {name}{note}"))
}
