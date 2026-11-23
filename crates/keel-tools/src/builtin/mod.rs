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
pub mod view_image;
pub mod write;

use crate::tool::ToolRegistry;

/// The default toolset: read, write, edit, apply_patch, bash, glob, search,
/// view_image.
pub fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(read::ReadTool);
    registry.register(write::WriteTool);
    registry.register(edit::EditTool);
    registry.register(apply_patch::ApplyPatchTool);
    registry.register(bash::BashTool::new());
    registry.register(glob::GlobTool);
    registry.register(search::SearchTool);
    registry.register(view_image::ViewImageTool);
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_specs_includes_only_read_only_builtins() {
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut reg = registry();
        #[cfg(unix)]
        reg.register(jobs::JobsTool::new());

        let read_only: Vec<String> = reg
            .read_only_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        assert_eq!(read_only, vec!["glob", "read", "search", "view_image"]);

        for mutating in ["apply_patch", "bash", "edit", "write"] {
            assert!(
                !reg.get(mutating).expect("registered").read_only(),
                "`{mutating}` must not be read-only"
            );
        }
        #[cfg(unix)]
        assert!(!reg.get("jobs").expect("registered").read_only());
    }

    #[test]
    fn plan_mode_specs_includes_read_only_builtins_and_platform_bash() {
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut reg = registry();
        #[cfg(unix)]
        reg.register(jobs::JobsTool::new());

        let plan_tools: Vec<String> = reg
            .plan_mode_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        let expected: Vec<&str> = if crate::plan_sandbox_supported() {
            vec!["bash", "glob", "read", "search", "view_image"]
        } else {
            vec!["glob", "read", "search", "view_image"]
        };
        assert_eq!(plan_tools, expected);

        assert_eq!(
            reg.get("bash").expect("registered").allowed_in_plan_mode(),
            crate::plan_sandbox_supported()
        );
        assert!(!reg.get("bash").expect("registered").read_only());

        for blocked in ["apply_patch", "edit", "write"] {
            assert!(
                !reg.get(blocked).expect("registered").allowed_in_plan_mode(),
                "`{blocked}` must not be allowed in plan mode"
            );
        }
        #[cfg(unix)]
        assert!(!reg.get("jobs").expect("registered").allowed_in_plan_mode());
    }
}
