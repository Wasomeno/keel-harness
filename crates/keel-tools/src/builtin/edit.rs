use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{
    Tool, ToolContext, ToolError, ToolFuture, ToolOutcome, ToolSpec, parse_arguments, tool_spec,
};

pub struct EditTool;

#[derive(Deserialize)]
struct EditArguments {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            "edit",
            "Use when: replacing a known, exact section of an existing text file.\n\
             Prefer: `write` to create or rewrite a whole file; `read` first, because old_string must match the file exactly, including whitespace.\n\
             Returns: how many occurrences were replaced and the file path.\n\
             On failure: zero matches tells you to read the file and copy the exact text; multiple matches tells you to add context or set replace_all; path and permission errors name the file and the next step.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root."},
                    "old_string": {"type": "string", "description": "Exact text to replace."},
                    "new_string": {"type": "string", "description": "Replacement text."},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence."}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        )
    }

    fn summarize(&self, arguments: &Value) -> String {
        format!(
            "edit {}",
            arguments.get("path").and_then(Value::as_str).unwrap_or("?")
        )
    }

    fn execute<'a>(&'a self, arguments: Value, context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            match edit(arguments, context).await {
                Ok(text) => ToolOutcome::ok(text),
                Err(error) => error.into_outcome(),
            }
        })
    }
}

async fn edit(arguments: Value, context: &ToolContext) -> Result<String, ToolError> {
    let arguments: EditArguments = parse_arguments(arguments)?;
    if arguments.old_string.is_empty() {
        return Err(ToolError::InvalidArguments(format!(
            "`old_string` must not be empty; read `{}` and pass the exact text to replace",
            arguments.path
        )));
    }

    let path = context.workspace.resolve_existing(&arguments.path)?;
    let name = context.workspace.relative(&path);

    let content = tokio::fs::read_to_string(&path).await.map_err(|error| {
        ToolError::Io(format!(
            "could not read `{name}` to edit it: {error}; check that it exists and is \
             readable, or use `glob` to find the right file"
        ))
    })?;

    let occurrences = content.matches(&arguments.old_string).count();
    if occurrences == 0 {
        return Err(ToolError::Message(format!(
            "old_string was not found in {name}; read the file with `read` and copy the \
             exact text, including whitespace, into old_string"
        )));
    }
    if occurrences > 1 && !arguments.replace_all {
        return Err(ToolError::Message(format!(
            "old_string appears {occurrences} times in {name}; add surrounding context so \
             it matches once, or set `replace_all` to true to replace all {occurrences}"
        )));
    }

    let updated = if arguments.replace_all {
        content.replace(&arguments.old_string, &arguments.new_string)
    } else {
        content.replacen(&arguments.old_string, &arguments.new_string, 1)
    };

    tokio::fs::write(&path, updated.as_bytes())
        .await
        .map_err(|error| {
            ToolError::Io(format!(
                "could not write `{name}` after editing: {error}; the change may not be \
                 saved, so read the file and retry"
            ))
        })?;

    Ok(format!("replaced {occurrences} occurrence(s) in {name}"))
}
