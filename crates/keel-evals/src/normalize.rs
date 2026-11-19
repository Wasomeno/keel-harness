//! Replayable tool output for the eval harness.
//!
//! A cassette records the model's whole conversation, including the tool
//! results that are sent back in the next request. Those results must stay
//! byte-identical when the run is replayed from a fresh temp directory on
//! another machine. Three eval-only adjustments make that true without
//! changing the product tools:
//!
//! - Paths: each task runs in a fresh temp directory, and tool results can
//!   mention that absolute path (a Python traceback, a shell diagnostic). The
//!   wrapper replaces the workspace root with the literal `<workspace>`.
//! - Shell listings: `find`, `ls`, `stat`, `du`, and `tree` print directory
//!   order, timestamps, owners, and sizes that differ between the recording
//!   and a replay (and between macOS and Linux). The eval shell rejects them
//!   and points at the `glob` tool, which returns a sorted path list.
//! - Search order: ripgrep walks the workspace in parallel and does not
//!   promise an order. The eval wrapper sorts `search` results so the same
//!   matches always produce the same text.
//!
//! Recording and replay use the same wrapper, so both sides agree and the
//! model sees a stable environment. None of this touches `keel-tools`.

use std::path::Path;
use std::sync::Arc;

use keel_llm_types::ToolSpec;
use keel_tools::{Tool, ToolContext, ToolFuture, ToolOutcome, ToolRegistry, Workspace, builtin};
use serde_json::Value;

struct NormalizedTool {
    inner: Arc<dyn Tool>,
    roots: Vec<String>,
}

impl Tool for NormalizedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    fn summarize(&self, arguments: &Value) -> String {
        self.inner.summarize(arguments)
    }

    fn parallel_safe(&self) -> bool {
        self.inner.parallel_safe()
    }

    fn execute<'a>(&'a self, arguments: Value, context: &'a ToolContext) -> ToolFuture<'a> {
        if let Some(error) = shell_guard(self.inner.name(), &arguments) {
            return Box::pin(async move { ToolOutcome::error(error) });
        }

        let inner = Arc::clone(&self.inner);
        let name = self.inner.name().to_owned();
        let roots = self.roots.clone();
        Box::pin(async move {
            let mut outcome: ToolOutcome = inner.execute(arguments, context).await;
            if name == "search" && !outcome.is_error {
                outcome.text = sorted_lines(&outcome.text);
            }
            for root in roots {
                if !root.is_empty() {
                    outcome.text = outcome.text.replace(&root, "<workspace>");
                }
            }
            outcome
        })
    }
}

/// The eval shell is for the task check and deterministic probes only.
/// Commands that print filesystem metadata are rejected before they run.
fn shell_guard(tool: &str, arguments: &Value) -> Option<String> {
    if tool != "bash" {
        return None;
    }
    let command = arguments.get("command")?.as_str()?;
    let blocked = command
        .split(|character: char| {
            character.is_whitespace() || matches!(character, ';' | '|' | '&' | '(' | ')' | '`')
        })
        .map(|token| token.rsplit('/').next().unwrap_or(token))
        .any(|token| matches!(token, "find" | "ls" | "stat" | "du" | "tree"));
    blocked.then(|| {
        "eval shell: `find`, `ls`, `stat`, `du`, and `tree` are disabled because their \
         output differs between machines; use the glob tool to list files"
            .to_owned()
    })
}

/// Sorts the match lines of one `search` result. A truncated result or a
/// single-line message is left alone.
fn sorted_lines(text: &str) -> String {
    if !text.contains('\n') || text.contains("[... ") {
        return text.to_owned();
    }
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort_unstable();
    lines.join("\n")
}

/// The full built-in tool registry with path-normalized results.
pub fn normalized_registry(workspace: &Workspace, raw_root: &Path) -> ToolRegistry {
    let base = builtin::registry();
    let mut roots = vec![workspace.root().display().to_string()];
    let raw = raw_root.display().to_string();
    if raw != roots[0] {
        roots.push(raw);
    }
    // Longest first, so the canonical spelling wins when one is a prefix of
    // the other.
    roots.sort_by_key(|root| std::cmp::Reverse(root.len()));

    let mut registry = ToolRegistry::new();
    for name in base.names() {
        let inner = base.get(name).expect("base registry lists its own tool");
        registry.register(NormalizedTool {
            inner,
            roots: roots.clone(),
        });
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_shell_guard_blocks_filesystem_listing_commands() {
        for command in [
            "ls -la",
            "find . -type f",
            "/bin/ls",
            "cat README.md; ls",
            "echo hi && find . -name '*.py'",
            "du -sh .",
            "tree",
            "stat README.md",
        ] {
            assert!(
                shell_guard("bash", &json!({ "command": command })).is_some(),
                "{command} should be rejected"
            );
        }
    }

    #[test]
    fn the_shell_guard_allows_the_check_and_deterministic_probes() {
        for command in [
            "python3 tests/check.py",
            "python3 tests/check.py; echo \"exit=$?\"",
            "cat keeltoy/text.py",
            "python3 -c \"print(1 + 1)\"",
            "./scripts/run.sh",
        ] {
            assert!(
                shell_guard("bash", &json!({ "command": command })).is_none(),
                "{command} should be allowed"
            );
        }
        assert!(shell_guard("read", &json!({ "path": "ls" })).is_none());
    }

    #[test]
    fn search_lines_are_sorted_and_single_lines_are_left_alone() {
        assert_eq!(
            sorted_lines("b.py:1:b\na.py:2:a\na.py:1:a"),
            "a.py:1:a\na.py:2:a\nb.py:1:b"
        );
        assert_eq!(sorted_lines("no matches for `x`"), "no matches for `x`");
        assert_eq!(
            sorted_lines("z\n[... 10 bytes truncated ...]\na"),
            "z\n[... 10 bytes truncated ...]\na"
        );
    }

    #[test]
    fn every_recorded_shell_command_passes_the_guard() {
        let dir = crate::fixture::cassettes_dir().join("evals");
        for entry in std::fs::read_dir(&dir).expect("the eval cassette directory exists") {
            let path = entry.expect("readable cassette dir entry").path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let cassette = keel_testkit::Cassette::load(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            for interaction in &cassette.interactions {
                let body: Value = serde_json::from_str(&interaction.request.body)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
                for message in body["messages"].as_array().into_iter().flatten() {
                    for call in message["tool_calls"].as_array().into_iter().flatten() {
                        if call["function"]["name"] != "bash" {
                            continue;
                        }
                        let arguments = call["function"]["arguments"]
                            .as_str()
                            .and_then(|raw| serde_json::from_str(raw).ok())
                            .unwrap_or(Value::Null);
                        assert!(
                            shell_guard("bash", &arguments).is_none(),
                            "{}: a recorded shell command is not deterministic: {arguments}",
                            path.display()
                        );
                    }
                }
            }
        }
    }
}
