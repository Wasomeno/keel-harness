//! Built-in tools for a coding harness.

pub mod apply_patch;
pub mod bash;
pub mod edit;
pub mod glob;
#[cfg(unix)]
pub mod jobs;
mod line_numbers;
pub mod read;
pub mod search;
#[cfg(unix)]
pub mod terminal;
pub mod write;

use crate::tool::ToolRegistry;

/// The default toolset: read, write, edit, apply_patch, bash, glob, search.
pub fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(read::ReadTool);
    registry.register(write::WriteTool);
    registry.register(edit::EditTool);
    registry.register(apply_patch::ApplyPatchTool);
    registry.register(bash::BashTool);
    registry.register(glob::GlobTool);
    registry.register(search::SearchTool);
    registry
}
