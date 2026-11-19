//! MCP tools for keel.
//!
//! MCP servers are configured per workspace, started over stdio, and exposed
//! through the ordinary [`keel_tools::Tool`] seam. The harness therefore
//! applies the same permission gate, timeout, cancellation and truncation to
//! MCP calls as it does to built-ins.
//!
//! Starting a server is an explicit CLI opt-in. A server command runs with the
//! user's full privileges, outside the workspace sandbox; `--yes` and the tool
//! permission gate do not cover startup.

pub mod client;
pub mod config;
pub mod tool;

pub use client::{CallResult, McpClient, McpError, RemoteTool};
pub use config::{
    CONFIG_PATH, ConfigError, MAX_STARTUP_TIMEOUT, MAX_STARTUP_TIMEOUT_MS, McpConfig, ServerConfig,
    ServerSetup, config_path, qualified_name,
};
pub use tool::McpTool;
