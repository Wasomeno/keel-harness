use std::path::Path;

use globset::GlobBuilder;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{
    Tool, ToolContext, ToolError, ToolFuture, ToolOutcome, ToolSpec, parse_arguments, tool_spec,
};
use crate::truncate::truncate_output;

pub struct GlobTool;

#[derive(Deserialize)]
struct GlobArguments {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
}

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            "glob",
            "Use when: finding files by name or extension, or listing files under a directory deterministically.\n\
             Prefer: `search` to find file contents; `bash` for metadata such as sizes or permissions, which glob does not return.\n\
             Returns: workspace-relative paths, one per line and sorted; .gitignore'd and hidden files are skipped; if nothing matches, says so and suggests checking the pattern.\n\
             On failure: an invalid pattern names the pattern and glob syntax; a bad path names the workspace root or says the path was not found.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob such as `**/*.rs` or `*.toml`."},
                    "path": {"type": "string", "description": "Directory to search, relative to the workspace root."},
                    "max_results": {"type": "integer", "description": "Maximum number of paths to return."}
                },
                "required": ["pattern"]
            }),
        )
    }

    fn summarize(&self, arguments: &Value) -> String {
        format!(
            "glob {}",
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
            match glob(arguments, context).await {
                Ok(text) => ToolOutcome::ok(text),
                Err(error) => error.into_outcome(),
            }
        })
    }
}

async fn glob(arguments: Value, context: &ToolContext) -> Result<String, ToolError> {
    let arguments: GlobArguments = parse_arguments(arguments)?;
    if arguments.pattern.trim().is_empty() {
        return Err(ToolError::InvalidArguments(
            "`pattern` must not be empty; pass a glob such as `**/*.rs`".to_owned(),
        ));
    }

    let base = context.workspace.resolve_dir(arguments.path.as_deref())?;
    let base_name = {
        let relative = context.workspace.relative(&base);
        if relative.is_empty() {
            ".".to_owned()
        } else {
            relative
        }
    };
    let root = context.workspace.root().to_path_buf();
    let pattern = arguments.pattern.clone();
    let max = arguments.max_results.unwrap_or(500).clamp(1, 5000);
    let output_limit = context.limits.max_output_bytes;

    let results = tokio::task::spawn_blocking(move || walk(&base, &root, &pattern, max))
        .await
        .map_err(|error| {
            ToolError::Io(format!(
                "could not run the glob walker for `{}`: {error}; retry with a narrower `path`",
                arguments.pattern
            ))
        })??;

    if results.is_empty() {
        return Ok(format!(
            "no files matched `{}` under `{base_name}`; check the pattern (`**/*.rs` \
             matches recursively) and note that .gitignore'd or hidden files are skipped",
            arguments.pattern
        ));
    }
    Ok(truncate_output(&results.join("\n"), output_limit))
}

fn walk(base: &Path, root: &Path, pattern: &str, max: usize) -> Result<Vec<String>, ToolError> {
    let matcher = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|error| {
            ToolError::InvalidArguments(format!(
                "`{pattern}` is not a valid glob: {error}; use `**` to recurse and `/` to \
                 match the full relative path"
            ))
        })?
        .compile_matcher();

    let match_full_path = pattern.contains('/');
    let mut found = Vec::new();

    let walker = WalkBuilder::new(base).hidden(true).git_ignore(true).build();
    for entry in walker {
        let entry = entry.map_err(|error| {
            let displayed = crate::workspace::slash_path(base.strip_prefix(root).unwrap_or(base));
            ToolError::Io(format!(
                "could not list files under `{displayed}` with pattern `{pattern}`: {error}; \
                 narrow `path` to a directory inside the workspace"
            ))
        })?;
        if !entry
            .file_type()
            .map(|file_type| file_type.is_file())
            .unwrap_or(false)
        {
            continue;
        }

        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(path);
        let matched = if match_full_path {
            matcher.is_match(relative)
        } else {
            entry
                .file_name()
                .to_str()
                .map(|name| matcher.is_match(name))
                .unwrap_or(false)
        };

        if matched {
            found.push(crate::workspace::slash_path(relative));
            if found.len() >= max {
                break;
            }
        }
    }

    found.sort();
    Ok(found)
}
