use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::tool::{
    Tool, ToolContext, ToolError, ToolFuture, ToolOutcome, ToolSpec, parse_arguments, tool_spec,
};
use crate::truncate::truncate_output;

/// The one-shot `bash` tool.
///
/// The shell is new on every call, but its working directory is not: the start
/// directory is read from [`ToolContext::session_state`], and the directory the
/// command finished in is written back. Without a session store the directory
/// lives in this tool instance, so a default registry still remembers it for
/// the rest of the run.
pub struct BashTool {
    /// Both the call lock and the directory for contexts without a session
    /// store. It is held across the command so two concurrent calls cannot
    /// read the same start and then race to record different ends.
    cwd: tokio::sync::Mutex<Option<PathBuf>>,
}

impl BashTool {
    pub fn new() -> Self {
        Self {
            cwd: tokio::sync::Mutex::new(None),
        }
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct BashArguments {
    command: String,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(
            "bash",
            "Use when: you need to run tests, builds, formatters, version control, or another command no structured tool covers. \
             The working directory persists between calls and across `--resume` on Unix, so a `cd` in one call is still in effect in the next; do not re-issue it. Windows starts every call in the workspace root.\n\
             Prefer: `read`, `write`, `edit`, `apply_patch`, `glob`, and `search` for file work; their paths always resolve from the workspace root, not from this shell's current directory. Avoid `find`, `ls`, `stat`, `du`, and `tree` when `glob` can list files.\n\
             Returns: `exit: N` followed by stdout and stderr; large output is truncated in the middle by the harness limit. A remembered directory that no longer exists falls back to the workspace root and adds one `note:` line.\n\
             On failure: a non-zero exit is an error result with the code and output plus a retry hint; a timeout names the command and limit; cancellation says the run was cancelled.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Command line to run."},
                    "workdir": {"type": "string", "description": "Working directory relative to the workspace root. Starts this call there and is remembered like a `cd`."},
                    "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds, capped by the harness limit."}
                },
                "required": ["command"]
            }),
        )
    }

    fn summarize(&self, arguments: &Value) -> String {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let first_line = command.lines().next().unwrap_or("");
        if first_line.chars().count() > 120 {
            let short: String = first_line.chars().take(120).collect();
            format!("bash: {short}…")
        } else {
            format!("bash: {first_line}")
        }
    }

    fn execute<'a>(&'a self, arguments: Value, context: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            match self.run(arguments, context).await {
                Ok(outcome) => outcome,
                Err(error) => error.into_outcome(),
            }
        })
    }
}

impl BashTool {
    async fn run(&self, arguments: Value, context: &ToolContext) -> Result<ToolOutcome, ToolError> {
        let arguments: BashArguments = parse_arguments(arguments)?;

        // One call at a time: the directory this call reads at the start must
        // belong to the directory it records at the end. The run loop already
        // treats bash as a barrier; this is for an embedder calling it
        // concurrently. Only Unix writes the directory back.
        #[cfg(unix)]
        let mut memory = self.cwd.lock().await;
        #[cfg(not(unix))]
        let memory = self.cwd.lock().await;

        let recorded = match arguments.workdir.as_deref() {
            Some(_) => None,
            None => match &context.session_state {
                Some(state) => state.bash_cwd(),
                None => memory.clone(),
            },
        };
        let had_record = recorded.is_some();

        let workspace = context.workspace.root().to_path_buf();
        #[cfg_attr(not(unix), allow(unused_mut))]
        let (start, mut note) = match arguments.workdir.as_deref() {
            Some(workdir) => (context.workspace.resolve_dir(Some(workdir))?, None),
            None => match recorded {
                Some(cwd) if cwd.is_dir() => (cwd, None),
                Some(cwd) => (
                    workspace.clone(),
                    Some(format!(
                        "the recorded working directory `{}` no longer exists; \
                         this call starts in the workspace root `{}`",
                        cwd.display(),
                        workspace.display()
                    )),
                ),
                None => (workspace.clone(), None),
            },
        };

        let timeout = arguments
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(context.limits.timeout)
            .min(context.limits.timeout);

        let program = bash_program();
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut sandbox = crate::sandbox::for_child("bash", context.sandbox, &workspace)?;

        // On Unix the shell reports its final directory through this file. If
        // the file cannot be created, the call still runs exactly as it did
        // before persistence existed; only the record is skipped.
        #[cfg(unix)]
        let state_file = match CwdStateFile::create() {
            Ok(file) => Some(file),
            Err(error) => {
                note.get_or_insert_with(|| {
                    format!(
                        "could not create a working-directory state file ({error}); \
                         this call does not update the remembered directory"
                    )
                });
                None
            }
        };

        #[cfg(target_os = "macos")]
        if let (Some(sandbox), Some(state_file)) = (sandbox.as_mut(), state_file.as_ref()) {
            sandbox.allow_state_file(&state_file.path);
        }

        // A sandboxed child cannot open the state file by its `/tmp` path.
        // Keep an append handle and let the child write through an inherited
        // descriptor instead. Flag off keeps the path trailer unchanged.
        #[cfg(target_os = "linux")]
        let state_append = if sandbox.is_some() {
            match state_file.as_ref().map(CwdStateFile::open_append) {
                Some(Ok(file)) => Some(file),
                Some(Err(error)) => {
                    note.get_or_insert_with(|| {
                        format!(
                            "could not keep the working-directory state file open ({error}); \
                             this call does not update the remembered directory"
                        )
                    });
                    None
                }
                None => None,
            }
        } else {
            None
        };
        #[cfg(unix)]
        let pass_fd = {
            #[cfg(target_os = "linux")]
            {
                state_append.is_some()
            }
            #[cfg(not(target_os = "linux"))]
            {
                false
            }
        };

        let build = |start: &Path, state: Option<&Path>| {
            let mut command = crate::sandbox::tokio_command(&program, sandbox.as_ref());
            command
                .current_dir(start)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);

            // Windows Python defaults to the ANSI code page when its output is
            // a pipe, so non-ASCII text arrives as replacement characters.
            // Force UTF-8 for the child so a command has the same transcript on
            // every platform.
            #[cfg(windows)]
            command
                .env("PYTHONUTF8", "1")
                .env("PYTHONIOENCODING", "utf-8");

            #[cfg(unix)]
            if pass_fd {
                #[cfg(target_os = "linux")]
                {
                    use std::os::fd::AsRawFd;
                    // Home is unreachable, so a login shell's profile read shows
                    // up as a permission error. Skip it. The inherited
                    // environment, including PATH, is unchanged.
                    command
                        .arg("--noprofile")
                        .arg("--norc")
                        .arg("-c")
                        .arg(CWD_TRAILER_FD)
                        .arg("bash")
                        .arg(&arguments.command)
                        .arg(start)
                        .arg(&workspace)
                        .arg(crate::sandbox::STATE_FD.to_string());
                    if let Some(file) = state_append.as_ref() {
                        crate::sandbox::inherit_state_fd(&mut command, file.as_raw_fd());
                    }
                }
            } else if sandbox.is_none() {
                if let Some(state) = state {
                    command
                        .arg("-lc")
                        .arg(CWD_TRAILER)
                        .arg("bash")
                        .arg(&arguments.command)
                        .arg(start)
                        .arg(&workspace)
                        .arg(state);
                } else {
                    command.arg("-lc").arg(&arguments.command);
                }
            } else if let Some(state) = state {
                command
                    .arg("--noprofile")
                    .arg("--norc")
                    .arg("-c")
                    .arg(CWD_TRAILER)
                    .arg("bash")
                    .arg(&arguments.command)
                    .arg(start)
                    .arg(&workspace)
                    .arg(state);
            } else {
                command
                    .arg("--noprofile")
                    .arg("--norc")
                    .arg("-c")
                    .arg(&arguments.command);
            }
            #[cfg(not(unix))]
            {
                let _ = (state, &sandbox);
                command.arg("-lc").arg(&arguments.command);
            }

            #[cfg(target_os = "linux")]
            if let Some(sandbox) = sandbox.as_ref() {
                crate::sandbox::apply_env_tokio(&mut command, sandbox);
                crate::sandbox::confine_tokio(&mut command, sandbox);
            }

            command
        };

        #[cfg(unix)]
        let state_path = if pass_fd || (cfg!(target_os = "linux") && sandbox.is_some()) {
            None
        } else {
            state_file.as_ref().map(|file| file.path.as_path())
        };
        #[cfg(not(unix))]
        let state_path: Option<&Path> = None;

        let child = match build(&start, state_path).spawn() {
            Ok(child) => child,
            // A recorded directory can disappear between the check and the
            // spawn. Retry once from the workspace root and say so, rather
            // than failing a call over a stale record.
            Err(error) if had_record && arguments.workdir.is_none() => {
                note = Some(format!(
                    "the recorded working directory `{}` could not be entered ({error}); \
                     this call starts in the workspace root `{}`",
                    start.display(),
                    workspace.display()
                ));
                build(&workspace, state_path).spawn().map_err(|error| {
                    ToolError::Io(format!(
                        "could not start bash ({}) for command `{}`: {error}; check that bash is \
                         installed and on PATH",
                        program.to_string_lossy(),
                        arguments.command
                    ))
                })?
            }
            Err(error) => {
                return Err(ToolError::Io(format!(
                    "could not start bash ({}) for command `{}`: {error}; check that bash is \
                     installed and on PATH",
                    program.to_string_lossy(),
                    arguments.command
                )));
            }
        };

        let output = tokio::select! {
            _ = context.cancel.cancelled() => return Err(ToolError::Cancelled),
            result = tokio::time::timeout(timeout, child.wait_with_output()) => {
                match result {
                    Ok(Ok(output)) => output,
                    Ok(Err(error)) => {
                        return Err(ToolError::Io(format!(
                            "bash failed while running `{}`: {error}; the command may have been interrupted",
                            arguments.command
                        )));
                    }
                    Err(_) => {
                        let hint = if timeout < context.limits.timeout {
                            format!(
                                "shorten the command, or raise timeout_ms (the harness cap is {:?})",
                                context.limits.timeout
                            )
                        } else {
                            "this is the harness cap, so shorten the command \
                             (raising timeout_ms will not help)"
                                .to_owned()
                        };
                        return Err(ToolError::Timeout {
                            after: timeout,
                            message: format!("bash was running `{}`; {hint}", arguments.command),
                        });
                    }
                }
            }
        };

        #[cfg(unix)]
        if let Some(recorded) = state_file.as_ref().and_then(CwdStateFile::read) {
            if recorded.fallback && note.is_none() {
                note = Some(format!(
                    "the working directory `{}` could not be entered; \
                     this call started in the workspace root `{}`",
                    start.display(),
                    workspace.display()
                ));
            }
            let accepted_cwd = recorded.cwd.and_then(|cwd| {
                if !cwd.is_absolute() {
                    return None;
                }
                if sandbox.is_none() {
                    return Some(cwd);
                }
                confined_cwd(&cwd, &workspace)
            });
            if let Some(cwd) = accepted_cwd {
                match &context.session_state {
                    Some(state) => state.set_bash_cwd(&cwd),
                    None => *memory = Some(cwd),
                }
            }
        }

        let code = output.status.code().unwrap_or(-1);
        let mut text = format!("exit: {code}");
        if let Some(note) = note {
            text.push_str("\nnote: ");
            text.push_str(&note);
        }

        let stdout = normalize_newlines(&output.stdout);
        let stderr = normalize_newlines(&output.stderr);
        if !stdout.trim().is_empty() {
            text.push_str("\nstdout:\n");
            text.push_str(stdout.trim_end());
        }
        if !stderr.trim().is_empty() {
            text.push_str("\nstderr:\n");
            text.push_str(stderr.trim_end());
        }
        if !output.status.success() {
            text.push_str(
                "\nIf this exit was not expected, read stderr and fix the command or the code, \
                 then retry",
            );
        }

        Ok(ToolOutcome {
            text: truncate_output(&text, context.limits.max_output_bytes),
            is_error: !output.status.success(),
            ..ToolOutcome::default()
        })
    }
}

/// Runs the caller's command in a login shell and writes the shell's final
/// physical directory to `$4`, one path per line.
///
/// Arguments are captured into shell variables and then cleared, so the
/// command's `$0` and positional parameters look as they do without the
/// trailer. The `EXIT` trap preserves the command's status exactly: it saves
/// `$?`, appends the directory, and re-exits with the saved value. A command
/// that `exec`s or kills its shell skips the trap and leaves the previous
/// record in place; it never changes the observed status or output. If the
/// recorded start cannot be entered, the first line is `fallback` and the
/// command runs from the workspace root instead.
#[cfg(unix)]
const CWD_TRAILER: &str = r#"__keel_command=$1
__keel_start=$2
__keel_workspace=$3
__keel_state=$4
set --
if ! cd -- "$__keel_start" 2>/dev/null; then
  printf 'fallback\n' > "$__keel_state" 2>/dev/null
  cd -- "$__keel_workspace" 2>/dev/null || exit 1
fi
trap '__keel_status=$?; pwd -P >> "$__keel_state" 2>/dev/null; exit $__keel_status' EXIT
eval "$__keel_command"
"#;

/// Same record as [`CWD_TRAILER`], written to an inherited descriptor.
///
/// The sandboxed shell cannot open the state file's path. `$4` is the
/// descriptor number the parent duplicated before exec; writes append.
#[cfg(target_os = "linux")]
const CWD_TRAILER_FD: &str = r#"__keel_command=$1
__keel_start=$2
__keel_workspace=$3
__keel_fd=$4
set --
if ! cd -- "$__keel_start" 2>/dev/null; then
  printf 'fallback\n' >&"$__keel_fd" 2>/dev/null
  cd -- "$__keel_workspace" 2>/dev/null || exit 1
fi
trap '__keel_status=$?; pwd -P >&'"$__keel_fd"' 2>/dev/null; exit $__keel_status' EXIT
eval "$__keel_command"
"#;

/// A uniquely named, empty file the shell appends its final directory to.
#[cfg(unix)]
struct CwdStateFile {
    path: PathBuf,
}

#[cfg(unix)]
impl CwdStateFile {
    fn create() -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir();
        for _ in 0..16 {
            let path = directory.join(format!(
                "keel-bash-cwd-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "no unique path in the temporary directory",
        ))
    }

    /// An append handle the sandboxed child inherits. The path itself is not
    /// reachable once Landlock is installed.
    #[cfg(target_os = "linux")]
    fn open_append(&self) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new().append(true).open(&self.path)
    }

    fn read(&self) -> Option<RecordedCwd> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        parse_recorded_cwd(&text)
    }
}

/// Parses the state file left by [`CWD_TRAILER`] or [`CWD_TRAILER_FD`].
///
/// Uses the last non-empty line for the final directory so anything a command
/// writes to the inherited state descriptor cannot override the `EXIT` trap's
/// `pwd -P` line.
#[cfg(unix)]
fn parse_recorded_cwd(text: &str) -> Option<RecordedCwd> {
    let mut lines = text.lines();
    let first = lines.next()?.trim_end_matches('\r');
    if first == "fallback" {
        let cwd = lines
            .rev()
            .map(|line| line.trim_end_matches('\r'))
            .find(|line| !line.is_empty())
            .map(PathBuf::from);
        return Some(RecordedCwd {
            cwd,
            fallback: true,
        });
    }
    let last = text
        .lines()
        .rev()
        .map(|line| line.trim_end_matches('\r'))
        .find(|line| !line.is_empty())?;
    Some(RecordedCwd {
        cwd: Some(PathBuf::from(last)),
        fallback: false,
    })
}

/// Validates a recorded `bash` working directory when the OS sandbox is active.
///
/// Rejects any path containing `..` components and requires the canonicalized
/// target to stay inside the canonicalized workspace so neither `..` traversal
/// nor symlinks can pin the next sandboxed shell outside the workspace.
#[cfg(unix)]
fn confined_cwd(cwd: &Path, workspace: &Path) -> Option<PathBuf> {
    if !cwd.is_absolute() {
        return None;
    }
    if cwd
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    let workspace_canonical = std::fs::canonicalize(workspace).ok()?;
    let cwd_canonical = std::fs::canonicalize(cwd).ok()?;
    cwd_canonical
        .starts_with(&workspace_canonical)
        .then_some(cwd_canonical)
}

#[cfg(unix)]
impl Drop for CwdStateFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// What the shell left in the state file.
#[cfg(unix)]
struct RecordedCwd {
    /// The final physical directory, when the shell reached its exit trap.
    cwd: Option<PathBuf>,
    /// Whether the recorded start directory could not be entered.
    fallback: bool,
}

/// Command output keeps LF on every platform. Git Bash passes through what a
/// Windows program writes, and Python's text mode turns `\n` into `\r\n`;
/// transcript text should not depend on that.
fn normalize_newlines(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// The shell the `bash` tool runs.
///
/// On Windows, `bash` on PATH is usually `C:\Windows\System32\bash.exe`, the
/// WSL launcher, which fails when no distribution is installed. Git for
/// Windows ships a real bash, so prefer it and let `KEEL_BASH` override it.
fn bash_program() -> std::ffi::OsString {
    #[cfg(windows)]
    {
        windows_bash::program()
    }
    #[cfg(not(windows))]
    {
        std::ffi::OsString::from("bash")
    }
}

#[cfg(windows)]
mod windows_bash {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    pub(super) fn program() -> OsString {
        if let Some(explicit) = std::env::var_os("KEEL_BASH").filter(|value| !value.is_empty()) {
            return explicit;
        }
        if let Some(bash) = candidates().into_iter().find(|path| path.is_file()) {
            return bash.into_os_string();
        }
        OsString::from("bash")
    }

    fn candidates() -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        for (variable, relative) in [
            ("ProgramFiles", "Git/bin/bash.exe"),
            ("ProgramFiles(x86)", "Git/bin/bash.exe"),
            ("LOCALAPPDATA", "Programs/Git/bin/bash.exe"),
        ] {
            if let Some(root) = std::env::var_os(variable) {
                candidates.push(PathBuf::from(root).join(relative));
            }
        }
        // A portable Git may live elsewhere; find `git.exe` on PATH and use the
        // `bin/bash.exe` beside its install root (`Git/cmd/git.exe`,
        // `Git/bin/git.exe` or `Git/mingw64/bin/git.exe`).
        if let Some(path) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&path) {
                for name in ["git.exe", "git"] {
                    let git = directory.join(name);
                    if !git.is_file() {
                        continue;
                    }
                    if let Some(install) = git.parent().and_then(Path::parent) {
                        candidates.push(install.join("bin").join("bash.exe"));
                    }
                }
            }
        }
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolLimits;
    use crate::workspace::Workspace;
    use serde_json::json;
    #[cfg(unix)]
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[cfg(unix)]
    use crate::tool::SessionState;

    #[cfg(unix)]
    #[derive(Debug, Default)]
    struct MemoryState {
        cwd: Mutex<Option<PathBuf>>,
    }

    #[cfg(unix)]
    impl SessionState for MemoryState {
        fn bash_cwd(&self) -> Option<PathBuf> {
            self.cwd
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        fn set_bash_cwd(&self, cwd: &Path) {
            *self
                .cwd
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(cwd.to_path_buf());
        }
    }

    fn context(directory: &tempfile::TempDir, timeout: Duration) -> ToolContext {
        ToolContext {
            workspace: Workspace::open(directory.path()).unwrap(),
            cancel: CancellationToken::new(),
            limits: ToolLimits {
                timeout,
                ..ToolLimits::default()
            },
            session_state: None,
            sandbox: false,
        }
    }

    #[cfg(unix)]
    fn state_context(
        directory: &tempfile::TempDir,
        state: std::sync::Arc<MemoryState>,
    ) -> ToolContext {
        ToolContext {
            workspace: Workspace::open(directory.path()).unwrap(),
            cancel: CancellationToken::new(),
            limits: ToolLimits::default(),
            session_state: Some(state),
            sandbox: false,
        }
    }

    #[cfg(unix)]
    fn workspace_path(context: &ToolContext) -> PathBuf {
        std::fs::canonicalize(context.workspace.root()).unwrap()
    }

    async fn run_in(tool: &BashTool, command: &str, context: &ToolContext) -> crate::ToolOutcome {
        tool.run(json!({ "command": command }), context)
            .await
            .unwrap_or_else(|error| panic!("{command}: {error}"))
    }

    #[test]
    fn command_output_uses_lf_on_every_platform() {
        assert_eq!(
            normalize_newlines(b"first\r\nsecond\nthird\rfour"),
            "first\nsecond\nthird\rfour"
        );
    }

    #[cfg(unix)]
    #[test]
    fn recorded_cwd_uses_last_line_so_earlier_fd_writes_cannot_poison_it() {
        let parsed = parse_recorded_cwd("/etc\n/workspace/sub\n").unwrap();
        assert!(!parsed.fallback);
        assert_eq!(parsed.cwd, Some(PathBuf::from("/workspace/sub")));

        let fallback = parse_recorded_cwd("fallback\n/etc\n/workspace\n").unwrap();
        assert!(fallback.fallback);
        assert_eq!(fallback.cwd, Some(PathBuf::from("/workspace")));

        let fallback_only = parse_recorded_cwd("fallback\n").unwrap();
        assert!(fallback_only.fallback);
        assert_eq!(fallback_only.cwd, None);
    }

    #[cfg(unix)]
    #[test]
    fn confined_cwd_rejects_parent_traversal_and_outside_symlinks() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sub = workspace.path().join("sub");
        std::fs::create_dir(&sub).unwrap();

        let valid = confined_cwd(&sub, workspace.path());
        assert_eq!(valid, Some(std::fs::canonicalize(&sub).unwrap()));

        let traversal = workspace
            .path()
            .join("..")
            .join(outside.path().file_name().unwrap());
        assert_eq!(confined_cwd(&traversal, workspace.path()), None);

        let escape_link = workspace.path().join("link-out");
        std::os::unix::fs::symlink(outside.path(), &escape_link).unwrap();
        assert_eq!(confined_cwd(&escape_link, workspace.path()), None);
    }

    #[tokio::test]
    async fn a_timeout_is_a_typed_tool_error() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, Duration::from_millis(100));

        let error = BashTool::new()
            .run(json!({"command": "sleep 5"}), &context)
            .await
            .expect_err("the command outlives the limit");

        assert!(matches!(error, ToolError::Timeout { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn the_first_call_starts_in_the_workspace_root() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("marker"), "here").unwrap();
        let context = context(&directory, Duration::from_secs(5));

        // A relative marker proves the start directory without comparing
        // Windows and POSIX spellings of the same path.
        let outcome = run_in(&BashTool::new(), "test -f marker && pwd", &context).await;
        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(
            outcome.text.starts_with("exit: 0\nstdout:\n"),
            "{}",
            outcome.text
        );

        #[cfg(unix)]
        {
            let expected = workspace_path(&context);
            assert_eq!(
                outcome.text,
                format!("exit: 0\nstdout:\n{}", expected.display())
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_cd_in_one_call_is_visible_in_the_next() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, Duration::from_secs(5));
        let tool = BashTool::new();

        let first = run_in(&tool, "mkdir -p sub/deeper && cd sub/deeper", &context).await;
        assert!(!first.is_error, "{}", first.text);

        let second = run_in(&tool, "pwd", &context).await;
        assert!(!second.is_error, "{}", second.text);
        assert!(
            second.text.ends_with("/sub/deeper"),
            "the next call did not start in the recorded directory:\n{}",
            second.text
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_session_store_carries_the_directory_to_a_fresh_tool() {
        let directory = tempfile::tempdir().unwrap();
        let state = std::sync::Arc::new(MemoryState::default());
        let context = state_context(&directory, state.clone());

        let first = run_in(
            &BashTool::new(),
            "mkdir -p remembered && cd remembered",
            &context,
        )
        .await;
        assert!(!first.is_error, "{}", first.text);
        assert!(
            state
                .bash_cwd()
                .is_some_and(|cwd| cwd.ends_with("remembered")),
            "the store was not updated: {:?}",
            state.bash_cwd()
        );

        // A fresh tool stands in for a resumed process: only the session
        // store carries the directory.
        let second = run_in(&BashTool::new(), "pwd", &context).await;
        assert!(!second.is_error, "{}", second.text);
        assert!(second.text.ends_with("/remembered"), "{}", second.text);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workdir_starts_the_call_and_is_remembered() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("sub")).unwrap();
        let context = context(&directory, Duration::from_secs(5));
        let tool = BashTool::new();

        let explicit = tool
            .run(json!({"command": "pwd", "workdir": "sub"}), &context)
            .await
            .unwrap();
        assert!(!explicit.is_error, "{}", explicit.text);
        assert!(explicit.text.ends_with("/sub"), "{}", explicit.text);

        let implicit = run_in(&tool, "pwd", &context).await;
        assert!(!implicit.is_error, "{}", implicit.text);
        assert!(implicit.text.ends_with("/sub"), "{}", implicit.text);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_last_command_keeps_its_exact_exit_code() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, Duration::from_secs(5));

        let outcome = run_in(&BashTool::new(), "true; exit 7", &context).await;

        assert!(outcome.is_error);
        assert!(outcome.text.starts_with("exit: 7\n"), "{}", outcome.text);
        assert!(outcome.text.contains("If this exit was not expected"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdout_without_a_trailing_newline_is_unchanged_byte_for_byte() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, Duration::from_secs(5));

        let outcome = run_in(&BashTool::new(), "printf 'no-newline'", &context).await;

        assert_eq!(outcome.text, "exit: 0\nstdout:\nno-newline");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_removed_recorded_directory_falls_back_to_the_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, Duration::from_secs(5));
        let tool = BashTool::new();
        let expected = workspace_path(&context);
        let gone = expected.join("gone");

        let first = run_in(&tool, "mkdir gone && cd gone", &context).await;
        assert!(!first.is_error, "{}", first.text);
        std::fs::remove_dir(&gone).unwrap();

        let second = run_in(&tool, "pwd", &context).await;

        assert!(!second.is_error, "{}", second.text);
        assert!(
            second.text.contains(&format!(
                "note: the recorded working directory `{}` no longer exists",
                gone.display()
            )),
            "the fallback was not explained:\n{}",
            second.text
        );
        assert!(
            second.text.contains(&format!(
                "this call starts in the workspace root `{}`",
                expected.display()
            )),
            "{}",
            second.text
        );
        assert!(
            second.text.ends_with(&expected.display().to_string()),
            "{}",
            second.text
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_directory_deleted_by_the_command_does_not_break_the_next_call() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, Duration::from_secs(5));
        let tool = BashTool::new();

        let first = run_in(&tool, "mkdir doomed && cd doomed", &context).await;
        assert!(!first.is_error, "{}", first.text);

        // The shell's final `pwd -P` fails after the directory is gone, so the
        // stale record is left in place. The removal runs in a subshell: the
        // outer shell stays in the directory it deletes.
        let second = run_in(&tool, "(cd .. && rmdir doomed)", &context).await;
        assert!(!second.is_error, "{}", second.text);

        let third = run_in(&tool, "pwd", &context).await;
        assert!(!third.is_error, "{}", third.text);
        assert!(third.text.contains("no longer exists"), "{}", third.text);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_that_execs_or_kills_its_shell_is_tolerated() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, Duration::from_secs(5));
        let tool = BashTool::new();

        let exec = run_in(&tool, "exec sh -c 'exit 4'", &context).await;
        assert!(exec.is_error);
        assert!(exec.text.starts_with("exit: 4\n"), "{}", exec.text);

        let killed = run_in(&tool, "kill -9 $$", &context).await;
        assert!(killed.is_error);
        assert!(killed.text.starts_with("exit: -1\n"), "{}", killed.text);

        let recovered = run_in(&tool, "printf alive", &context).await;
        assert!(!recovered.is_error, "{}", recovered.text);
        assert_eq!(recovered.text, "exit: 0\nstdout:\nalive");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_calls_leave_a_coherent_record() {
        let directory = tempfile::tempdir().unwrap();
        let state = std::sync::Arc::new(MemoryState::default());
        let context = state_context(&directory, state.clone());
        let tool = BashTool::new();
        let first = tool.run(json!({"command": "mkdir -p one && cd one"}), &context);
        let second = tool.run(json!({"command": "mkdir -p two && cd two"}), &context);

        let (first, second) = tokio::join!(first, second);
        assert!(!first.unwrap().is_error);
        assert!(!second.unwrap().is_error);

        let recorded = state.bash_cwd().expect("a directory was recorded");
        assert!(
            recorded.is_dir(),
            "the record is not a directory: {recorded:?}"
        );
        assert!(
            recorded.ends_with("one") || recorded.ends_with("two"),
            "the record is not one call's final directory: {recorded:?}"
        );

        let third = run_in(&tool, "pwd", &context).await;
        assert!(!third.is_error, "{}", third.text);
        assert!(
            third.text.ends_with(&recorded.display().to_string()),
            "the next call did not start in the recorded directory:\n{}",
            third.text
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_starts_each_call_in_the_workspace_root() {
        let directory = tempfile::tempdir().unwrap();
        let context = context(&directory, Duration::from_secs(5));
        let tool = BashTool::new();

        let base = run_in(&tool, "pwd", &context).await;
        assert!(!base.is_error, "{}", base.text);

        let changed = run_in(&tool, "mkdir -p sub && cd sub && pwd", &context).await;
        assert!(!changed.is_error, "{}", changed.text);
        assert!(changed.text.ends_with("/sub"), "{}", changed.text);
        assert_ne!(changed.text, base.text);

        let after = run_in(&tool, "pwd", &context).await;
        assert!(!after.is_error, "{}", after.text);
        assert_eq!(
            after.text, base.text,
            "Windows must keep the one-shot directory behavior"
        );
    }
}
