//! Built-in tools for a coding harness.

pub mod bash;
pub mod edit;
pub mod glob;
pub mod read;
pub mod search;
pub mod write;

use crate::tool::ToolRegistry;

/// The default toolset: read, write, edit, bash, glob, search.
pub fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(read::ReadTool);
    registry.register(write::WriteTool);
    registry.register(edit::EditTool);
    registry.register(bash::BashTool);
    registry.register(glob::GlobTool);
    registry.register(search::SearchTool);
    registry
}
