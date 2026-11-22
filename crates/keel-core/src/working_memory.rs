//! Compaction working memory: the harness-recorded facts and the model's
//! structured narrative that survive a context cut.
//!
//! Prose alone is a lossy carrier. A summary can omit a file the model edited or
//! a command it ran even though the harness observed both directly. This module
//! splits the handoff in two:
//!
//! - **Deterministic facts** are scanned from the transcript at compaction time:
//!   successful `write`/`edit` paths, `bash` commands with their exit status and
//!   first output line, and the visible-test status the harness already tracks.
//!   The model never authors these, so a second compaction cannot invent or
//!   forget them.
//! - **Narrative fields** are requested from the summarizer as a small JSON
//!   block: the goal, plan steps with status, decisions with reasons, blockers
//!   and the next step. The prose handoff follows. A missing or malformed block
//!   falls back to the old whole-response summary, while the deterministic
//!   facts are still kept.
//!
//! A later compaction merges the previous memory with the new scan, so a fact
//! recorded before the first cut is still present after the second. The
//! rendered block has a hard token bound; when it is too large the oldest
//! commands are dropped first, then the oldest files. The goal, plan,
//! decisions, blockers and next step are never dropped, and the goal and
//! blockers are never removed by the trimmer.

use std::collections::HashSet;

use keel_llm_types::{Content, Message};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::test_loop::TestStatus;
use crate::tokens::estimate_text_tokens;

/// Header that identifies the rendered block in a summary message.
pub const WORKING_MEMORY_HEADER: &str = "[keel working memory]";

/// Footer that closes the rendered block.
pub const WORKING_MEMORY_FOOTER: &str = "[/keel working memory]";

/// Fixed maximum for remembered file paths, newest last.
pub const MAX_FILES: usize = 24;

/// Fixed maximum for remembered commands, newest last.
pub const MAX_COMMANDS: usize = 24;

/// Hard bound on the rendered block, using the same conservative estimate as
/// the rest of compaction. The narrative field caps keep the non-trimmable
/// part well below this, so the trimmer always has room to remove commands and
/// files.
pub const MAX_WORKING_MEMORY_TOKENS: u64 = 2_500;

const MAX_GOAL_CHARS: usize = 240;
const MAX_NEXT_STEP_CHARS: usize = 240;
const MAX_PLAN_STEPS: usize = 8;
const MAX_PLAN_STEP_CHARS: usize = 160;
const MAX_DECISIONS: usize = 6;
const MAX_DECISION_CHARS: usize = 120;
const MAX_REASON_CHARS: usize = 160;
const MAX_BLOCKERS: usize = 6;
const MAX_BLOCKER_CHARS: usize = 240;
const MAX_PATH_CHARS: usize = 160;
const MAX_COMMAND_CHARS: usize = 160;
const MAX_FIRST_LINE_CHARS: usize = 120;

/// One command the harness observed through `bash`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandFact {
    pub command: String,
    /// Parsed from the tool result's leading `exit: N` line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i32>,
    /// First non-empty output line after the synthetic exit line.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub first_line: String,
}

/// One plan step and its status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: String,
}

/// A decision and why it was taken.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub decision: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// The structured fields the summarizer is asked to provide.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NarrativeMemory {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub goal: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan: Vec<PlanStep>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<Decision>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub next_step: String,
}

impl NarrativeMemory {
    pub fn is_empty(&self) -> bool {
        self.goal.is_empty()
            && self.plan.is_empty()
            && self.decisions.is_empty()
            && self.blockers.is_empty()
            && self.next_step.is_empty()
    }

    /// A later response fills only the fields it provides; an empty field
    /// inherits the earlier value instead of erasing it.
    pub fn merged(self, newer: Self) -> Self {
        Self {
            goal: inherit(self.goal, newer.goal),
            plan: if newer.plan.is_empty() {
                self.plan
            } else {
                newer.plan
            },
            decisions: if newer.decisions.is_empty() {
                self.decisions
            } else {
                newer.decisions
            },
            blockers: if newer.blockers.is_empty() {
                self.blockers
            } else {
                newer.blockers
            },
            next_step: inherit(self.next_step, newer.next_step),
        }
        .capped()
    }

    fn capped(mut self) -> Self {
        self.goal = cap_chars(&self.goal, MAX_GOAL_CHARS);
        self.next_step = cap_chars(&self.next_step, MAX_NEXT_STEP_CHARS);
        self.plan.truncate(MAX_PLAN_STEPS);
        for step in &mut self.plan {
            step.step = cap_chars(&step.step, MAX_PLAN_STEP_CHARS);
            step.status = normalize_status(&step.status);
        }
        self.decisions.truncate(MAX_DECISIONS);
        for decision in &mut self.decisions {
            decision.decision = cap_chars(&decision.decision, MAX_DECISION_CHARS);
            decision.reason = cap_chars(&decision.reason, MAX_REASON_CHARS);
        }
        self.blockers.truncate(MAX_BLOCKERS);
        for blocker in &mut self.blockers {
            *blocker = cap_chars(blocker, MAX_BLOCKER_CHARS);
        }
        self
    }
}

fn inherit(previous: String, newer: String) -> String {
    if newer.trim().is_empty() {
        previous
    } else {
        newer
    }
}

fn normalize_status(status: &str) -> String {
    let normalized = status.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    match normalized.as_str() {
        "done" | "complete" | "completed" | "finished" => "done".to_owned(),
        "in_progress" | "doing" | "active" | "started" => "in_progress".to_owned(),
        "blocked" | "waiting" => "blocked".to_owned(),
        _ => "pending".to_owned(),
    }
}

/// Facts observed directly in the transcript, before merging with earlier
/// memory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObservedFacts {
    pub files: Vec<String>,
    pub commands: Vec<CommandFact>,
}

/// The complete working memory carried across compactions and resumes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingMemory {
    /// Successful `write`/`edit` paths, newest last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Observed `bash` commands, newest last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CommandFact>,
    #[serde(default)]
    pub narrative: NarrativeMemory,
    /// Latest harness-tracked visible-test status. It is deterministic state,
    /// not model narrative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_status: Option<TestStatus>,
    /// Successful compactions so far for this session, including resumed ones.
    #[serde(default)]
    pub compactions: u32,
}

/// Bounded counters attached to one compaction record and status event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStats {
    /// Bytes of the rendered block actually appended to the summary message.
    pub memory_bytes: usize,
    /// Estimated tokens for those bytes.
    pub memory_tokens: u64,
    /// File paths present in the rendered block after trimming.
    pub files_remembered: usize,
    /// Commands present in the rendered block after trimming.
    pub commands_remembered: usize,
    /// Successful compactions so far, including this one.
    pub compactions_so_far: u32,
}

impl WorkingMemory {
    /// Scans the transcript for observable facts.
    ///
    /// Every assistant turn and tool result is dropped by a compaction, so the
    /// scan covers the whole history being cut. A `write` or `edit` counts only
    /// when its matching result exists and is not an error. A `bash` command
    /// counts whenever its result exists, because a non-zero exit is useful
    /// state; the exit status comes from the tool's leading `exit: N` line.
    pub fn observe(history: &[Message]) -> ObservedFacts {
        let mut results: Vec<(&str, &str, bool)> = Vec::new();
        for message in history {
            for (call_id, output, is_error) in message.tool_results() {
                results.push((call_id.as_str(), output, is_error));
            }
        }

        let mut files = Vec::new();
        let mut commands = Vec::new();
        for message in history {
            for part in &message.parts {
                let Content::ToolCall {
                    id,
                    name,
                    arguments,
                } = &part.content
                else {
                    continue;
                };
                let Some((_, output, is_error)) = results
                    .iter()
                    .find(|(call_id, _, _)| *call_id == id.as_str())
                else {
                    continue;
                };
                match name.as_str() {
                    "write" | "edit" if !is_error => {
                        if let Some(path) = string_argument(arguments, "path") {
                            files.push(cap_chars(&normalize_path(&path), MAX_PATH_CHARS));
                        }
                    }
                    "bash" => {
                        if let Some(command) = string_argument(arguments, "command") {
                            commands.push(command_fact(&command, output));
                        }
                    }
                    _ => {}
                }
            }
        }

        ObservedFacts {
            files: dedupe_and_cap(files, MAX_FILES, |path| path.clone()),
            commands: dedupe_and_cap(commands, MAX_COMMANDS, |command| command.command.clone()),
        }
    }

    /// Adds newly observed facts to the earlier ones. Deduplication keeps the
    /// newest occurrence, so the list stays ordered with the newest last and
    /// never contains the same path or command twice.
    pub fn merge_observed(&mut self, observed: &ObservedFacts) {
        let mut files = std::mem::take(&mut self.files);
        files.extend(observed.files.iter().cloned());
        self.files = dedupe_and_cap(files, MAX_FILES, |path| path.clone());

        let mut commands = std::mem::take(&mut self.commands);
        commands.extend(observed.commands.iter().cloned());
        self.commands = dedupe_and_cap(commands, MAX_COMMANDS, |command| command.command.clone());
    }

    /// Records the latest test status. A session with no recognised test run
    /// keeps the status it already had.
    pub fn merge_test_status(&mut self, status: &TestStatus) {
        if status.tests_run() {
            self.test_status = Some(status.clone());
        }
    }

    /// True when there is anything worth injecting besides the counter line.
    pub fn has_content(&self) -> bool {
        !self.narrative.is_empty()
            || self
                .test_status
                .as_ref()
                .is_some_and(|status| status.tests_run())
            || !self.files.is_empty()
            || !self.commands.is_empty()
    }

    /// Renders the bounded block that is appended to the summary message.
    pub fn render(&self) -> String {
        self.render_bounded().text
    }

    /// Renders and reports the counters for the exact block that was injected.
    /// A memory with nothing beyond the counter line renders no block and
    /// reports zero bytes, so the counters describe what actually entered the
    /// request.
    pub fn render_with_stats(&self) -> (String, MemoryStats) {
        if !self.has_content() {
            return (
                String::new(),
                MemoryStats {
                    compactions_so_far: self.compactions,
                    ..MemoryStats::default()
                },
            );
        }
        let rendered = self.render_bounded();
        let stats = MemoryStats {
            memory_bytes: rendered.text.len(),
            memory_tokens: estimate_text_tokens(&rendered.text),
            files_remembered: rendered.files,
            commands_remembered: rendered.commands,
            compactions_so_far: self.compactions,
        };
        (rendered.text, stats)
    }

    fn render_bounded(&self) -> Rendered {
        self.render_with_limit(MAX_WORKING_MEMORY_TOKENS)
    }

    fn render_with_limit(&self, limit_tokens: u64) -> Rendered {
        let mut files = self.files.clone();
        let mut commands = self.commands.clone();
        loop {
            let text = self.render_sections(&files, &commands);
            let fits = estimate_text_tokens(&text) <= limit_tokens;
            if fits || (commands.is_empty() && files.is_empty()) {
                return Rendered {
                    text,
                    files: files.len(),
                    commands: commands.len(),
                };
            }
            // Documented trim order: oldest commands first, then oldest files.
            if !commands.is_empty() {
                commands.remove(0);
            } else {
                files.remove(0);
            }
        }
    }

    fn render_sections(&self, files: &[String], commands: &[CommandFact]) -> String {
        let mut lines = vec![
            WORKING_MEMORY_HEADER.to_owned(),
            format!("Compactions so far: {}", self.compactions),
        ];
        if !self.narrative.goal.is_empty() {
            lines.push(format!("Goal: {}", self.narrative.goal));
        }
        if !self.narrative.plan.is_empty() {
            lines.push("Plan:".to_owned());
            for step in &self.narrative.plan {
                lines.push(format!("- [{}] {}", step.status, step.step));
            }
        }
        if !self.narrative.decisions.is_empty() {
            lines.push("Decisions:".to_owned());
            for decision in &self.narrative.decisions {
                if decision.reason.is_empty() {
                    lines.push(format!("- {}", decision.decision));
                } else {
                    lines.push(format!("- {}: {}", decision.decision, decision.reason));
                }
            }
        }
        if !self.narrative.blockers.is_empty() {
            lines.push("Blockers:".to_owned());
            for blocker in &self.narrative.blockers {
                lines.push(format!("- {blocker}"));
            }
        }
        if !self.narrative.next_step.is_empty() {
            lines.push(format!("Next step: {}", self.narrative.next_step));
        }
        if let Some(status) = &self.test_status {
            if let Some(text) = status.working_memory() {
                lines.push(text);
            }
        }
        if !files.is_empty() {
            lines.push("Files changed (newest last):".to_owned());
            for path in files {
                lines.push(format!("- {path}"));
            }
        }
        if !commands.is_empty() {
            lines.push("Commands (newest last):".to_owned());
            for command in commands {
                let exit = command
                    .exit_status
                    .map_or_else(|| "?".to_owned(), |code| code.to_string());
                if command.first_line.is_empty() {
                    lines.push(format!("- `{}` exit {exit}", command.command));
                } else {
                    lines.push(format!(
                        "- `{}` exit {exit}: {}",
                        command.command, command.first_line
                    ));
                }
            }
        }
        lines.push(WORKING_MEMORY_FOOTER.to_owned());
        lines.join("\n")
    }
}

struct Rendered {
    text: String,
    files: usize,
    commands: usize,
}

/// The result of parsing a summary response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSummary {
    pub narrative: NarrativeMemory,
    pub prose: String,
    /// True when a structured block parsed. Malformed or missing blocks keep
    /// `prose` as the whole response and leave the narrative empty.
    pub structured: bool,
}

/// Parses the model's structured block, when present, and separates it from
/// the prose handoff that follows.
///
/// A `json` code fence anywhere in the response is accepted, and so is a JSON
/// object at the very start of the response followed by prose. Anything else,
/// including malformed JSON, is the old path: the whole response is the
/// summary.
pub fn parse_summary(raw: &str) -> ParsedSummary {
    if let Some((value, prose)) = structured_block(raw) {
        return ParsedSummary {
            narrative: narrative_from_value(&value),
            prose,
            structured: true,
        };
    }
    ParsedSummary {
        narrative: NarrativeMemory::default(),
        prose: raw.trim().to_owned(),
        structured: false,
    }
}

fn structured_block(raw: &str) -> Option<(Value, String)> {
    if let Some(start) = raw.find("```") {
        let mut body = &raw[start + 3..];
        if let Some(rest) = body
            .strip_prefix("json")
            .or_else(|| body.strip_prefix("JSON"))
        {
            body = rest;
        }
        if let Some(end) = body.find("```") {
            let json = body[..end].trim();
            let before = raw[..start].trim();
            let after = body[end + 3..].trim();
            let value = serde_json::from_str::<Value>(json).ok()?;
            if !is_memory_block(&value) {
                return None;
            }
            let prose = match (before.is_empty(), after.is_empty()) {
                (true, _) => after.to_owned(),
                (false, true) => before.to_owned(),
                (false, false) => format!("{before}\n{after}"),
            };
            return Some((value, prose));
        }
        return None;
    }

    let trimmed = raw.trim_start();
    if !trimmed.starts_with('{') {
        return None;
    }
    let mut stream = serde_json::Deserializer::from_str(trimmed).into_iter::<Value>();
    let value = stream.next()?.ok()?;
    if !is_memory_block(&value) {
        return None;
    }
    let offset = stream.byte_offset();
    Some((value, trimmed[offset..].trim().to_owned()))
}

fn is_memory_block(value: &Value) -> bool {
    value.is_object()
        && ["goal", "plan", "decisions", "blockers", "next_step"]
            .iter()
            .any(|key| value.get(*key).is_some())
}

fn narrative_from_value(value: &Value) -> NarrativeMemory {
    let mut narrative = NarrativeMemory {
        goal: string_field(value, "goal"),
        plan: plan_from_value(value.get("plan")),
        decisions: decisions_from_value(value.get("decisions")),
        blockers: blockers_from_value(value.get("blockers")),
        next_step: string_field(value, "next_step"),
    };
    narrative = narrative.capped();
    narrative
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned()
}

fn plan_from_value(value: Option<&Value>) -> Vec<PlanStep> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut plan = Vec::new();
    for item in items {
        let (step, status) = match item {
            Value::String(step) => (step.trim().to_owned(), "pending".to_owned()),
            Value::Object(_) => (
                string_field(item, "step"),
                item.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("pending")
                    .to_owned(),
            ),
            _ => continue,
        };
        if step.is_empty() {
            continue;
        }
        plan.push(PlanStep { step, status });
    }
    plan
}

fn decisions_from_value(value: Option<&Value>) -> Vec<Decision> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut decisions = Vec::new();
    for item in items {
        let (decision, reason) = match item {
            Value::String(decision) => (decision.trim().to_owned(), String::new()),
            Value::Object(_) => (string_field(item, "decision"), string_field(item, "reason")),
            _ => continue,
        };
        if decision.is_empty() {
            continue;
        }
        decisions.push(Decision { decision, reason });
    }
    decisions
}

fn blockers_from_value(value: Option<&Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    if let Some(blocker) = value.as_str() {
        let blocker = blocker.trim();
        return if blocker.is_empty() {
            Vec::new()
        } else {
            vec![blocker.to_owned()]
        };
    }
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|blocker| !blocker.is_empty())
        .map(str::to_owned)
        .collect()
}

fn string_argument(arguments: &str, key: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(arguments).ok()?;
    value
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn normalize_path(path: &str) -> String {
    let path = path.trim();
    path.strip_prefix("./").unwrap_or(path).to_owned()
}

fn command_fact(command: &str, output: &str) -> CommandFact {
    let mut lines = output.lines();
    let first = lines.next().unwrap_or_default().trim();
    let (exit_status, first_line) = if let Some(rest) = first.strip_prefix("exit:") {
        (
            rest.trim().parse::<i32>().ok(),
            lines
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default()
                .trim()
                .to_owned(),
        )
    } else {
        (
            None,
            if first.is_empty() {
                lines
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            } else {
                first.to_owned()
            },
        )
    };
    CommandFact {
        command: cap_chars(command.trim(), MAX_COMMAND_CHARS),
        exit_status,
        first_line: cap_chars(&first_line, MAX_FIRST_LINE_CHARS),
    }
}

fn dedupe_and_cap<T>(items: Vec<T>, max: usize, key: impl Fn(&T) -> String) -> Vec<T> {
    let mut seen = HashSet::new();
    let mut kept: Vec<T> = Vec::new();
    for item in items.into_iter().rev() {
        if seen.insert(key(&item)) {
            kept.push(item);
        }
    }
    kept.reverse();
    if kept.len() > max {
        kept.drain(..kept.len() - max);
    }
    kept
}

fn cap_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut capped: String = text.chars().take(max).collect();
    capped.push('…');
    capped
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_llm_types::{Part, ProtocolId, Source};

    fn source() -> Source {
        Source {
            protocol: ProtocolId::OpenAiChat,
            provider: "test".into(),
            model: "gpt-4.1".into(),
        }
    }

    fn call(id: &str, name: &str, arguments: Value) -> Message {
        Message::assistant(vec![Part::tool_call(
            id.into(),
            name,
            arguments.to_string(),
            source(),
        )])
    }

    fn result(id: &str, output: &str, is_error: bool) -> Message {
        Message::new(keel_llm_types::Role::User).with_part(Part::tool_result(
            id.into(),
            output,
            is_error,
        ))
    }

    #[test]
    fn successful_writes_and_edits_are_deduplicated_ordered_and_capped() {
        let history = vec![
            Message::user("start"),
            call("w1", "write", serde_json::json!({"path": "src/a.rs"})),
            result("w1", "wrote 10 bytes to src/a.rs", false),
            call("e1", "edit", serde_json::json!({"path": "./src/b.rs"})),
            result("e1", "replaced 1 occurrence(s) in src/b.rs", false),
            call("w2", "write", serde_json::json!({"path": "./src/a.rs"})),
            result("w2", "wrote 20 bytes to src/a.rs", false),
            call("w3", "write", serde_json::json!({"path": "src/failed.rs"})),
            result("w3", "could not write: denied", true),
            call(
                "r1",
                "write",
                serde_json::json!({"path": "src/no-result.rs"}),
            ),
        ];

        let observed = WorkingMemory::observe(&history);
        assert_eq!(
            observed.files,
            ["src/b.rs", "src/a.rs"],
            "the failed and result-less writes are dropped, the duplicate keeps \
             its newest position, and ./ is normalized"
        );

        let mut many = Vec::new();
        for index in 0..(MAX_FILES + 3) {
            let id = format!("many{index}");
            many.push(call(
                &id,
                "write",
                serde_json::json!({"path": format!("src/many{index}.rs")}),
            ));
            many.push(result(&id, "wrote bytes", false));
        }
        let observed = WorkingMemory::observe(&many);
        assert_eq!(
            observed.files.len(),
            MAX_FILES,
            "the fixed file cap is enforced after deduplication"
        );
        assert_eq!(
            observed.files.last().map(String::as_str),
            Some(format!("src/many{}.rs", MAX_FILES + 2).as_str()),
            "the newest paths are kept and the oldest are dropped"
        );
    }

    #[test]
    fn bash_commands_keep_their_exit_status_and_first_output_line() {
        let history = vec![
            call(
                "c1",
                "bash",
                serde_json::json!({"command": "cargo test parser"}),
            ),
            result("c1", "exit: 0\ntest result: ok. 3 passed\nmore", false),
            call(
                "c2",
                "bash",
                serde_json::json!({"command": "cargo test parser"}),
            ),
            result("c2", "exit: 101\nassertion failed\n", true),
            call(
                "c3",
                "bash",
                serde_json::json!({"command": "python -m pytest -q"}),
            ),
            result("c3", "exit: 1\n2 failed, 1 passed\n", true),
        ];

        let observed = WorkingMemory::observe(&history);
        assert_eq!(observed.commands.len(), 2);
        assert_eq!(observed.commands[0].command, "cargo test parser");
        assert_eq!(
            observed.commands[0].exit_status,
            Some(101),
            "a repeated command keeps its newest result"
        );
        assert_eq!(observed.commands[0].first_line, "assertion failed");
        assert_eq!(observed.commands[1].command, "python -m pytest -q");
        assert_eq!(observed.commands[1].exit_status, Some(1));
        assert_eq!(observed.commands[1].first_line, "2 failed, 1 passed");
    }

    #[test]
    fn a_second_merge_keeps_a_fact_recorded_before_the_first() {
        let first = ObservedFacts {
            files: vec!["src/old.rs".to_owned()],
            commands: vec![CommandFact {
                command: "cargo test old".to_owned(),
                exit_status: Some(0),
                first_line: "ok".to_owned(),
            }],
        };
        let mut memory = WorkingMemory {
            narrative: NarrativeMemory {
                goal: "fix the parser".to_owned(),
                next_step: "run the focused test".to_owned(),
                ..NarrativeMemory::default()
            },
            ..WorkingMemory::default()
        };
        memory.merge_observed(&first);
        memory.compactions = 1;

        let second = ObservedFacts {
            files: vec!["src/new.rs".to_owned()],
            commands: Vec::new(),
        };
        memory.merge_observed(&second);
        memory.compactions = 2;
        memory.narrative = memory.narrative.merged(NarrativeMemory {
            goal: String::new(),
            next_step: "run the full suite".to_owned(),
            ..NarrativeMemory::default()
        });

        assert_eq!(memory.files, ["src/old.rs", "src/new.rs"]);
        assert_eq!(memory.commands.len(), 1);
        assert_eq!(memory.commands[0].command, "cargo test old");
        assert_eq!(
            memory.narrative.goal, "fix the parser",
            "an empty field inherits the earlier value"
        );
        assert_eq!(memory.narrative.next_step, "run the full suite");
        assert_eq!(memory.compactions, 2);
    }

    #[test]
    fn a_malformed_or_missing_block_falls_back_to_the_whole_response() {
        let missing = parse_summary("Progress: fixed the parser.");
        assert!(!missing.structured);
        assert_eq!(missing.prose, "Progress: fixed the parser.");
        assert!(missing.narrative.is_empty());

        let malformed = parse_summary("```json\n{\"goal\":\n```\nAfter the block.");
        assert!(!malformed.structured);
        assert_eq!(
            malformed.prose,
            "```json\n{\"goal\":\n```\nAfter the block."
        );
        assert!(malformed.narrative.is_empty());

        let structured = parse_summary(
            "```json\n{\"goal\":\"ship it\",\"plan\":[{\"step\":\"write\",\"status\":\"done\"}],\"blockers\":[\"none\"]}\n```\n\nProgress: shipped.",
        );
        assert!(structured.structured);
        assert_eq!(structured.prose, "Progress: shipped.");
        assert_eq!(structured.narrative.goal, "ship it");
        assert_eq!(structured.narrative.plan[0].status, "done");
        assert_eq!(structured.narrative.blockers, ["none"]);

        let bare_fence = parse_summary("```\n{\"goal\":\"bare\"}\n```\n\nProgress.");
        assert!(bare_fence.structured);
        assert_eq!(bare_fence.narrative.goal, "bare");
        assert_eq!(bare_fence.prose, "Progress.");

        let unrelated_json = parse_summary("```json\n{\"example\":true}\n```\n\nProgress.");
        assert!(
            !unrelated_json.structured,
            "JSON without a working-memory key is prose, not a block"
        );
        assert_eq!(
            unrelated_json.prose,
            "```json\n{\"example\":true}\n```\n\nProgress."
        );
    }

    #[test]
    fn a_leading_json_object_splits_at_the_end_of_the_value() {
        let parsed = parse_summary(
            "{\"goal\":\"one line\",\"plan\":[],\"decisions\":[],\"blockers\":[],\"next_step\":\"go\"}\n\nThe prose.",
        );
        assert!(parsed.structured);
        assert_eq!(parsed.prose, "The prose.");
        assert_eq!(parsed.narrative.goal, "one line");
        assert_eq!(parsed.narrative.next_step, "go");
    }

    #[test]
    fn every_narrative_field_is_capped() {
        let raw = format!(
            "```json\n{}\n```\nsummary",
            serde_json::json!({
                "goal": "g".repeat(1_000),
                "plan": (0..20).map(|_| serde_json::json!({"step": "s".repeat(1_000), "status": "done"})).collect::<Vec<_>>(),
                "decisions": (0..20).map(|_| serde_json::json!({"decision": "d".repeat(1_000), "reason": "r".repeat(1_000)})).collect::<Vec<_>>(),
                "blockers": (0..20).map(|_| "b".repeat(1_000)).collect::<Vec<_>>(),
                "next_step": "n".repeat(1_000),
            })
        );
        let parsed = parse_summary(&raw);
        let narrative = parsed.narrative;
        assert_eq!(narrative.goal.chars().count(), MAX_GOAL_CHARS + 1);
        assert_eq!(narrative.plan.len(), MAX_PLAN_STEPS);
        assert_eq!(narrative.decisions.len(), MAX_DECISIONS);
        assert_eq!(narrative.blockers.len(), MAX_BLOCKERS);
        assert_eq!(narrative.next_step.chars().count(), MAX_NEXT_STEP_CHARS + 1);
    }

    #[test]
    fn the_rendered_block_is_stable() {
        let memory = WorkingMemory {
            files: vec![
                "src/parser.rs".to_owned(),
                "tests/test_parser.py".to_owned(),
            ],
            commands: vec![
                CommandFact {
                    command: "python -m pytest tests/test_parser.py".to_owned(),
                    exit_status: Some(1),
                    first_line: "1 failed, 2 passed".to_owned(),
                },
                CommandFact {
                    command: "git diff --check".to_owned(),
                    exit_status: Some(0),
                    first_line: String::new(),
                },
            ],
            narrative: NarrativeMemory {
                goal: "fix the failing parser test".to_owned(),
                plan: vec![
                    PlanStep {
                        step: "reproduce the failure".to_owned(),
                        status: "done".to_owned(),
                    },
                    PlanStep {
                        step: "update the tokenizer".to_owned(),
                        status: "in_progress".to_owned(),
                    },
                ],
                decisions: vec![Decision {
                    decision: "reuse the existing tokenizer".to_owned(),
                    reason: "avoids a new dependency".to_owned(),
                }],
                blockers: vec!["the fixture path is unknown".to_owned()],
                next_step: "run tests/test_parser.py".to_owned(),
            },
            test_status: Some(TestStatus {
                runs: 2,
                last_command: Some("python -m pytest tests/test_parser.py".to_owned()),
                last_outcome: Some(crate::test_loop::TestOutcome::Failed),
                failing_tests: vec!["tests/test_parser.py::test_value".to_owned()],
                excerpt: Some("assert 1 == 2".to_owned()),
            }),
            compactions: 2,
        };

        assert_eq!(
            memory.render(),
            "[keel working memory]\n\
             Compactions so far: 2\n\
             Goal: fix the failing parser test\n\
             Plan:\n\
             - [done] reproduce the failure\n\
             - [in_progress] update the tokenizer\n\
             Decisions:\n\
             - reuse the existing tokenizer: avoids a new dependency\n\
             Blockers:\n\
             - the fixture path is unknown\n\
             Next step: run tests/test_parser.py\n\
             Visible test status (tracked by the harness): 2 run(s); last failed\n\
             - last command: `python -m pytest tests/test_parser.py`\n\
             - failing tests: tests/test_parser.py::test_value\n\
             - last failure excerpt: assert 1 == 2\n\
             Files changed (newest last):\n\
             - src/parser.rs\n\
             - tests/test_parser.py\n\
             Commands (newest last):\n\
             - `python -m pytest tests/test_parser.py` exit 1: 1 failed, 2 passed\n\
             - `git diff --check` exit 0\n\
             [/keel working memory]"
        );
    }

    #[test]
    fn the_size_limit_drops_oldest_commands_then_oldest_files() {
        let memory = WorkingMemory {
            files: vec!["old.rs".to_owned(), "new.rs".to_owned()],
            commands: vec![
                CommandFact {
                    command: "old command".to_owned(),
                    exit_status: Some(0),
                    first_line: "old".to_owned(),
                },
                CommandFact {
                    command: "middle command".to_owned(),
                    exit_status: Some(0),
                    first_line: "middle".to_owned(),
                },
                CommandFact {
                    command: "new command".to_owned(),
                    exit_status: Some(0),
                    first_line: "new".to_owned(),
                },
            ],
            narrative: NarrativeMemory {
                goal: "keep the goal".to_owned(),
                blockers: vec!["keep the blocker".to_owned()],
                ..NarrativeMemory::default()
            },
            ..WorkingMemory::default()
        };
        let full = memory.render();
        let full_tokens = estimate_text_tokens(&full);

        let one_less = memory.render_with_limit(full_tokens - 1);
        assert_eq!(
            one_less.commands, 2,
            "the oldest command is dropped first: {}",
            one_less.text
        );
        assert_eq!(one_less.files, 2, "files survive while a command is left");
        assert!(!one_less.text.contains("old command"));
        assert!(one_less.text.contains("middle command"));
        assert!(one_less.text.contains("new command"));

        let no_commands = WorkingMemory {
            commands: Vec::new(),
            ..memory.clone()
        };
        let no_command_tokens = estimate_text_tokens(&no_commands.render());
        let with_files = memory.render_with_limit(no_command_tokens);
        assert_eq!(with_files.commands, 0, "all commands go before any file");
        assert_eq!(with_files.files, 2, "{}", with_files.text);

        let only_narrative = WorkingMemory {
            files: Vec::new(),
            commands: Vec::new(),
            ..memory.clone()
        };
        let narrative_tokens = estimate_text_tokens(&only_narrative.render());
        let no_files = memory.render_with_limit(narrative_tokens);
        assert_eq!(no_files.commands, 0);
        assert_eq!(no_files.files, 0);
        assert!(
            no_files.text.contains("keep the goal"),
            "the goal is never dropped: {}",
            no_files.text
        );
        assert!(
            no_files.text.contains("keep the blocker"),
            "blockers are never dropped: {}",
            no_files.text
        );

        assert_eq!(
            memory.render_with_stats().1.files_remembered,
            2,
            "the production limit keeps this small block intact"
        );
    }
}
