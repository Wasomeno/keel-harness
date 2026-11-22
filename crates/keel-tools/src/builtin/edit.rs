use std::io::Read;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::builtin::line_numbers::strip_numbered_block;
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
             Prefer: `apply_patch` to change several places in one call; `write` to create or rewrite a whole file; `read` first, because old_string must match the file exactly, including whitespace. A block copied from `read` may keep its line-number gutter: edit strips a uniform, consecutive gutter when the literal text does not match.\n\
             Returns: how many occurrences were replaced and the file path; the replacement is atomic, so hard links keep the old contents.\n\
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

    let workspace = context.workspace.clone();
    let requested = arguments.path.clone();
    let name = workspace.display_path(&requested);
    let mut old_string = arguments.old_string;
    let mut new_string = arguments.new_string;
    let replace_all = arguments.replace_all;
    let edit_name = name.clone();
    let occurrences = tokio::task::spawn_blocking(move || {
        let mut file = workspace.open_for_edit(&requested)?;
        let metadata = file.metadata().map_err(|error| {
            ToolError::Io(format!(
                "could not inspect `{edit_name}`: {error}; check that it is a regular file"
            ))
        })?;
        if metadata.is_dir() {
            return Err(ToolError::Message(format!(
                "`{requested}` is a directory; choose a file to edit"
            )));
        }
        let mut content = String::new();
        file.read_to_string(&mut content).map_err(|error| {
            ToolError::Io(format!(
                "could not read `{edit_name}` to edit it: {error}; check that it exists and is \
                 readable, or use `glob` to find the right file"
            ))
        })?;
        drop(file);

        let mut occurrences = content.matches(&old_string).count();
        if occurrences == 0 {
            // A block copied out of `read` carries the display gutter. Try the
            // literal first, then remove one uniform, consecutive gutter. A
            // block that does not look like a read selection is left alone.
            if let Some(stripped) = strip_numbered_block(&old_string) {
                let stripped_occurrences = content.matches(&stripped).count();
                if stripped_occurrences > 0 {
                    old_string = stripped;
                    occurrences = stripped_occurrences;
                    if let Some(stripped_new) = strip_numbered_block(&new_string) {
                        new_string = stripped_new;
                    }
                }
            }
        }
        if occurrences == 0 {
            return Err(ToolError::Message(format!(
                "old_string was not found in {edit_name}; read the file with `read` and copy the \
                 exact text, including whitespace, into old_string"
            )));
        }
        if occurrences > 1 && !replace_all {
            return Err(ToolError::Message(format!(
                "old_string appears {occurrences} times in {edit_name}; add surrounding context so \
                 it matches once, or set `replace_all` to true to replace all {occurrences}"
            )));
        }

        let updated = if replace_all {
            content.replace(&old_string, &new_string)
        } else {
            content.replacen(&old_string, &new_string, 1)
        };

        workspace.edit_file(&requested, updated.as_bytes())?;
        Ok::<_, ToolError>(occurrences)
    })
    .await
    .map_err(|error| {
        ToolError::Io(format!(
            "could not edit `{name}`: the blocking editor failed: {error}"
        ))
    })??;

    Ok(format!("replaced {occurrences} occurrence(s) in {name}"))
}
