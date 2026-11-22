//! Tool execution for keel.
//!
//! A tool is a name, a JSON Schema, and an async function. Everything that
//! decides *whether* a tool may run (permissions) or *where* it may run (the
//! workspace sandbox) lives outside the tool.

pub mod builtin;
pub mod permissions;
mod sandbox;
pub mod tool;
pub mod truncate;
pub mod workspace;

pub use permissions::{PermissionDecision, PermissionGate, PermissionRequest};
pub use tool::{
    SessionState, Tool, ToolContext, ToolError, ToolFuture, ToolLimits, ToolOutcome, ToolRegistry,
    tool_spec,
};
pub use truncate::truncate_output;
pub use workspace::Workspace;
