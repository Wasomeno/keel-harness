//! The `Tool` adapter for one remote MCP tool.

use std::sync::Arc;

use keel_llm_types::ToolSpec;
use keel_tools::{Tool, ToolContext, ToolFuture, ToolOutcome};
use serde_json::{Value, json};

use crate::client::{McpClient, RemoteTool};

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
        remote: RemoteTool,
        client: Arc<McpClient>,
        parallel_safe: bool,
    ) -> Self {
        Self {
            name,
            server,
            remote,
            client,
            parallel_safe,
        }
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
            json!({"type": "object"})
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
            match client.call_tool(&remote, arguments, timeout).await {
                Ok(result) if result.is_error => ToolOutcome::error(result.text),
                Ok(result) => ToolOutcome::ok(result.text),
                Err(error) => ToolOutcome::error(format!(
                    "MCP tool `{name}` on server `{server}` failed: {error}"
                )),
            }
        })
    }
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
