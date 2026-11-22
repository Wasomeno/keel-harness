//! The tool seam: a registry, a context and an outcome.
//!
//! `parallel_safe` declares that a tool may run beside another. The run loop
//! executes consecutive safe calls concurrently, up to a small bound, while an
//! unsafe call acts as a barrier.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use keel_llm_types::ToolSpec;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::workspace::Workspace;

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>>;

#[derive(Clone, Debug)]
pub struct ToolLimits {
    pub timeout: Duration,
    pub max_output_bytes: usize,
    /// Optional wall-clock deadline for the whole run. A background job caps
    /// its per-command timeout by the time remaining, so a job cannot outlive
    /// the task that started it. `None` means the host did not set a budget.
    pub deadline: Option<Instant>,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            max_output_bytes: 64 * 1024,
            deadline: None,
        }
    }
}

/// Durable state that belongs to one session rather than one tool instance.
///
/// The harness attaches a store to every [`ToolContext`] in a session. A tool
/// that keeps a value between calls asks the store instead of a field of its
/// own, so the value can survive a process restart. Implementations must be
/// safe to call from several tool calls at once.
pub trait SessionState: Send + Sync + std::fmt::Debug {
    /// The directory the one-shot `bash` tool should start in, if the session
    /// recorded one.
    fn bash_cwd(&self) -> Option<PathBuf>;

    /// Records the directory a one-shot `bash` call finished in. A store that
    /// cannot persist the value reports it to its host; the tool call itself
    /// is not failed for a state-write error.
    fn set_bash_cwd(&self, cwd: &Path);
}

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub workspace: Workspace,
    pub cancel: CancellationToken,
    pub limits: ToolLimits,
    /// Durable per-session state. `None` means a stateful tool keeps its value
    /// for the lifetime of the tool instance instead (the default registry and
    /// most embedders), which is enough within one run.
    pub session_state: Option<Arc<dyn SessionState>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error(
        "path `{path}` escapes the workspace root `{root}`; \
         use a workspace-relative path or an absolute path inside the root"
    )]
    OutsideWorkspace { path: String, root: String },
    #[error(
        "path not found: `{path}` (workspace root `{root}`); \
         use `glob` to list matching paths, or `write` to create the file"
    )]
    NotFound { path: String, root: String },
    #[error("{0}")]
    Io(String),
    #[error("timed out after {after:?}: {message}")]
    Timeout { after: Duration, message: String },
    #[error("cancelled before it finished; the run was cancelled")]
    Cancelled,
    #[error("{0}")]
    Message(String),
}

impl ToolError {
    pub fn into_outcome(self) -> ToolOutcome {
        ToolOutcome::error(self.to_string())
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    fn spec(&self) -> ToolSpec;

    /// A short human-readable description for a permission prompt.
    fn summarize(&self, arguments: &Value) -> String {
        format!("{}({})", self.name(), compact(arguments))
    }

    /// Whether the tool may run concurrently with another `parallel_safe` tool.
    fn parallel_safe(&self) -> bool {
        false
    }

    fn execute<'a>(&'a self, arguments: Value, context: &'a ToolContext) -> ToolFuture<'a>;
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: impl Tool + 'static) -> &mut Self {
        let name = tool.name().to_owned();
        debug_assert_eq!(tool.spec().name, name);
        self.tools.insert(name, Arc::new(tool));
        self
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|tool| tool.spec()).collect()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

pub fn tool_spec(
    name: impl Into<String>,
    description: impl Into<String>,
    input_schema: Value,
) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema,
    }
}

pub(crate) fn parse_arguments<T: serde::de::DeserializeOwned>(
    arguments: Value,
) -> Result<T, ToolError> {
    serde_json::from_value(arguments).map_err(|error| {
        ToolError::InvalidArguments(format!(
            "{error}; pass arguments as a JSON object that matches the tool's schema"
        ))
    })
}

fn compact(arguments: &Value) -> String {
    let text = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_owned());
    if text.chars().count() <= 120 {
        return text;
    }
    let mut short: String = text.chars().take(120).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_keeps_specs_in_name_order() {
        struct Echo;

        impl Tool for Echo {
            fn name(&self) -> &str {
                "echo"
            }

            fn spec(&self) -> ToolSpec {
                tool_spec("echo", "echo", serde_json::json!({"type": "object"}))
            }

            fn execute<'a>(
                &'a self,
                _arguments: Value,
                _context: &'a ToolContext,
            ) -> ToolFuture<'a> {
                Box::pin(async { ToolOutcome::ok("echo") })
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Echo);
        assert_eq!(registry.names(), vec!["echo"]);
        assert_eq!(registry.specs()[0].name, "echo");
    }
}
