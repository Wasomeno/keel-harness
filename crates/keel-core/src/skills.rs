//! Agent Skills, in the [agentskills.io](https://agentskills.io/specification)
//! shape Claude Code and Codex both load.
//!
//! A skill is a directory with a `SKILL.md`: YAML frontmatter (`name`,
//! `description`) and a markdown body. Discovery reads the workspace and, when
//! a home directory is available, the user directories those harnesses use.
//! There is no marketplace and no plugin install step.
//!
//! Progressive disclosure matches those harnesses. The session prompt receives
//! only the name, the description, and the `SKILL.md` path. The body stays on
//! disk until the model calls the `skill` tool or the host loads the skill by
//! name. Claude Code injects the loaded body with the skill's base directory;
//! Codex lists the path so the model can open the file. Keel does both: the
//! listing carries the path, and loading returns the body plus that directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use keel_tools::{Tool, ToolContext, ToolFuture, ToolOutcome, ToolRegistry, tool_spec};
use serde_json::{Value, json};

/// How many skills the prompt lists. The rest stay loadable by name. Codex
/// shortens or drops listings once they would crowd the context window; a hard
/// cap is the small version of that.
const MAX_LISTED_SKILLS: usize = 256;

/// Claude Code truncates the combined description shown in the listing.
const MAX_DESCRIPTION_CHARS: usize = 1536;

/// A discovered skill. `body` is the markdown after the frontmatter, not the
/// whole file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    /// Directory that contains `SKILL.md`. Scripts and references live here.
    pub directory: PathBuf,
    pub path: PathBuf,
}

/// What [`SkillCatalog::load`] returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    /// Markdown body of `SKILL.md`, without the frontmatter.
    pub body: String,
    pub directory: PathBuf,
    pub path: PathBuf,
}

impl LoadedSkill {
    /// The text the model sees once the skill is invoked.
    ///
    /// Claude Code prefixes the body with the skill's base directory so later
    /// reads of `scripts/` and `references/` resolve from that folder.
    pub fn instructions(&self) -> String {
        format!(
            "Base directory for this skill: {}\n\n{}",
            self.directory.display(),
            self.body
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SkillError {
    #[error("unknown skill `{name}`; available skills: {available}")]
    Unknown { name: String, available: String },
}

/// Skills discovered for one session, in name order.
#[derive(Clone, Debug, Default)]
pub struct SkillCatalog {
    skills: Arc<[Skill]>,
}

impl SkillCatalog {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Workspace skills plus the user directories, when `HOME` (or
    /// `USERPROFILE` on Windows) is set.
    ///
    /// Later roots replace an earlier skill of the same name. User directories
    /// are scanned first, then the workspace, and within each scope
    /// `.agents/skills` then `.claude/skills`, so a project skill wins over a
    /// user skill and a Claude Code directory wins over a Codex directory when
    /// both define the same name.
    pub fn discover(workspace: &Path) -> Self {
        Self::discover_with_home(workspace, home_dir().as_deref())
    }

    pub fn discover_with_home(workspace: &Path, home: Option<&Path>) -> Self {
        let mut found = BTreeMap::new();
        for root in skill_roots(workspace, home) {
            scan_root(&root, &mut found);
        }
        let skills: Vec<Skill> = found.into_values().collect();
        Self {
            skills: skills.into(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect()
    }

    /// Loads one skill by name. This is the explicit invocation path: a host
    /// that already knows the name, or the `skill` tool after the model asks.
    pub fn load(&self, name: &str) -> Result<LoadedSkill, SkillError> {
        let Some(skill) = self.get(name) else {
            return Err(SkillError::Unknown {
                name: name.to_owned(),
                available: self.available_list(),
            });
        };
        Ok(LoadedSkill {
            name: skill.name.clone(),
            description: skill.description.clone(),
            body: skill.body.clone(),
            directory: skill.directory.clone(),
            path: skill.path.clone(),
        })
    }

    /// Prompt text listing names, descriptions, and paths. The body is not
    /// included. `None` when nothing was discovered.
    pub fn listing(&self) -> Option<String> {
        if self.skills.is_empty() {
            return None;
        }
        let mut out = String::from(
            "Skills are available. Each entry is only the name, when to use it, \
             and the SKILL.md path. The instructions are not loaded yet. When a \
             task matches a description, or the user invokes a skill by name, \
             call the `skill` tool with that name and follow the body it returns. \
             Do not guess the instructions from the description.\n",
        );
        let listed = self.skills.len().min(MAX_LISTED_SKILLS);
        for skill in self.skills.iter().take(listed) {
            out.push_str("\n- name: ");
            out.push_str(&skill.name);
            out.push_str("\n  description: ");
            out.push_str(&one_line(&skill.description));
            out.push_str("\n  path: ");
            out.push_str(&skill.path.display().to_string());
            out.push('\n');
        }
        if self.skills.len() > listed {
            out.push_str(&format!(
                "\n{} more skills are installed and can be loaded by name.\n",
                self.skills.len() - listed
            ));
        }
        Some(out)
    }

    /// Registers the `skill` tool when the catalog is not empty. An empty
    /// catalog leaves the registry unchanged, so a workspace with no skills
    /// advertises the same tools as before.
    pub fn register(&self, registry: &mut ToolRegistry) {
        if !self.is_empty() {
            registry.register(SkillTool::new(self.clone()));
        }
    }

    fn available_list(&self) -> String {
        if self.skills.is_empty() {
            "(none)".to_owned()
        } else {
            self.names().join(", ")
        }
    }
}

/// The tool the model calls to load a skill body.
///
/// Claude Code's Skill tool takes a `skill` argument and returns the
/// `SKILL.md` instructions. The tool name here is `skill`, matching keel's
/// other tool names.
#[derive(Clone, Debug)]
pub struct SkillTool {
    catalog: SkillCatalog,
}

impl SkillTool {
    pub fn new(catalog: SkillCatalog) -> Self {
        Self { catalog }
    }
}

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn spec(&self) -> keel_llm_types::ToolSpec {
        tool_spec(
            "skill",
            "Use when: a skill listed in the system prompt matches the task, or the user invokes a skill by name. The prompt lists only the name, the description, and the SKILL.md path.\n\
             Prefer: calling this tool before acting on a matching skill. Do not guess the instructions from the description. Read files under the returned base directory only when the loaded instructions say to.\n\
             Returns: the SKILL.md body and the skill's base directory.\n\
             On failure: an unknown name lists the skills that were discovered.",
            json!({
                "type": "object",
                "properties": {
                    "skill": {
                        "type": "string",
                        "description": "Skill name, exactly as listed in the system prompt."
                    }
                },
                "required": ["skill"]
            }),
        )
    }

    fn summarize(&self, arguments: &Value) -> String {
        let name = arguments
            .get("skill")
            .and_then(Value::as_str)
            .unwrap_or("?");
        format!("skill: {name}")
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn execute<'a>(&'a self, arguments: Value, _context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let Some(name) = arguments
                .get("skill")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
            else {
                return ToolOutcome::error(
                    "invalid arguments: `skill` is required and must be the skill name from the system prompt",
                );
            };
            match self.catalog.load(name) {
                Ok(loaded) => ToolOutcome::ok(loaded.instructions()),
                Err(error) => ToolOutcome::error(error.to_string()),
            }
        })
    }
}

fn skill_roots(workspace: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = home {
        roots.push(home.join(".agents").join("skills"));
        roots.push(home.join(".claude").join("skills"));
    }
    roots.push(workspace.join(".agents").join("skills"));
    roots.push(workspace.join(".claude").join("skills"));
    roots
}

fn home_dir() -> Option<PathBuf> {
    for name in home_vars() {
        if let Some(value) = std::env::var_os(name).filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(value));
        }
    }
    None
}

fn home_vars() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["USERPROFILE", "HOME"]
    }
    #[cfg(not(windows))]
    {
        &["HOME"]
    }
}

fn scan_root(root: &Path, into: &mut BTreeMap<String, Skill>) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut directories = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            directories.push(path);
        }
    }
    directories.sort();
    for directory in directories {
        let skill_md = directory.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&skill_md) else {
            continue;
        };
        let dir_name = directory
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(skill) = parse_skill(&text, &dir_name, &directory, &skill_md) {
            into.insert(skill.name.clone(), skill);
        }
    }
}

fn parse_skill(text: &str, dir_name: &str, directory: &Path, path: &Path) -> Option<Skill> {
    let (front, body) = split_frontmatter(text);
    let fields = front.map(parse_fields).unwrap_or_default();
    let name = fields
        .get("name")
        .map(|value| value.trim())
        .filter(|value| valid_name(value))
        .unwrap_or(dir_name);
    if !valid_name(name) {
        return None;
    }
    let mut description = fields.get("description").cloned().unwrap_or_default();
    if let Some(when) = fields.get("when_to_use").filter(|value| !value.is_empty()) {
        if !description.is_empty() {
            description.push(' ');
        }
        description.push_str(when);
    }
    let body = body.trim().to_owned();
    if description.trim().is_empty() {
        description = first_content_line(&body)?;
    }
    let description = truncate_chars(description.trim(), MAX_DESCRIPTION_CHARS);
    if description.is_empty() {
        return None;
    }
    Some(Skill {
        name: name.to_owned(),
        description,
        body,
        directory: directory.to_path_buf(),
        path: path.to_path_buf(),
    })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.contains('\n')
        && !name.contains('\r')
}

/// Splits a leading `---` fence. Without one, the whole file is the body, which
/// is how Claude Code treats a `SKILL.md` whose first line is not a fence.
fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let start = if text.starts_with("---\r\n") {
        5
    } else if text.starts_with("---\n") {
        4
    } else {
        return (None, text);
    };
    let mut offset = start;
    for line in text[start..].split_inclusive('\n') {
        let bare = line.trim_end_matches(['\r', '\n']);
        if bare == "---" {
            let body_at = offset + line.len();
            return (Some(&text[start..offset]), &text[body_at..]);
        }
        offset += line.len();
    }
    (None, text)
}

fn parse_fields(front: &str) -> BTreeMap<String, String> {
    let lines: Vec<&str> = front.split('\n').collect();
    let mut fields = BTreeMap::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim_end_matches('\r');
        index += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, raw_value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.contains(' ') {
            continue;
        }
        let raw_value = raw_value.trim();
        if raw_value == "|" || raw_value == "|-" || raw_value == ">" || raw_value == ">-" {
            let fold = raw_value.starts_with('>');
            let mut block = Vec::new();
            while index < lines.len() {
                let next = lines[index].trim_end_matches('\r');
                if next.is_empty() {
                    block.push(String::new());
                    index += 1;
                    continue;
                }
                if next.starts_with(' ') || next.starts_with('\t') {
                    block.push(next.trim().to_owned());
                    index += 1;
                } else {
                    break;
                }
            }
            let value = if fold {
                fold_block(&block)
            } else {
                block.join("\n")
            };
            fields.insert(key.to_owned(), value);
        } else if raw_value.is_empty() {
            while index < lines.len() {
                let next = lines[index].trim_end_matches('\r');
                if next.is_empty() || next.starts_with(' ') || next.starts_with('\t') {
                    index += 1;
                } else {
                    break;
                }
            }
        } else {
            fields.insert(key.to_owned(), unquote(raw_value));
        }
    }
    fields
}

fn fold_block(lines: &[String]) -> String {
    let mut out = String::new();
    for line in lines {
        if line.is_empty() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            continue;
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push(' ');
        }
        out.push_str(line);
    }
    out
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        let inner = &value[1..value.len() - 1];
        if bytes[0] == b'"' {
            return inner
                .replace("\\n", "\n")
                .replace("\\\"", "\"")
                .replace("\\\\", "\\");
        }
        return inner.to_owned();
    }
    value.to_owned()
}

fn first_content_line(body: &str) -> Option<String> {
    body.lines().map(str::trim).find_map(|line| {
        let line = line.trim_start_matches('#').trim();
        if line.is_empty() {
            None
        } else {
            Some(line.to_owned())
        }
    })
}

fn one_line(text: &str) -> String {
    truncate_chars(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        MAX_DESCRIPTION_CHARS,
    )
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, relative: &str, text: &str) {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_skill_file_yields_name_description_and_body() {
        let text = "\
---
name: review-diff
description: Review a diff for risky changes. Use when the user asks for a code review.
---

Always mention the risk ledger.
";
        let skill = parse_skill(
            text,
            "review-diff",
            Path::new("/tmp/review-diff"),
            Path::new("/tmp/review-diff/SKILL.md"),
        )
        .unwrap();
        assert_eq!(skill.name, "review-diff");
        assert!(skill.description.contains("risky changes"));
        assert_eq!(skill.body, "Always mention the risk ledger.");
        assert!(!skill.description.contains("risk ledger"));
    }

    #[test]
    fn a_missing_name_uses_the_directory_and_a_missing_description_uses_the_body() {
        let text = "\
---
---
# Commit helper
Write a one-line commit message.
";
        let skill = parse_skill(
            text,
            "commit-helper",
            Path::new("/tmp/commit-helper"),
            Path::new("/tmp/commit-helper/SKILL.md"),
        )
        .unwrap();
        assert_eq!(skill.name, "commit-helper");
        assert_eq!(skill.description, "Commit helper");
        assert!(skill.body.contains("one-line commit message"));
    }

    #[test]
    fn a_folded_description_and_when_to_use_join_the_listing_text() {
        let text = "\
---
name: pdf
description: >
  Extract text from PDFs.
  Use when the user mentions a PDF.
when_to_use: also when they say document extraction
---
Run the extractor.
";
        let skill = parse_skill(
            text,
            "pdf",
            Path::new("/tmp/pdf"),
            Path::new("/tmp/pdf/SKILL.md"),
        )
        .unwrap();
        assert!(skill.description.starts_with("Extract text from PDFs."));
        assert!(skill.description.contains("document extraction"));
        assert_eq!(skill.body, "Run the extractor.");
    }

    #[test]
    fn discovery_reads_workspace_and_user_dirs_and_the_project_wins() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_skill(
            home.path(),
            ".claude/skills/review-diff/SKILL.md",
            "---\nname: review-diff\ndescription: from the user directory\n---\nuser body\n",
        );
        write_skill(
            home.path(),
            ".agents/skills/codex-only/SKILL.md",
            "---\nname: codex-only\ndescription: from the Codex user dir\n---\ncodex body\n",
        );
        write_skill(
            workspace.path(),
            ".agents/skills/review-diff/SKILL.md",
            "---\nname: review-diff\ndescription: from the Codex project dir\n---\nagents body\n",
        );
        write_skill(
            workspace.path(),
            ".claude/skills/review-diff/SKILL.md",
            "---\nname: review-diff\ndescription: from the project\n---\nUNIQUE_SKILL_BODY\n",
        );
        write_skill(
            workspace.path(),
            ".claude/skills/notes/SKILL.md",
            "---\nname: notes\ndescription: Take session notes.\n---\nnote the date\n",
        );

        let catalog = SkillCatalog::discover_with_home(workspace.path(), Some(home.path()));
        assert_eq!(catalog.names(), vec!["codex-only", "notes", "review-diff"]);
        let loaded = catalog.load("review-diff").unwrap();
        assert_eq!(loaded.description, "from the project");
        assert_eq!(loaded.body, "UNIQUE_SKILL_BODY");
        assert!(loaded.instructions().contains("UNIQUE_SKILL_BODY"));
        assert!(
            loaded
                .instructions()
                .contains("Base directory for this skill:")
        );
        assert_eq!(
            catalog.load("codex-only").unwrap().description,
            "from the Codex user dir"
        );

        let listing = catalog.listing().unwrap();
        assert!(listing.contains("from the project"));
        assert!(listing.contains("Take session notes."));
        assert!(!listing.contains("UNIQUE_SKILL_BODY"));
        assert!(!listing.contains("user body"));
        assert!(catalog.load("missing").is_err());
    }
}
