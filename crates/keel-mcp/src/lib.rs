//! MCP tools for keel.
//!
//! MCP servers are configured per workspace, spoken to over stdio or
//! Streamable HTTP, and exposed through the ordinary [`keel_tools::Tool`] seam.
//! The harness therefore applies the same permission gate, timeout,
//! cancellation and truncation to MCP calls as it does to built-ins.
//!
//! Starting a server is an explicit CLI opt-in. A stdio command runs with the
//! user's full privileges, outside the workspace sandbox; `--yes` and the tool
//! permission gate do not cover startup. HTTP servers receive only the static
//! headers in `.keel/mcp.toml`; there is no OAuth flow and no other credential
//! source.

pub mod client;
pub mod config;
mod http;
mod stdio;
pub mod tool;

pub use client::{CallResult, McpClient, McpError, RemoteTool};
pub use config::{
    CONFIG_PATH, ConfigError, MAX_STARTUP_TIMEOUT, MAX_STARTUP_TIMEOUT_MS, McpConfig, ServerConfig,
    ServerSetup, config_path, qualified_name,
};
pub use tool::McpTool;
