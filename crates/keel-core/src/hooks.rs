//! Command hooks at the session and tool boundaries Claude Code and Codex share.
//!
//! Both harnesses read a workspace JSON file of matcher groups. Claude Code
//! stores them under `hooks` in `.claude/settings.json`. Codex stores the same
//! object in `.codex/hooks.json`. A group names an event (`PreToolUse`,
//! `PostToolUse`, `SessionStart`), an optional tool matcher, and command
//! handlers. Each command receives one JSON object on stdin and reports with
//! its exit code and stdout.
//!
//! Keel runs those three events and only `type: command` handlers. It does not
//! load plugins, prompt hooks, HTTP hooks, or a marketplace. A hook is a child
//! process. The keel process is not placed in a sandbox to run it, and the
//! command is not routed through the `bash` tool, so a `--sandbox` flag on
//! `bash` and `jobs` does not confine the hook or the keel process.
//!
//! Exit status is stricter than Claude Code's default. Claude Code and Codex
//! block a `PreToolUse` call on exit code 2, and treat other non-zero codes as
//! non-blocking unless the hook asks to fail closed. Keel blocks the tool on
//! any non-zero exit, and the error includes the hook command. Exit 0 with a
//! JSON `permissionDecision` of `deny`, or a top-level `decision` of `block`,
//! also blocks. That JSON decision is the shape both harnesses accept.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// One tool call a `PreToolUse` or `PostToolUse` hook inspects.
pub struct ToolHook<'a> {
    pub session_id: &'a str,
    pub cwd: &'a Path,
    pub tool: &'a str,
    pub tool_use_id: &'a str,
    pub tool_input: &'a Value,
    pub cancel: &'a CancellationToken,
}

/// Hook configuration loaded from the workspace.
#[derive(Clone, Debug, Default)]
pub struct HookConfig {
    pre: Vec<HookHandler>,
    post: Vec<HookHandler>,
    session: Vec<HookHandler>,
    warnings: Vec<String>,
}

#[derive(Clone, Debug)]
struct HookHandler {
    matcher: Option<String>,
    /// The command string as written, plus any args, used to name the hook.
    label: String,
    command: String,
    args: Vec<String>,
    timeout: Duration,
}

impl HookConfig {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty() && self.session.is_empty()
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Loads `.claude/settings.json` and `.codex/hooks.json` from `workspace`.
    /// A missing file is ignored. A file that cannot be read or parsed is
    /// skipped and named in [`HookConfig::warnings`]. Handlers from both files
    /// run, Claude Code's file first.
    pub fn load(workspace: &Path) -> Self {
        let mut config = Self::empty();
        config.merge_file(&workspace.join(".claude").join("settings.json"));
        config.merge_file(&workspace.join(".codex").join("hooks.json"));
        config
    }

    /// Runs `SessionStart` handlers. Plain stdout, or JSON
    /// `hookSpecificOutput.additionalContext`, is returned so the caller can
    /// append it to the system prompt. A non-zero exit does not stop the
    /// session; the failure is included in the returned text and names the
    /// hook. `None` means there was nothing to add.
    pub async fn session_start(
        &self,
        session_id: &str,
        cwd: &Path,
        source: &str,
        cancel: &CancellationToken,
    ) -> Option<String> {
        if self.session.is_empty() {
            return None;
        }
        let payload = json!({
            "session_id": session_id,
            "cwd": cwd.display().to_string(),
            "hook_event_name": "SessionStart",
            "source": source,
        });
        let mut parts = Vec::new();
        for handler in self.matching(&self.session, source) {
            if cancel.is_cancelled() {
                break;
            }
            let outcome = handler.run(&payload, cwd, cancel).await;
            if let Some(text) = session_text(&handler.label, outcome) {
                parts.push(text);
            }
        }
        let joined = parts.join("\n\n");
        if joined.trim().is_empty() {
            None
        } else {
            Some(joined)
        }
    }

    /// Runs matching `PreToolUse` handlers. `Ok` means the tool may run.
    /// `Err` is the tool error: the tool must not run, and the message names
    /// the hook that blocked it.
    pub async fn pre_tool(&self, call: &ToolHook<'_>) -> Result<(), String> {
        self.run_tool_event("PreToolUse", &self.pre, call, None)
            .await
    }

    /// Runs matching `PostToolUse` handlers after the tool has finished.
    /// A non-zero exit cannot undo the tool. The returned error names the hook
    /// so the caller can mark the tool result failed.
    pub async fn post_tool(&self, call: &ToolHook<'_>, tool_response: &str) -> Result<(), String> {
        self.run_tool_event("PostToolUse", &self.post, call, Some(tool_response))
            .await
    }

    async fn run_tool_event(
        &self,
        event: &str,
        handlers: &[HookHandler],
        call: &ToolHook<'_>,
        tool_response: Option<&str>,
    ) -> Result<(), String> {
        let names = hook_match_names(call.tool);
        let matched: Vec<&HookHandler> = handlers
            .iter()
            .filter(|handler| matcher_matches(handler.matcher.as_deref(), &names))
            .collect();
        if matched.is_empty() {
            return Ok(());
        }
        let mut payload = json!({
            "session_id": call.session_id,
            "cwd": call.cwd.display().to_string(),
            "hook_event_name": event,
            "tool_name": hook_tool_name(call.tool),
            "tool_use_id": call.tool_use_id,
            "tool_input": call.tool_input,
        });
        if let Some(response) = tool_response {
            payload["tool_response"] = Value::String(response.to_owned());
        }
        for handler in matched {
            if call.cancel.is_cancelled() {
                return Err("cancelled".to_owned());
            }
            let outcome = handler.run(&payload, call.cwd, call.cancel).await;
            if let Some(detail) = outcome.block_reason() {
                return Err(hook_error(event, &handler.label, &detail));
            }
        }
        Ok(())
    }

    fn matching<'a>(&'a self, handlers: &'a [HookHandler], source: &str) -> Vec<&'a HookHandler> {
        handlers
            .iter()
            .filter(|handler| matcher_matches(handler.matcher.as_deref(), &[source]))
            .collect()
    }

    fn merge_file(&mut self, path: &Path) {
        if !path.is_file() {
            return;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                self.warnings.push(format!(
                    "could not read hooks from {}: {error}",
                    path.display()
                ));
                return;
            }
        };
        if text.len() > MAX_FILE_BYTES {
            self.warnings.push(format!(
                "hooks file {} is larger than 1 MiB and was skipped",
                path.display()
            ));
            return;
        }
        let parsed: FileShape = match serde_json::from_str(&text) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.warnings.push(format!(
                    "could not parse hooks in {}: {error}",
                    path.display()
                ));
                return;
            }
        };
        self.add_map(path, &parsed.hooks);
        self.add_map(path, &parsed.root);
    }

    fn add_map(&mut self, path: &Path, map: &HookMap) {
        let mut pre = Vec::new();
        let mut post = Vec::new();
        let mut session = Vec::new();
        collect_groups(
            path,
            "PreToolUse",
            &map.pre_tool_use,
            &mut pre,
            &mut self.warnings,
        );
        collect_groups(
            path,
            "PostToolUse",
            &map.post_tool_use,
            &mut post,
            &mut self.warnings,
        );
        collect_groups(
            path,
            "SessionStart",
            &map.session_start,
            &mut session,
            &mut self.warnings,
        );
        self.pre.extend(pre);
        self.post.extend(post);
        self.session.extend(session);
    }
}

fn collect_groups(
    path: &Path,
    event: &str,
    groups: &[HookGroupJson],
    into: &mut Vec<HookHandler>,
    warnings: &mut Vec<String>,
) {
    for group in groups {
        let matcher = group
            .matcher
            .as_deref()
            .map(str::trim)
            .filter(|matcher| !matcher.is_empty())
            .map(str::to_owned);
        for hook in &group.hooks {
            let kind = hook.kind.as_deref().unwrap_or("command").trim();
            if !kind.eq_ignore_ascii_case("command") {
                warnings.push(format!(
                    "hooks in {}: skipped {event} hook of type `{kind}` (only command hooks run)",
                    path.display()
                ));
                continue;
            }
            if hook.async_hook == Some(true) {
                warnings.push(format!(
                    "hooks in {}: skipped async {event} command hook (only synchronous hooks run)",
                    path.display()
                ));
                continue;
            }
            let Some(command) = hook
                .command
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
            else {
                warnings.push(format!(
                    "hooks in {}: skipped a {event} command hook with an empty command",
                    path.display()
                ));
                continue;
            };
            let args = hook.args.clone().unwrap_or_default();
            let label = if args.is_empty() {
                command.to_owned()
            } else {
                format!("{command} {}", args.join(" "))
            };
            into.push(HookHandler {
                matcher: matcher.clone(),
                label,
                command: command.to_owned(),
                args,
                timeout: timeout_of(hook.timeout.as_ref()),
            });
        }
    }
}

fn timeout_of(value: Option<&Value>) -> Duration {
    let Some(value) = value else {
        return DEFAULT_TIMEOUT;
    };
    let secs = value
        .as_u64()
        .or_else(|| value.as_f64().map(|number| number as u64))
        .unwrap_or(0);
    if secs == 0 {
        DEFAULT_TIMEOUT
    } else {
        Duration::from_secs(secs.min(MAX_TIMEOUT.as_secs()))
    }
}

#[derive(Debug, Deserialize)]
struct FileShape {
    #[serde(default)]
    hooks: HookMap,
    #[serde(flatten)]
    root: HookMap,
}

#[derive(Debug, Default, Deserialize)]
struct HookMap {
    #[serde(default, rename = "PreToolUse")]
    pre_tool_use: Vec<HookGroupJson>,
    #[serde(default, rename = "PostToolUse")]
    post_tool_use: Vec<HookGroupJson>,
    #[serde(default, rename = "SessionStart")]
    session_start: Vec<HookGroupJson>,
}

#[derive(Debug, Deserialize)]
struct HookGroupJson {
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<HookJson>,
}

#[derive(Debug, Deserialize)]
struct HookJson {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    timeout: Option<Value>,
    #[serde(default, rename = "async")]
    async_hook: Option<bool>,
}

struct HookRun {
    status: HookStatus,
    stdout: String,
    stderr: String,
}

enum HookStatus {
    Exited(i32),
    Failed(String),
    Cancelled,
}

impl HookRun {
    /// `Some` when the tool must not proceed. Exit 0 with no deny decision is
    /// `None`.
    fn block_reason(&self) -> Option<String> {
        match &self.status {
            HookStatus::Cancelled => Some("cancelled".to_owned()),
            HookStatus::Failed(message) => Some(message.clone()),
            HookStatus::Exited(0) => json_block_reason(&self.stdout),
            HookStatus::Exited(code) => {
                let detail = self.stderr.trim();
                if detail.is_empty() {
                    Some(format!("exited {code}"))
                } else {
                    Some(detail.to_owned())
                }
            }
        }
    }
}

impl HookHandler {
    async fn run(&self, payload: &Value, cwd: &Path, cancel: &CancellationToken) -> HookRun {
        if cancel.is_cancelled() {
            return HookRun {
                status: HookStatus::Cancelled,
                stdout: String::new(),
                stderr: String::new(),
            };
        }
        let body = match serde_json::to_vec(payload) {
            Ok(body) => body,
            Err(error) => {
                return HookRun {
                    status: HookStatus::Failed(format!("could not encode hook input: {error}")),
                    stdout: String::new(),
                    stderr: String::new(),
                };
            }
        };
        let mut command = spawn_command(self, cwd);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return HookRun {
                    status: HookStatus::Failed(format!("failed to start: {error}")),
                    stdout: String::new(),
                    stderr: String::new(),
                };
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&body).await;
            let _ = stdin.shutdown().await;
        }
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let read_stdout = read_capped(stdout, MAX_OUTPUT_BYTES);
        let read_stderr = read_capped(stderr, MAX_OUTPUT_BYTES);
        let wait = child.wait();
        let finished = tokio::select! {
            _ = cancel.cancelled() => None,
            result = tokio::time::timeout(self.timeout, async { tokio::join!(read_stdout, read_stderr, wait) }) => {
                Some(result)
            }
        };
        let Some(result) = finished else {
            return HookRun {
                status: HookStatus::Cancelled,
                stdout: String::new(),
                stderr: String::new(),
            };
        };
        match result {
            Err(_) => HookRun {
                status: HookStatus::Failed(format!("timed out after {}s", self.timeout.as_secs())),
                stdout: String::new(),
                stderr: String::new(),
            },
            Ok((stdout, stderr, status)) => match status {
                Ok(status) => HookRun {
                    status: match status.code() {
                        Some(code) => HookStatus::Exited(code),
                        None => HookStatus::Failed("exited without a status".to_owned()),
                    },
                    stdout: lossy(&stdout),
                    stderr: lossy(&stderr),
                },
                Err(error) => HookRun {
                    status: HookStatus::Failed(format!("failed while waiting: {error}")),
                    stdout: lossy(&stdout),
                    stderr: lossy(&stderr),
                },
            },
        }
    }
}

fn spawn_command(handler: &HookHandler, cwd: &Path) -> Command {
    let mut command = if handler.args.is_empty() {
        let mut command = Command::new(hook_shell());
        command.arg(hook_shell_flag()).arg(&handler.command);
        command
    } else {
        let mut command = Command::new(&handler.command);
        command.args(&handler.args);
        command
    };
    command
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    command
}

fn hook_shell() -> &'static str {
    #[cfg(unix)]
    {
        "/bin/sh"
    }
    #[cfg(windows)]
    {
        "cmd"
    }
}

fn hook_shell_flag() -> &'static str {
    #[cfg(unix)]
    {
        "-c"
    }
    #[cfg(windows)]
    {
        "/C"
    }
}

async fn read_capped(pipe: Option<impl tokio::io::AsyncRead + Unpin>, cap: usize) -> Vec<u8> {
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut buf = vec![0; cap];
    let mut filled = 0;
    while filled < cap {
        match pipe.read(&mut buf[filled..]).await {
            Ok(0) | Err(_) => break,
            Ok(n) => filled += n,
        }
    }
    buf.truncate(filled);
    buf
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

fn hook_error(event: &str, label: &str, detail: &str) -> String {
    let detail = detail.trim();
    if detail.is_empty() {
        format!("{event} hook error [{label}]")
    } else {
        format!("{event} hook error [{label}]: {detail}")
    }
}

fn json_block_reason(stdout: &str) -> Option<String> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    if let Some(decision) = value
        .pointer("/hookSpecificOutput/permissionDecision")
        .and_then(Value::as_str)
    {
        if decision.eq_ignore_ascii_case("deny") {
            let reason = value
                .pointer("/hookSpecificOutput/permissionDecisionReason")
                .and_then(Value::as_str)
                .or_else(|| value.get("reason").and_then(Value::as_str))
                .unwrap_or("denied by hook");
            return Some(reason.to_owned());
        }
    }
    if value
        .get("decision")
        .and_then(Value::as_str)
        .is_some_and(|decision| decision.eq_ignore_ascii_case("block"))
    {
        let reason = value
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("blocked by hook");
        return Some(reason.to_owned());
    }
    None
}

fn session_text(label: &str, outcome: HookRun) -> Option<String> {
    if matches!(outcome.status, HookStatus::Cancelled) {
        return None;
    }
    let mut parts = Vec::new();
    let failed = outcome.block_reason();
    if let Some(detail) = &failed {
        parts.push(hook_error("SessionStart", label, detail));
    }
    let trimmed = outcome.stdout.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if let Some(text) = value
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            parts.push(text.to_owned());
        }
    } else if failed.is_none() && !trimmed.is_empty() {
        parts.push(trimmed.to_owned());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// Names a hook command is matched against. The model calls keel's lowercase
/// tool name; Claude Code and Codex match `Bash`, `Read`, `Edit`, and so on.
pub(crate) fn hook_tool_name(tool: &str) -> &str {
    match tool {
        "bash" => "Bash",
        "read" => "Read",
        "write" => "Write",
        "edit" => "Edit",
        "glob" => "Glob",
        "search" => "Grep",
        "apply_patch" => "apply_patch",
        "skill" => "Skill",
        other => other,
    }
}

fn hook_match_names(tool: &str) -> [&str; 2] {
    [tool, hook_tool_name(tool)]
}

/// Claude Code matchers are exact names, or a `|` / `,` list, or `*`. Codex
/// matchers are regular expressions; the common form is `^Bash$`. This accepts
/// that shared subset: optional `^` and `$` anchors, exact case-insensitive
/// names, and `|` or `,` lists. A full regular expression is not evaluated.
pub(crate) fn matcher_matches(matcher: Option<&str>, names: &[&str]) -> bool {
    let Some(matcher) = matcher.map(str::trim).filter(|matcher| !matcher.is_empty()) else {
        return true;
    };
    if matcher == "*" || matcher == ".*" {
        return true;
    }
    let parts: Vec<&str> = if matcher.contains('|') {
        matcher.split('|').collect()
    } else if matcher.contains(',') {
        matcher.split(',').collect()
    } else {
        vec![matcher]
    };
    for part in parts {
        let part = part.trim();
        let part = part.strip_prefix('^').unwrap_or(part);
        let part = part.strip_suffix('$').unwrap_or(part);
        if part.is_empty() || part == "*" || part == ".*" {
            return true;
        }
        if names.iter().any(|name| part.eq_ignore_ascii_case(name)) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_matchers_from_both_harnesses_match_the_keel_tool() {
        let names = hook_match_names("bash");
        assert!(matcher_matches(Some("Bash"), &names));
        assert!(matcher_matches(Some("^Bash$"), &names));
        assert!(matcher_matches(Some("Bash|Read"), &names));
        assert!(matcher_matches(Some("Edit, Bash"), &names));
        assert!(matcher_matches(None, &names));
        assert!(matcher_matches(Some("*"), &names));
        assert!(!matcher_matches(Some("Read"), &names));
        assert!(!matcher_matches(Some("^Edit$"), &names));
    }

    #[test]
    fn settings_and_hooks_json_contribute_command_hooks() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join(".claude")).unwrap();
        std::fs::create_dir_all(workspace.path().join(".codex")).unwrap();
        std::fs::write(
            workspace.path().join(".claude/settings.json"),
            r#"{
                "permissions": {"allow": []},
                "hooks": {
                    "PreToolUse": [{
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": "sh hooks/allow.sh"}]
                    }],
                    "PostToolUse": [{
                        "matcher": "^Bash$",
                        "hooks": [
                            {"type": "http", "url": "http://127.0.0.1/nope"},
                            {"type": "command", "command": "sh hooks/after.sh", "timeout": 5}
                        ]
                    }]
                }
            }"#,
        )
        .unwrap();
        std::fs::write(
            workspace.path().join(".codex/hooks.json"),
            r#"{
                "hooks": {
                    "SessionStart": [{
                        "matcher": "startup|resume",
                        "hooks": [{"type": "command", "command": "sh hooks/start.sh"}]
                    }]
                }
            }"#,
        )
        .unwrap();

        let config = HookConfig::load(workspace.path());
        assert_eq!(config.pre.len(), 1);
        assert_eq!(config.pre[0].label, "sh hooks/allow.sh");
        assert_eq!(config.post.len(), 1);
        assert_eq!(config.post[0].label, "sh hooks/after.sh");
        assert_eq!(config.post[0].timeout, Duration::from_secs(5));
        assert_eq!(config.session.len(), 1);
        assert!(
            config
                .warnings()
                .iter()
                .any(|warning| warning.contains("type `http`"))
        );
        assert!(matcher_matches(
            config.pre[0].matcher.as_deref(),
            &hook_match_names("bash")
        ));
    }

    #[test]
    fn a_top_level_codex_event_and_a_bad_file_are_handled() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join(".codex")).unwrap();
        std::fs::write(
            workspace.path().join(".codex/hooks.json"),
            r#"{"PreToolUse":[{"hooks":[{"type":"command","command":"true"}]}]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(workspace.path().join(".claude")).unwrap();
        std::fs::write(workspace.path().join(".claude/settings.json"), "not-json").unwrap();

        let config = HookConfig::load(workspace.path());
        assert_eq!(config.pre.len(), 1);
        assert!(config.pre[0].matcher.is_none());
        assert!(
            config
                .warnings()
                .iter()
                .any(|warning| warning.contains("not-json") || warning.contains("parse"))
        );
    }

    #[test]
    fn an_exit_zero_json_deny_blocks_and_names_the_decision() {
        let run = HookRun {
            status: HookStatus::Exited(0),
            stdout: r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"no rm"}}"#.to_owned(),
            stderr: String::new(),
        };
        assert_eq!(run.block_reason().as_deref(), Some("no rm"));
        let message = hook_error("PreToolUse", "sh hooks/block.sh", "no rm");
        assert!(message.contains("sh hooks/block.sh"));
        assert!(message.contains("no rm"));
    }
}
