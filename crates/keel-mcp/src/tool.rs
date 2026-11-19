//! The `Tool` adapter for one remote MCP tool.

use std::sync::Arc;

use keel_llm_types::ToolSpec;
use keel_tools::{Tool, ToolContext, ToolFuture, ToolOutcome};
use serde_json::{Value, json};

use crate::client::{McpClient, RemoteTool};

const MAX_SCHEMA_BYTES: usize = 16 * 1024;
const MAX_DESCRIPTION_BYTES: usize = 2 * 1024;

#[derive(Debug)]
pub struct McpTool {
    name: String,
    server: String,
    remote: RemoteTool,
    client: Arc<McpClient>,
    parallel_safe: bool,
}

impl McpTool {
    pub(crate) fn new(
        name: String,
        server: String,
        mut remote: RemoteTool,
        client: Arc<McpClient>,
        parallel_safe: bool,
    ) -> Result<(Self, Option<String>), String> {
        if !is_object_schema(&remote.input_schema) {
            return Err(
                "inputSchema must be an object schema with `type: \"object\"` and a `properties` object"
                    .to_owned(),
            );
        }
        let schema_size = serde_json::to_vec(&remote.input_schema)
            .map_err(|error| format!("inputSchema could not be serialized: {error}"))?
            .len();
        if schema_size > MAX_SCHEMA_BYTES {
            return Err(format!(
                "inputSchema is {schema_size} bytes; the limit is {MAX_SCHEMA_BYTES}"
            ));
        }

        let mut warning = None;
        if remote.description.len() > MAX_DESCRIPTION_BYTES {
            remote.description = truncate_text(remote.description.trim(), MAX_DESCRIPTION_BYTES);
            warning = Some(format!(
                "MCP tool `{name}` description was truncated to {MAX_DESCRIPTION_BYTES} bytes"
            ));
        }

        Ok((
            Self {
                name,
                server,
                remote,
                client,
                parallel_safe,
            },
            warning,
        ))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn remote_name(&self) -> &str {
        &self.remote.name
    }

    pub fn is_parallel_safe(&self) -> bool {
        self.parallel_safe
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn spec(&self) -> ToolSpec {
        let description = if self.remote.description.trim().is_empty() {
            format!(
                "MCP tool `{}` from server `{}`.",
                self.remote.name, self.server
            )
        } else {
            format!(
                "MCP tool `{}` from server `{}`. {}",
                self.remote.name,
                self.server,
                self.remote.description.trim()
            )
        };
        let input_schema = if self.remote.input_schema.is_object() {
            self.remote.input_schema.clone()
        } else {
            json!({"type": "object", "properties": {}})
        };
        ToolSpec {
            name: self.name.clone(),
            description,
            input_schema,
        }
    }

    fn summarize(&self, arguments: &Value) -> String {
        format!(
            "{} (MCP `{}` on `{}`, {})",
            self.name,
            self.remote.name,
            self.server,
            compact(arguments)
        )
    }

    fn parallel_safe(&self) -> bool {
        self.parallel_safe
    }

    fn execute<'a>(&'a self, arguments: Value, context: &'a ToolContext) -> ToolFuture<'a> {
        let client = self.client.clone();
        let remote = self.remote.name.clone();
        let name = self.name.clone();
        let server = self.server.clone();
        let timeout = context.limits.timeout;
        Box::pin(async move {
            tokio::select! {
                biased;
                () = context.cancel.cancelled() => {
                    ToolOutcome::error("cancelled before the MCP server answered")
                }
                result = client.call_tool(&remote, arguments, timeout) => {
                    match result {
                        Ok(result) if result.is_error => ToolOutcome::error(result.text),
                        Ok(result) => ToolOutcome::ok(result.text),
                        Err(error) => ToolOutcome::error(format!(
                            "MCP tool `{name}` on server `{server}` failed: {error}"
                        )),
                    }
                }
            }
        })
    }
}

fn is_object_schema(schema: &Value) -> bool {
    schema.is_object()
        && schema.get("type").and_then(Value::as_str) == Some("object")
        && schema.get("properties").is_some_and(Value::is_object)
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn compact(arguments: &Value) -> String {
    let text = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_owned());
    if text.chars().count() <= 80 {
        return text;
    }
    let mut short: String = text.chars().take(80).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_schemas_need_a_type_and_properties() {
        assert!(is_object_schema(
            &json!({"type": "object", "properties": {}})
        ));
        assert!(!is_object_schema(&json!({"type": "object"})));
        assert!(!is_object_schema(
            &json!({"type": "string", "properties": {}})
        ));
        assert!(!is_object_schema(
            &json!({"type": "object", "properties": []})
        ));
    }

    #[test]
    fn descriptions_are_cut_on_a_character_boundary() {
        let text = "é".repeat(MAX_DESCRIPTION_BYTES);
        let short = truncate_text(&text, MAX_DESCRIPTION_BYTES);
        assert!(short.len() <= MAX_DESCRIPTION_BYTES + 4);
        assert!(short.ends_with('…'));
    }
}
