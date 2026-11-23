//! OS confinement for `bash` and `jobs` children.
//!
//! - **Linux 5.13+**: Landlock ruleset built in the parent and installed in the
//!   child after `fork` and before `exec`, so the keel process is never
//!   restricted. On Linux 6.12+ (ABI v6), signal and abstract Unix socket
//!   scoping are enabled too. `.git` and `.keel` stay read-only. Landlock
//!   rules only grant access, so the child bind-mounts those paths read-only
//!   in a private mount namespace before applying the ruleset, then blocks
//!   mount and unshare so it cannot undo that. The keel process is not in
//!   that namespace.
//! - **macOS**: Seatbelt policy (`(deny default)` SBPL profile) enforced via
//!   `/usr/bin/sandbox-exec`, confining writes to the canonicalized workspace
//!   and private `TMPDIR` while keeping `.git` and `.keel` read-only.
//!
//! In [`SandboxMode::Workspace`] (`--sandbox`), network stays open. In
//! [`SandboxMode::PlanReadOnly`] (plan mode), the entire workspace (including
//! `.git` and `.keel`) is read-only, writes are restricted to the private
//! `TMPDIR` (also `HOME`), `GIT_OPTIONAL_LOCKS=0` and `GIT_PAGER=cat` are set,
//! and network is denied (Seatbelt `(deny network*)` on macOS; Landlock ABI
//! v4+ TCP rules plus seccomp denying non-`AF_UNIX` `socket(2)` and
//! `io_uring_setup(2)` on Linux 6.7+). On Windows and Linux kernels below
//! Landlock ABI v4, [`plan_sandbox_supported`] returns `false` and plan-mode
//! `bash` is refused rather than silently allowing network.
//! On both platforms, sensitive provider credential env vars are scrubbed from
//! confined children, and each confined command gets a private mode-`0700`
//! writable `TMPDIR` while host `/tmp` and `/home` stay unreachable. On
//! unsupported platforms (or when Landlock / `sandbox-exec` is unavailable),
//! enabling the sandbox is an error; the command is never started unsandboxed.

#![cfg_attr(target_os = "linux", allow(unsafe_code))]

use std::path::Path;

use crate::tool::ToolError;

/// System directories a Linux toolchain has to read and execute.
///
/// Everything else, including host `/tmp`, `/home`, and `/proc`, is
/// unreachable. A rule on the shell's own `/proc/<pid>` does not cover the
/// commands that shell runs, and a private proc mount needs a pid namespace,
/// which would replace the process id `Command` tracks. `/proc` stays
/// unreachable.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const READ_ONLY_PREFIXES: &[&str] = &["/usr", "/bin", "/lib", "/lib64", "/etc"];

/// Character devices that standard shell commands and toolchains open for
/// reading and writing (for example `2>/dev/null`).
#[cfg(target_os = "linux")]
const READ_WRITE_DEVICES: &[&str] = &["/dev/null", "/dev/zero"];

/// Character devices that runtimes and crypto libraries read from.
#[cfg(target_os = "linux")]
const READ_ONLY_DEVICES: &[&str] = &["/dev/urandom", "/dev/random"];

/// Provider credentials and keel secret-path variables removed from sandboxed
/// children so an open network cannot exfiltrate them from the environment.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
pub(crate) const SCRUBBED_ENV_VARS: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENCODE_API_KEY",
    "COMMANDCODE_API_KEY",
    "XAI_API_KEY",
    "NEURALWATT_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "KEEL_CUSTOM_API_KEY",
    "KEEL_CREDENTIALS",
    "KEEL_RECORD_KEY_FILE",
];

/// File descriptor the sandboxed Linux shell uses for the working-directory
/// record.
///
/// The record lives in the temporary directory, which the ruleset does not
/// allow the child to open by path. The parent keeps the file open and the
/// child inherits this descriptor, so the write does not depend on `/tmp`.
#[cfg(target_os = "linux")]
pub(crate) const STATE_FD: i32 = 253;

/// A private, mode-0700 scratch directory exposed to a sandboxed child as
/// `TMPDIR`, removed automatically on drop.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
pub(crate) struct SandboxTempDir {
    path: std::path::PathBuf,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl SandboxTempDir {
    fn create() -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir();
        for _ in 0..16 {
            let path = base.join(format!(
                "keel-sandbox-tmp-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "no unique path in the temporary directory",
        ))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for SandboxTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Whether this host supports the read-only, network-disabled OS sandbox
/// required for `bash` in plan mode (macOS Seatbelt or Linux Landlock ABI v4+).
///
/// The runtime probe is cached once per process so repeated tool-list queries
/// do not re-issue syscalls.
pub fn plan_sandbox_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *SUPPORTED.get_or_init(|| linux::landlock_abi().is_ok_and(|abi| abi >= 4))
    }
    #[cfg(target_os = "macos")]
    {
        static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *SUPPORTED.get_or_init(|| Path::new(macos::SANDBOX_EXEC).is_file())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

/// Whether this host supports standard workspace OS sandboxing
/// (`--sandbox`, enabled by default on supported Unix hosts).
pub fn workspace_sandbox_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *SUPPORTED.get_or_init(|| linux::landlock_abi().is_ok())
    }
    #[cfg(target_os = "macos")]
    {
        static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *SUPPORTED.get_or_init(macos::seatbelt_available)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

/// Returns a one-time startup warning when workspace sandboxing cannot protect
/// `.git` and `.keel` on this host (for example, Linux with unprivileged user
/// namespaces disabled).
pub fn sandbox_startup_warning() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        static WARNING: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
        *WARNING.get_or_init(|| {
            if linux::userns_blocked_by_sysctl() {
                Some(
                    "unprivileged user namespaces are blocked on this host; --sandbox confines writes to the workspace with Landlock, but `.git` and `.keel` remain writable (set KEEL_SANDBOX_STRICT=1 to fail closed instead)",
                )
            } else {
                None
            }
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Confinement level requested for a child process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SandboxMode {
    /// No OS confinement.
    Off,
    /// Standard `--sandbox` confinement: workspace read-write (`.git` and
    /// `.keel` read-only), private `TMPDIR`, network open.
    Workspace,
    /// Plan-mode confinement: workspace (including `.git` and `.keel`) is
    /// read-only, writes are restricted to the private `TMPDIR` (also `HOME`),
    /// network is denied, and `GIT_OPTIONAL_LOCKS=0` / `GIT_PAGER=cat` are set.
    PlanReadOnly,
}

/// A prepared OS sandbox paired with its private `TMPDIR`.
///
/// Hold it until `spawn` returns (and for foreground `bash`, until the command
/// finishes so `temp_dir` stays alive). Background `jobs` move `temp_dir` onto
/// the `Job` via [`Sandbox::take_temp_dir`].
#[derive(Debug)]
pub(crate) struct Sandbox {
    #[cfg(target_os = "linux")]
    ruleset: std::os::fd::OwnedFd,
    /// Absolute `.git` and `.keel` paths, plus the id maps, kept alive until
    /// `spawn` returns. The child reads them from the forked address space.
    #[cfg(target_os = "linux")]
    readonly_trees: linux::ReadonlyTrees,
    #[cfg(target_os = "linux")]
    strict: bool,
    #[cfg(target_os = "macos")]
    seatbelt: macos::SeatbeltConfig,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    temp_dir: Option<SandboxTempDir>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    plan_mode: bool,
}

impl Sandbox {
    /// Transfers ownership of the private `TMPDIR` to a long-lived caller
    /// (such as a background `Job`) so it outlives the `Sandbox`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn take_temp_dir(&mut self) -> Option<SandboxTempDir> {
        self.temp_dir.take()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn temp_dir_path(&self) -> Option<&Path> {
        self.temp_dir.as_ref().map(SandboxTempDir::path)
    }

    /// Permits the sandboxed macOS child to append to `path` for recording its
    /// final working directory without exposing the rest of host `/tmp`.
    #[cfg(target_os = "macos")]
    pub(crate) fn allow_state_file(&mut self, path: &Path) {
        self.seatbelt.set_state_file(path);
    }
}

/// Builds a sandbox configuration when `mode` is not [`SandboxMode::Off`].
///
/// The command must not start when this returns an error: the OS sandbox is
/// missing or a required path could not be configured, and running unsandboxed
/// would hide that.
pub(crate) fn for_child(
    tool: &str,
    mode: SandboxMode,
    workspace: &Path,
) -> Result<Option<Sandbox>, ToolError> {
    let plan_mode = match mode {
        SandboxMode::Off => return Ok(None),
        SandboxMode::Workspace => false,
        SandboxMode::PlanReadOnly => true,
    };
    prepare(workspace, READ_ONLY_PREFIXES, plan_mode)
        .map(Some)
        .map_err(|error| ToolError::Message(format!("{tool}: {error}")))
}

fn prepare(workspace: &Path, read_only: &[&str], plan_mode: bool) -> Result<Sandbox, ToolError> {
    #[cfg(target_os = "linux")]
    {
        linux::prepare(workspace, read_only, plan_mode)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = read_only;
        macos::prepare(workspace, plan_mode)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (workspace, read_only, plan_mode);
        Err(ToolError::Message(
            "OS sandboxing is not available on this operating system; it requires Linux 5.13 or newer (Landlock) or macOS (Seatbelt). Refusing to start the command".to_owned(),
        ))
    }
}

/// Creates a `tokio::process::Command` for `program`, wrapping it in
/// `/usr/bin/sandbox-exec` on macOS when `sandbox` is active.
pub(crate) fn tokio_command(
    program: &std::ffi::OsStr,
    sandbox: Option<&Sandbox>,
) -> tokio::process::Command {
    #[cfg(target_os = "macos")]
    if let Some(sandbox) = sandbox {
        let mut command = tokio::process::Command::new(macos::SANDBOX_EXEC);
        command.args(sandbox.seatbelt.args(program));
        apply_env_tokio(&mut command, sandbox);
        return command;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = sandbox;
    tokio::process::Command::new(program)
}

/// Creates a `std::process::Command` for `program`, wrapping it in
/// `/usr/bin/sandbox-exec` on macOS when `sandbox` is active.
#[cfg(unix)]
pub(crate) fn std_command(
    program: &std::ffi::OsStr,
    sandbox: Option<&Sandbox>,
) -> std::process::Command {
    #[cfg(target_os = "macos")]
    if let Some(sandbox) = sandbox {
        let mut command = std::process::Command::new(macos::SANDBOX_EXEC);
        command.args(sandbox.seatbelt.args(program));
        apply_env_std(&mut command, sandbox);
        return command;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = sandbox;
    std::process::Command::new(program)
}

/// Scrubs sensitive credentials, points `TMPDIR`/`TEMP`/`TMP` at the sandbox's
/// private scratch directory, and in plan mode sets `HOME`,
/// `GIT_OPTIONAL_LOCKS=0`, and `GIT_PAGER=cat` on a tokio command.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn apply_env_tokio(command: &mut tokio::process::Command, sandbox: &Sandbox) {
    for var in SCRUBBED_ENV_VARS {
        command.env_remove(var);
    }
    if let Some(dir) = sandbox.temp_dir_path() {
        command.env("TMPDIR", dir).env("TEMP", dir).env("TMP", dir);
        if sandbox.plan_mode {
            command.env("HOME", dir);
        }
    }
    if sandbox.plan_mode {
        command
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_PAGER", "cat");
    }
}

/// Scrubs sensitive credentials, points `TMPDIR`/`TEMP`/`TMP` at the sandbox's
/// private scratch directory, and in plan mode sets `HOME`,
/// `GIT_OPTIONAL_LOCKS=0`, and `GIT_PAGER=cat` on a std command.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn apply_env_std(command: &mut std::process::Command, sandbox: &Sandbox) {
    for var in SCRUBBED_ENV_VARS {
        command.env_remove(var);
    }
    if let Some(dir) = sandbox.temp_dir_path() {
        command.env("TMPDIR", dir).env("TEMP", dir).env("TMP", dir);
        if sandbox.plan_mode {
            command.env("HOME", dir);
        }
    }
    if sandbox.plan_mode {
        command
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_PAGER", "cat");
    }
}

/// Installs `sandbox` on a tokio command, in the child, before `exec`.
#[cfg(target_os = "linux")]
pub(crate) fn confine_tokio(command: &mut tokio::process::Command, sandbox: &Sandbox) {
    let ruleset_fd = sandbox.ruleset.as_raw_fd();
    let trees = sandbox.readonly_trees.as_raw();
    let plan_mode = sandbox.plan_mode;
    let strict = sandbox.strict;
    // SAFETY: `pre_exec` runs in the forked child before exec. `trees` holds
    // addresses of strings owned by `sandbox`, which the caller keeps alive
    // until `spawn` returns; the child inherits that memory at fork. The
    // closure only issues syscalls (unshare, mount, open, write, seccomp,
    // capset, prctl, landlock) and does not allocate.
    unsafe {
        command.pre_exec(move || linux::confine_current(ruleset_fd, trees, plan_mode, strict));
    }
}

/// Installs `sandbox` on a std command, in the child, before `exec`.
#[cfg(target_os = "linux")]
pub(crate) fn confine_std(command: &mut std::process::Command, sandbox: &Sandbox) {
    use std::os::unix::process::CommandExt;

    let ruleset_fd = sandbox.ruleset.as_raw_fd();
    let trees = sandbox.readonly_trees.as_raw();
    let plan_mode = sandbox.plan_mode;
    let strict = sandbox.strict;
    // SAFETY: same contract as [`confine_tokio`].
    unsafe {
        command.pre_exec(move || linux::confine_current(ruleset_fd, trees, plan_mode, strict));
    }
}

/// Duplicates `source` onto [`STATE_FD`] in the child and clears close-on-exec.
///
/// `source` must stay open until `spawn` returns.
#[cfg(target_os = "linux")]
pub(crate) fn inherit_state_fd(command: &mut tokio::process::Command, source: std::os::fd::RawFd) {
    // SAFETY: `dup2` is async-signal-safe. It runs in the child before exec
    // and does not allocate on the success path.
    unsafe {
        command.pre_exec(move || linux::dup_state_fd(source));
    }
}

#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};

    use super::{Sandbox, SandboxTempDir};
    use crate::tool::ToolError;

    pub(super) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

    pub(super) fn seatbelt_available() -> bool {
        std::process::Command::new(SANDBOX_EXEC)
            .args(["-p", "(version 1)(allow default)", "--", "/usr/bin/true"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Base Seatbelt policy (SBPL) for macOS.
    ///
    /// Paths are supplied via `-D KEY=VALUE` parameters rather than string
    /// interpolation so arbitrary workspace characters cannot alter the Scheme
    /// syntax. Last-match-wins semantics make `.git` and `.keel` read-only
    /// even inside the writable workspace (preventing both direct edits and
    /// lockfile-rename updates to `.git/config` or hooks); case-insensitive
    /// regex rules cover APFS case variants such as `.GIT` and `.KEEL`.
    ///
    /// `file-read-data` on the literal root is required because dyld's cache
    /// finder reads `/` to locate the shared cache; without it dyld aborts
    /// during startup and the child never executes. It exposes only the
    /// top-level name listing: reading any entry still needs its own rule.
    const BASE_PROFILE: &str = r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow sysctl-read)
(allow file-read-metadata)
(allow file-read-data (literal "/"))
(allow file-map-executable)
(allow pseudo-tty)
(allow ipc-posix-shm-read-data)
(allow mach-lookup
  (global-name "com.apple.system.logger")
  (global-name "com.apple.system.notification_center")
  (global-name "com.apple.bsd.dirhelper")
  (global-name "com.apple.cfprefsd.daemon")
  (global-name "com.apple.cfprefsd.agent")
  (global-name "com.apple.SecurityServer")
  (global-name "com.apple.securityd.xpc")
  (global-name "com.apple.TrustEvaluationAgent"))
(allow network*)
(allow file-read*
  (subpath "/usr")
  (subpath "/bin")
  (subpath "/sbin")
  (subpath "/System")
  (subpath "/Library")
  (subpath "/private/etc")
  (subpath "/etc")
  (subpath "/opt/homebrew")
  (subpath "/Applications/Xcode.app/Contents/Developer")
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/urandom")
  (literal "/dev/random")
  (literal "/dev/tty")
  (literal "/dev/dtracehelper")
  (literal "/dev/fd")
  (subpath "/dev/fd"))
(allow file-write*
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/tty")
  (literal "/dev/dtracehelper"))
(allow file-ioctl
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/tty")
  (literal "/dev/dtracehelper"))
(allow file-read* file-write*
  (subpath (param "WORKSPACE"))
  (subpath (param "TMPDIR")))
(deny file-write*
  (subpath (param "GIT_DIR"))
  (subpath (param "KEEL_DIR"))
  (regex (param "GIT_REGEX"))
  (regex (param "KEEL_REGEX")))
"#;

    /// Plan-mode Seatbelt policy (SBPL) for macOS.
    ///
    /// Denies all network access and all file writes except the private
    /// `TMPDIR` (and standard null/tty devices), while keeping the workspace,
    /// `.git`, and `.keel` read-only so `git diff`, `git log`, `rg`, and
    /// inspection commands work without any possibility of mutation.
    const PLAN_PROFILE: &str = r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow sysctl-read)
(allow file-read-metadata)
(allow file-read-data (literal "/"))
(allow file-map-executable)
(allow pseudo-tty)
(allow ipc-posix-shm-read-data)
(allow mach-lookup
  (global-name "com.apple.system.logger")
  (global-name "com.apple.system.notification_center")
  (global-name "com.apple.bsd.dirhelper")
  (global-name "com.apple.cfprefsd.daemon")
  (global-name "com.apple.cfprefsd.agent")
  (global-name "com.apple.SecurityServer")
  (global-name "com.apple.securityd.xpc")
  (global-name "com.apple.TrustEvaluationAgent"))
(deny network*)
(allow file-read*
  (subpath "/usr")
  (subpath "/bin")
  (subpath "/sbin")
  (subpath "/System")
  (subpath "/Library")
  (subpath "/private/etc")
  (subpath "/etc")
  (subpath "/opt/homebrew")
  (subpath "/Applications/Xcode.app/Contents/Developer")
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/urandom")
  (literal "/dev/random")
  (literal "/dev/tty")
  (literal "/dev/dtracehelper")
  (literal "/dev/fd")
  (subpath "/dev/fd"))
(allow file-write*
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/tty")
  (literal "/dev/dtracehelper"))
(allow file-ioctl
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/tty")
  (literal "/dev/dtracehelper"))
(allow file-read*
  (subpath (param "WORKSPACE")))
(allow file-read* file-write*
  (subpath (param "TMPDIR")))
(deny file-write*
  (subpath (param "WORKSPACE"))
  (subpath (param "GIT_DIR"))
  (subpath (param "KEEL_DIR"))
  (regex (param "GIT_REGEX"))
  (regex (param "KEEL_REGEX")))
"#;

    const STATE_FILE_RULE: &str = "\n(allow file-write-data (literal (param \"STATE_FILE\")))\n";

    #[derive(Debug)]
    pub(super) struct SeatbeltConfig {
        workspace: PathBuf,
        temp_dir: PathBuf,
        state_file: Option<PathBuf>,
        plan_mode: bool,
    }

    impl SeatbeltConfig {
        pub(super) fn set_state_file(&mut self, path: &Path) {
            let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            self.state_file = Some(resolved);
        }

        pub(super) fn profile(&self) -> String {
            let mut profile = if self.plan_mode {
                PLAN_PROFILE.to_owned()
            } else {
                BASE_PROFILE.to_owned()
            };
            if self.state_file.is_some() {
                profile.push_str(STATE_FILE_RULE);
            }
            profile
        }

        pub(super) fn args(&self, program: &OsStr) -> Vec<OsString> {
            let git_dir = self.workspace.join(".git");
            let keel_dir = self.workspace.join(".keel");
            let git_regex = case_insensitive_subpath_regex(&self.workspace, ".git");
            let keel_regex = case_insensitive_subpath_regex(&self.workspace, ".keel");

            let mut args = vec![
                OsString::from("-p"),
                OsString::from(self.profile()),
                param("WORKSPACE", &self.workspace),
                param("TMPDIR", &self.temp_dir),
                param("GIT_DIR", &git_dir),
                param("KEEL_DIR", &keel_dir),
                param_str("GIT_REGEX", &git_regex),
                param_str("KEEL_REGEX", &keel_regex),
            ];
            if let Some(state_file) = &self.state_file {
                args.push(param("STATE_FILE", state_file));
            }
            args.push(OsString::from("--"));
            args.push(program.to_os_string());
            args
        }
    }

    fn param(name: &str, path: &Path) -> OsString {
        let mut arg = OsString::from(format!("-D{name}="));
        arg.push(path.as_os_str());
        arg
    }

    fn param_str(name: &str, value: &str) -> OsString {
        OsString::from(format!("-D{name}={value}"))
    }

    fn case_insensitive_subpath_regex(base: &Path, child: &str) -> String {
        let base_str = base.to_string_lossy();
        let trimmed = base_str.trim_end_matches('/');
        let full = if trimmed.is_empty() {
            format!("/{child}")
        } else {
            format!("{trimmed}/{child}")
        };
        let mut regex = String::with_capacity(full.len() * 4 + 8);
        regex.push('^');
        for ch in full.chars() {
            if ch.is_ascii_alphabetic() {
                regex.push('[');
                regex.push(ch.to_ascii_lowercase());
                regex.push(ch.to_ascii_uppercase());
                regex.push(']');
            } else if matches!(
                ch,
                '\\' | '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
            ) {
                regex.push('\\');
                regex.push(ch);
            } else {
                regex.push(ch);
            }
        }
        regex.push_str("(/|$)");
        regex
    }

    pub(super) fn prepare(workspace: &Path, plan_mode: bool) -> Result<Sandbox, ToolError> {
        if !Path::new(SANDBOX_EXEC).is_file() {
            return Err(ToolError::Message(format!(
                "Seatbelt sandbox tool `{SANDBOX_EXEC}` is not available on this system. \
                 Refusing to start the command"
            )));
        }

        let canonical_workspace = std::fs::canonicalize(workspace).map_err(|error| {
            ToolError::Message(format!(
                "could not resolve workspace `{}` for Seatbelt ({error}). \
                 Refusing to start the command",
                workspace.display()
            ))
        })?;

        let temp_dir = SandboxTempDir::create().map_err(|error| {
            ToolError::Message(format!(
                "could not create a private sandbox temp directory ({error}). \
                 Refusing to start the command"
            ))
        })?;
        let canonical_temp = std::fs::canonicalize(temp_dir.path())
            .unwrap_or_else(|_| temp_dir.path().to_path_buf());

        Ok(Sandbox {
            seatbelt: SeatbeltConfig {
                workspace: canonical_workspace,
                temp_dir: canonical_temp,
                state_file: None,
                plan_mode,
            },
            temp_dir: Some(temp_dir),
            plan_mode,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn seatbelt_profile_denies_default_and_protects_git_and_keel() {
            let workspace = tempfile::tempdir().unwrap();
            let mut sandbox = prepare(workspace.path(), false).unwrap();
            let profile = sandbox.seatbelt.profile();

            assert!(profile.starts_with("(version 1)\n(deny default)\n"));
            assert!(profile.contains("(allow network*)"));
            assert!(profile.contains("(subpath (param \"WORKSPACE\"))"));
            assert!(profile.contains("(subpath (param \"TMPDIR\"))"));
            assert!(profile.contains("(subpath (param \"GIT_DIR\"))"));
            assert!(profile.contains("(subpath (param \"KEEL_DIR\"))"));
            assert!(profile.contains("(regex (param \"GIT_REGEX\"))"));
            assert!(profile.contains("(regex (param \"KEEL_REGEX\"))"));
            assert!(profile.contains("(literal \"/dev/null\")"));
            assert!(profile.contains("(literal \"/dev/urandom\")"));
            assert!(!profile.contains("STATE_FILE"));

            let state = workspace.path().join("state");
            std::fs::write(&state, b"").unwrap();
            sandbox.allow_state_file(&state);
            assert!(sandbox.seatbelt.profile().contains("STATE_FILE"));
        }

        #[test]
        fn plan_seatbelt_profile_denies_network_and_workspace_writes_but_allows_tmpdir() {
            let workspace = tempfile::tempdir().unwrap();
            let mut sandbox = prepare(workspace.path(), true).unwrap();
            let profile = sandbox.seatbelt.profile();

            assert!(profile.starts_with("(version 1)\n(deny default)\n"));
            assert!(profile.contains("(deny network*)"));
            assert!(!profile.contains("(allow network*)"));
            assert!(profile.contains("(allow file-read*\n  (subpath (param \"WORKSPACE\")))"));
            assert!(
                profile.contains("(allow file-read* file-write*\n  (subpath (param \"TMPDIR\")))")
            );
            assert!(profile.contains(
                "(deny file-write*\n  (subpath (param \"WORKSPACE\"))\n  (subpath (param \"GIT_DIR\"))\n  (subpath (param \"KEEL_DIR\"))\n  (regex (param \"GIT_REGEX\"))\n  (regex (param \"KEEL_REGEX\")))"
            ));

            let state = workspace.path().join("state");
            std::fs::write(&state, b"").unwrap();
            sandbox.allow_state_file(&state);
            assert!(sandbox.seatbelt.profile().contains("STATE_FILE"));
        }

        #[test]
        fn seatbelt_args_pass_canonical_parameters_and_program() {
            let workspace = tempfile::tempdir().unwrap();
            let sandbox = prepare(workspace.path(), false).unwrap();
            let args = sandbox.seatbelt.args(OsStr::new("bash"));
            let rendered: Vec<String> = args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();

            let canonical_ws = std::fs::canonicalize(workspace.path()).unwrap();
            assert_eq!(rendered[0], "-p");
            assert!(
                rendered
                    .iter()
                    .any(|arg| arg == &format!("-DWORKSPACE={}", canonical_ws.display())),
                "{rendered:?}"
            );
            assert!(
                rendered
                    .iter()
                    .any(|arg| arg.starts_with("-DTMPDIR=") && arg.contains("keel-sandbox-tmp-")),
                "{rendered:?}"
            );
            assert!(
                rendered
                    .iter()
                    .any(|arg| arg == &format!("-DGIT_DIR={}/.git", canonical_ws.display())),
                "{rendered:?}"
            );
            assert!(
                rendered
                    .iter()
                    .any(|arg| arg == &format!("-DKEEL_DIR={}/.keel", canonical_ws.display())),
                "{rendered:?}"
            );
            assert!(
                rendered.iter().any(|arg| arg.starts_with("-DGIT_REGEX=^")),
                "{rendered:?}"
            );
            assert!(
                rendered.iter().any(|arg| arg.starts_with("-DKEEL_REGEX=^")),
                "{rendered:?}"
            );
            assert_eq!(rendered[rendered.len() - 2], "--");
            assert_eq!(rendered[rendered.len() - 1], "bash");
        }

        #[test]
        fn case_insensitive_subpath_regex_escapes_metacharacters() {
            let regex = case_insensitive_subpath_regex(Path::new("/tmp/a+b.c/"), ".git");
            assert_eq!(
                regex,
                r"^/[tT][mM][pP]/[aA]\+[bB]\.[cC]/\.[gG][iI][tT](/|$)"
            );
        }

        #[test]
        fn std_command_scrubs_seeded_credentials_and_sets_private_tmpdir() {
            let workspace = tempfile::tempdir().unwrap();
            let sandbox = prepare(workspace.path(), false).unwrap();
            let command = crate::sandbox::std_command(OsStr::new("bash"), Some(&sandbox));

            let envs: Vec<(String, Option<String>)> = command
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|val| val.to_string_lossy().into_owned()),
                    )
                })
                .collect();

            for key in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "KEEL_CREDENTIALS"] {
                assert!(
                    envs.iter().any(|(k, v)| k == key && v.is_none()),
                    "{key} was not explicitly removed from sandboxed command env: {envs:?}"
                );
            }
            assert!(
                envs.iter().any(|(k, v)| k == "TMPDIR"
                    && v.as_deref()
                        .is_some_and(|val| val.contains("keel-sandbox-tmp-"))),
                "TMPDIR was not pointed at private sandbox temp dir: {envs:?}"
            );
            assert!(
                !envs.iter().any(|(k, _)| k == "GIT_OPTIONAL_LOCKS"),
                "workspace sandbox should not override GIT_OPTIONAL_LOCKS: {envs:?}"
            );
        }

        #[test]
        fn plan_std_command_sets_home_git_env_and_scrubs_credentials() {
            let workspace = tempfile::tempdir().unwrap();
            let sandbox = prepare(workspace.path(), true).unwrap();
            let expected_tmp = sandbox.temp_dir_path().unwrap().display().to_string();
            let command = crate::sandbox::std_command(OsStr::new("bash"), Some(&sandbox));

            let envs: Vec<(String, Option<String>)> = command
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|val| val.to_string_lossy().into_owned()),
                    )
                })
                .collect();

            for key in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "KEEL_CREDENTIALS"] {
                assert!(
                    envs.iter().any(|(k, v)| k == key && v.is_none()),
                    "{key} was not explicitly removed from plan sandbox env: {envs:?}"
                );
            }
            for key in ["TMPDIR", "TEMP", "TMP", "HOME"] {
                assert!(
                    envs.iter()
                        .any(|(k, v)| k == key && v.as_deref() == Some(expected_tmp.as_str())),
                    "{key} was not set to private sandbox temp dir: {envs:?}"
                );
            }
            assert!(
                envs.iter()
                    .any(|(k, v)| k == "GIT_OPTIONAL_LOCKS" && v.as_deref() == Some("0")),
                "GIT_OPTIONAL_LOCKS=0 missing: {envs:?}"
            );
            assert!(
                envs.iter()
                    .any(|(k, v)| k == "GIT_PAGER" && v.as_deref() == Some("cat")),
                "GIT_PAGER=cat missing: {envs:?}"
            );
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::OpenOptions;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    use super::{READ_ONLY_DEVICES, READ_WRITE_DEVICES, Sandbox, SandboxTempDir};
    use crate::tool::ToolError;

    const ACCESS_FS_EXECUTE: u64 = 1 << 0;
    const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
    const ACCESS_FS_READ_FILE: u64 = 1 << 2;
    const ACCESS_FS_READ_DIR: u64 = 1 << 3;
    const ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
    const ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
    const ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
    const ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
    const ACCESS_FS_MAKE_REG: u64 = 1 << 8;
    const ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
    const ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
    const ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
    const ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
    const ACCESS_FS_REFER: u64 = 1 << 13;
    const ACCESS_FS_TRUNCATE: u64 = 1 << 14;
    const ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;

    const ACCESS_FS_READ_EXECUTE: u64 =
        ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR;

    /// Non-directory device access: read, write, and truncate (for `>/dev/null`).
    /// Directory-only flags must not be passed for non-directory fds.
    const ACCESS_FS_DEVICE_RW: u64 =
        ACCESS_FS_READ_FILE | ACCESS_FS_WRITE_FILE | ACCESS_FS_TRUNCATE;

    /// Landlock ABI v4+ network access flags (Linux 6.7+): handling these
    /// without adding port rules denies all TCP bind and connect calls.
    const ACCESS_NET_BIND_TCP: u64 = 1 << 0;
    const ACCESS_NET_CONNECT_TCP: u64 = 1 << 1;

    /// Landlock ABI v6 scope flags (Linux 6.12+): block abstract Unix domain
    /// socket connects and signals to processes outside the sandbox domain.
    const LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    const LANDLOCK_SCOPE_SIGNAL: u64 = 1 << 1;

    /// `landlock_create_ruleset` flag: return the ABI version instead of a ruleset.
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;

    /// Matches `struct landlock_ruleset_attr` through ABI v6.
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }

    const _: () = assert!(std::mem::size_of::<RulesetAttr>() == 24);

    /// Matches the packed `struct landlock_path_beneath_attr`.
    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    const _: () = assert!(std::mem::size_of::<PathBeneathAttr>() == 12);

    fn strict_sandbox_requested() -> bool {
        std::env::var("KEEL_SANDBOX_STRICT").is_ok_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
    }

    pub(super) fn userns_blocked_by_sysctl() -> bool {
        let apparmor =
            std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").ok();
        let clone = std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone").ok();
        let max_ns = std::fs::read_to_string("/proc/sys/user/max_user_namespaces").ok();
        userns_blocked_from_values(apparmor.as_deref(), clone.as_deref(), max_ns.as_deref())
    }

    pub(super) fn userns_blocked_from_values(
        apparmor_restrict: Option<&str>,
        unprivileged_clone: Option<&str>,
        max_user_namespaces: Option<&str>,
    ) -> bool {
        apparmor_restrict.is_some_and(|value| value.trim() == "1")
            || unprivileged_clone.is_some_and(|value| value.trim() == "0")
            || max_user_namespaces.is_some_and(|value| value.trim() == "0")
    }

    pub(super) fn prepare(
        workspace: &Path,
        read_only: &[&str],
        plan_mode: bool,
    ) -> Result<Sandbox, ToolError> {
        let strict = strict_sandbox_requested();
        if strict && !plan_mode && userns_blocked_by_sysctl() {
            return Err(ToolError::Message(
                "unprivileged user namespaces are blocked on this host, so `.git` and `.keel` cannot be mounted read-only; KEEL_SANDBOX_STRICT=1 refuses to start the command".to_owned(),
            ));
        }
        let readonly_trees = ReadonlyTrees::for_workspace(workspace).map_err(|error| {
            ToolError::Message(format!(
                "could not prepare read-only `.git` and `.keel` mounts ({error}). \
                 Refusing to start the command"
            ))
        })?;
        let abi = landlock_abi().map_err(unavailable)?;
        if plan_mode && abi < 4 {
            return Err(ToolError::Message(format!(
                "Landlock ABI v{abi} does not support network restriction \
                 (ABI v4 / Linux 6.7 or newer is required for plan-mode bash). \
                 Refusing to start the command"
            )));
        }
        let handled = handled_access(abi);
        let read_only_access = handled & ACCESS_FS_READ_EXECUTE;
        if read_only_access == 0 {
            return Err(ToolError::Message(
                "Landlock reported an ABI without filesystem read access. Refusing to start the command"
                    .to_owned(),
            ));
        }

        let ruleset = create_ruleset(abi, handled, plan_mode).map_err(|error| {
            ToolError::Message(format!(
                "could not create a Landlock ruleset ({error}). Refusing to start the command"
            ))
        })?;

        let workspace_access = if plan_mode { read_only_access } else { handled };
        add_path(&ruleset, workspace, workspace_access).map_err(|error| {
            ToolError::Message(format!(
                "could not add a Landlock rule for the workspace `{}` ({error}). Refusing to start the command",
                workspace.display()
            ))
        })?;

        let temp_dir = SandboxTempDir::create().map_err(|error| {
            ToolError::Message(format!(
                "could not create a private sandbox temp directory ({error}). Refusing to start the command"
            ))
        })?;
        add_path(&ruleset, temp_dir.path(), handled).map_err(|error| {
            ToolError::Message(format!(
                "could not add a Landlock rule for the sandbox temp directory `{}` ({error}). Refusing to start the command",
                temp_dir.path().display()
            ))
        })?;

        for prefix in read_only {
            add_optional_path(&ruleset, prefix, read_only_access, "read-only")?;
        }
        let device_rw = handled & ACCESS_FS_DEVICE_RW;
        for device in READ_WRITE_DEVICES {
            add_optional_path(&ruleset, device, device_rw, "device read-write")?;
        }
        let device_ro = handled & ACCESS_FS_READ_FILE;
        for device in READ_ONLY_DEVICES {
            add_optional_path(&ruleset, device, device_ro, "device read-only")?;
        }

        Ok(Sandbox {
            ruleset,
            readonly_trees,
            strict,
            temp_dir: Some(temp_dir),
            plan_mode,
        })
    }

    fn add_optional_path(
        ruleset: &OwnedFd,
        path: &str,
        access: u64,
        kind: &str,
    ) -> Result<(), ToolError> {
        match add_path(ruleset, Path::new(path), access) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ToolError::Message(format!(
                "could not add a {kind} Landlock rule for `{path}` ({error}). Refusing to start the command"
            ))),
        }
    }

    fn add_path(ruleset: &OwnedFd, path: &Path, access: u64) -> io::Result<()> {
        let file = open_rule_fd(path)?;
        add_rule(ruleset.as_raw_fd(), file.as_raw_fd(), access)
    }

    fn open_rule_fd(path: &Path) -> io::Result<std::fs::File> {
        let mut options = OpenOptions::new();
        options.read(true);
        options.custom_flags(libc::O_PATH | libc::O_CLOEXEC);
        options.open(path)
    }

    pub(super) fn landlock_abi() -> io::Result<i32> {
        let version = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        if version <= 0 {
            return Err(io::Error::last_os_error());
        }
        i32::try_from(version).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Landlock ABI version does not fit in an i32",
            )
        })
    }

    fn create_ruleset(abi: i32, handled: u64, plan_mode: bool) -> io::Result<OwnedFd> {
        let handled_net = if plan_mode && abi >= 4 {
            ACCESS_NET_BIND_TCP | ACCESS_NET_CONNECT_TCP
        } else {
            0
        };
        let (attr, size) = if abi >= 6 {
            (
                RulesetAttr {
                    handled_access_fs: handled,
                    handled_access_net: handled_net,
                    scoped: LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET | LANDLOCK_SCOPE_SIGNAL,
                },
                std::mem::size_of::<RulesetAttr>(),
            )
        } else if handled_net != 0 {
            (
                RulesetAttr {
                    handled_access_fs: handled,
                    handled_access_net: handled_net,
                    scoped: 0,
                },
                std::mem::size_of::<u64>() * 2,
            )
        } else {
            (
                RulesetAttr {
                    handled_access_fs: handled,
                    handled_access_net: 0,
                    scoped: 0,
                },
                std::mem::size_of::<u64>(),
            )
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                size,
                0u32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = i32::try_from(fd).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Landlock ruleset descriptor does not fit in an i32",
            )
        })?;
        // SAFETY: `landlock_create_ruleset` returned this open descriptor and
        // the caller takes exclusive ownership of it. The kernel marks it
        // close-on-exec.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn handled_access(abi: i32) -> u64 {
        let mut access = ACCESS_FS_EXECUTE
            | ACCESS_FS_WRITE_FILE
            | ACCESS_FS_READ_FILE
            | ACCESS_FS_READ_DIR
            | ACCESS_FS_REMOVE_DIR
            | ACCESS_FS_REMOVE_FILE
            | ACCESS_FS_MAKE_CHAR
            | ACCESS_FS_MAKE_DIR
            | ACCESS_FS_MAKE_REG
            | ACCESS_FS_MAKE_SOCK
            | ACCESS_FS_MAKE_FIFO
            | ACCESS_FS_MAKE_BLOCK
            | ACCESS_FS_MAKE_SYM;
        if abi >= 2 {
            access |= ACCESS_FS_REFER;
        }
        if abi >= 3 {
            access |= ACCESS_FS_TRUNCATE;
        }
        if abi >= 5 {
            access |= ACCESS_FS_IOCTL_DEV;
        }
        access
    }

    fn unavailable(error: io::Error) -> ToolError {
        let why = match error.raw_os_error() {
            Some(libc::ENOSYS) => {
                "this kernel does not implement Landlock (Linux 5.13 or newer is required)"
            }
            Some(libc::EOPNOTSUPP) => {
                "Landlock is disabled on this kernel (it is missing from the LSM list)"
            }
            Some(libc::EPERM) => "this process is not allowed to create a Landlock ruleset",
            _ => "the OS sandbox needs Linux 5.13 or newer with Landlock enabled",
        };
        ToolError::Message(format!(
            "Landlock is not available ({error}); {why}. Refusing to start the command"
        ))
    }

    pub(super) fn confine_current(
        ruleset_fd: RawFd,
        trees: ReadonlyTreesRaw,
        plan_mode: bool,
        strict: bool,
    ) -> io::Result<()> {
        // Mounts first. Landlock cannot revoke a write that the workspace
        // rule grants to `.git` and `.keel`, and a read-only bind in this
        // child's mount namespace can. Seccomp then freezes those mounts.
        protect_readonly_trees(trees, strict && !plan_mode)?;
        set_no_new_privs()?;
        if plan_mode {
            install_no_network_seccomp()?;
        }
        restrict_self(ruleset_fd)
    }

    /// Denies non-`AF_UNIX` `socket(2)` calls and `io_uring_setup(2)` in plan
    /// mode so IPv4/IPv6/UDP network access is blocked on all Linux 5.13+
    /// kernels, even below Landlock ABI v4.
    fn install_no_network_seccomp() -> io::Result<()> {
        const BPF_LD: u16 = 0x00;
        const BPF_W: u16 = 0x00;
        const BPF_ABS: u16 = 0x20;
        const BPF_JMP: u16 = 0x05;
        const BPF_JEQ: u16 = 0x10;
        const BPF_RET: u16 = 0x06;
        const DENY: u32 = libc::SECCOMP_RET_ERRNO | (libc::EACCES as u32);

        #[cfg(target_arch = "x86_64")]
        const AUDIT_ARCH: u32 = 0xc000_003e;
        #[cfg(target_arch = "aarch64")]
        const AUDIT_ARCH: u32 = 0xc000_00b7;
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        compile_error!("sandbox seccomp needs an audit architecture for this target");

        let stmt = |code: u16, k: u32| libc::sock_filter {
            code,
            jt: 0,
            jf: 0,
            k,
        };
        let jump = |code: u16, k: u32, jt: u8, jf: u8| libc::sock_filter { code, jt, jf, k };

        // seccomp_data: nr @ 0, arch @ 4, args[0] (socket domain) @ 16 (low32) & 20 (high32).
        let mut filter = [
            stmt(BPF_LD | BPF_W | BPF_ABS, 4),
            jump(BPF_JMP | BPF_JEQ, AUDIT_ARCH, 1, 0),
            stmt(BPF_RET, DENY),
            stmt(BPF_LD | BPF_W | BPF_ABS, 0),
            jump(BPF_JMP | BPF_JEQ, libc::SYS_io_uring_setup as u32, 5, 0),
            jump(BPF_JMP | BPF_JEQ, libc::SYS_socket as u32, 0, 5),
            stmt(BPF_LD | BPF_W | BPF_ABS, 20),
            jump(BPF_JMP | BPF_JEQ, 0, 0, 2),
            stmt(BPF_LD | BPF_W | BPF_ABS, 16),
            jump(BPF_JMP | BPF_JEQ, libc::AF_UNIX as u32, 1, 0),
            stmt(BPF_RET, DENY),
            stmt(BPF_RET, libc::SECCOMP_RET_ALLOW),
        ];
        let mut prog = libc::sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_mut_ptr(),
        };
        if unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &mut prog) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Addresses captured by `pre_exec`. `usize` rather than pointers so the
    /// closure is `Send`. The allocations live in the parent `Sandbox` until
    /// `spawn` returns.
    #[derive(Clone, Copy)]
    pub(super) struct ReadonlyTreesRaw {
        git: usize,
        keel: usize,
        uid_map: usize,
        uid_map_len: usize,
        gid_map: usize,
        gid_map_len: usize,
    }

    #[derive(Debug)]
    pub(super) struct ReadonlyTrees {
        git: std::ffi::CString,
        keel: std::ffi::CString,
        uid_map: Vec<u8>,
        gid_map: Vec<u8>,
    }

    impl ReadonlyTrees {
        fn for_workspace(workspace: &Path) -> io::Result<Self> {
            use std::os::unix::ffi::OsStrExt;

            let root = if workspace.is_absolute() {
                workspace.to_path_buf()
            } else {
                std::fs::canonicalize(workspace)?
            };
            let path = |name: &str| {
                std::ffi::CString::new(root.join(name).as_os_str().as_bytes()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "sandbox path contains a NUL byte",
                    )
                })
            };
            // SAFETY: `geteuid` and `getegid` only read the process credentials.
            let uid = unsafe { libc::geteuid() };
            let gid = unsafe { libc::getegid() };
            Ok(Self {
                git: path(".git")?,
                keel: path(".keel")?,
                uid_map: format!("{uid} {uid} 1\n").into_bytes(),
                gid_map: format!("{gid} {gid} 1\n").into_bytes(),
            })
        }

        pub(super) fn as_raw(&self) -> ReadonlyTreesRaw {
            ReadonlyTreesRaw {
                git: self.git.as_ptr() as usize,
                keel: self.keel.as_ptr() as usize,
                uid_map: self.uid_map.as_ptr() as usize,
                uid_map_len: self.uid_map.len(),
                gid_map: self.gid_map.as_ptr() as usize,
                gid_map_len: self.gid_map.len(),
            }
        }
    }

    /// Enter a user and mount namespace, bind-mount `.git` and `.keel`
    /// read-only when they exist, and deny the syscalls that could undo it.
    ///
    /// A missing path is left alone: there is no mountpoint to cover, and
    /// creating one would add a directory to the host workspace. Landlock
    /// still allows creating that name later. Existing trees, including a
    /// `.git` gitfile, are covered.
    fn protect_readonly_trees(trees: ReadonlyTreesRaw, strict: bool) -> io::Result<()> {
        // SAFETY: the parent `Sandbox` owns these bytes and outlives `spawn`.
        // Fork copied the address space; nothing in this child reallocates them.
        let uid_map =
            unsafe { std::slice::from_raw_parts(trees.uid_map as *const u8, trees.uid_map_len) };
        let gid_map =
            unsafe { std::slice::from_raw_parts(trees.gid_map as *const u8, trees.gid_map_len) };
        // A host that blocks unprivileged user namespaces (Ubuntu's AppArmor
        // sysctl, for example) cannot carve `.git` out. Landlock still applies;
        // refusing the command by default would turn `--sandbox` off entirely,
        // while `KEEL_SANDBOX_STRICT=1` fails closed instead.
        if !enter_private_mount_namespace(uid_map, gid_map)? {
            if strict {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            return Ok(());
        }
        ro_bind(trees.git as *const libc::c_char)?;
        ro_bind(trees.keel as *const libc::c_char)?;
        set_no_new_privs()?;
        install_mount_seccomp()?;
        drop_capabilities()
    }

    /// Returns `Ok(false)` when the host refuses a user namespace and this
    /// process has not entered one. Any error after `unshare` is fatal: the
    /// child is already in a half-configured namespace.
    fn enter_private_mount_namespace(uid_map: &[u8], gid_map: &[u8]) -> io::Result<bool> {
        if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
            let error = io::Error::last_os_error();
            return if namespace_unavailable(&error) {
                Ok(false)
            } else {
                Err(error)
            };
        }
        // An unprivileged process may write a single-line gid map only after
        // it promises not to call setgroups. Supplementary groups are then
        // unmapped; world-readable toolchain paths stay readable.
        write_proc(b"/proc/self/setgroups\0", b"deny")?;
        write_proc(b"/proc/self/uid_map\0", uid_map)?;
        write_proc(b"/proc/self/gid_map\0", gid_map)?;
        if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // Make the new namespace private before binding, or a shared `/`
        // would publish the read-only mount to the host, including keel.
        if unsafe {
            libc::mount(
                c"none".as_ptr(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(true)
    }

    fn namespace_unavailable(error: &io::Error) -> bool {
        matches!(
            error.raw_os_error(),
            Some(libc::EPERM | libc::EACCES | libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)
        )
    }

    fn write_proc(path: &[u8], data: &[u8]) -> io::Result<()> {
        let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_WRONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut wrote = 0;
        while wrote < data.len() {
            let n = unsafe { libc::write(fd, data[wrote..].as_ptr().cast(), data.len() - wrote) };
            if n < 0 {
                let error = io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(error);
            }
            if n == 0 {
                unsafe { libc::close(fd) };
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            wrote += n as usize;
        }
        if unsafe { libc::close(fd) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn ro_bind(path: *const libc::c_char) -> io::Result<()> {
        let mut info: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::lstat(path, &mut info) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok(());
            }
            return Err(error);
        }
        let directory = (info.st_mode & libc::S_IFMT) == libc::S_IFDIR;
        let mut flags = libc::MS_BIND;
        if directory {
            flags |= libc::MS_REC;
        }
        if unsafe { libc::mount(path, path, std::ptr::null(), flags, std::ptr::null()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe {
            libc::mount(
                std::ptr::null(),
                path,
                std::ptr::null(),
                flags | libc::MS_REMOUNT | libc::MS_RDONLY,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Deny mount-family syscalls. A later `clone3` into a new mount namespace
    /// copies these binds; without `mount` or `umount2` the child cannot make
    /// them writable again. A non-native syscall ABI is denied outright so an
    /// `int 0x80` mount cannot bypass the native numbers.
    fn install_mount_seccomp() -> io::Result<()> {
        const BPF_LD: u16 = 0x00;
        const BPF_W: u16 = 0x00;
        const BPF_ABS: u16 = 0x20;
        const BPF_JMP: u16 = 0x05;
        const BPF_JEQ: u16 = 0x10;
        const BPF_RET: u16 = 0x06;
        const DENY: u32 = libc::SECCOMP_RET_ERRNO | (libc::EPERM as u32);

        // `linux/audit.h`. libc does not re-export these.
        #[cfg(target_arch = "x86_64")]
        const AUDIT_ARCH: u32 = 0xc000_003e;
        #[cfg(target_arch = "aarch64")]
        const AUDIT_ARCH: u32 = 0xc000_00b7;
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        compile_error!("sandbox seccomp needs an audit architecture for this target");

        let blocked: [u32; 12] = [
            libc::SYS_mount as u32,
            libc::SYS_umount2 as u32,
            libc::SYS_pivot_root as u32,
            libc::SYS_move_mount as u32,
            libc::SYS_open_tree as u32,
            libc::SYS_fsopen as u32,
            libc::SYS_fsconfig as u32,
            libc::SYS_fsmount as u32,
            libc::SYS_fspick as u32,
            libc::SYS_mount_setattr as u32,
            libc::SYS_unshare as u32,
            libc::SYS_setns as u32,
        ];

        let mut filter = [libc::sock_filter {
            code: 0,
            jt: 0,
            jf: 0,
            k: 0,
        }; 40];
        let mut len = 0;
        let mut push = |item: libc::sock_filter| {
            filter[len] = item;
            len += 1;
        };
        let stmt = |code: u16, k: u32| libc::sock_filter {
            code,
            jt: 0,
            jf: 0,
            k,
        };
        let jump = |code: u16, k: u32, jt: u8, jf: u8| libc::sock_filter { code, jt, jf, k };

        // seccomp_data.arch is at byte 4; nr is at byte 0.
        push(stmt(BPF_LD | BPF_W | BPF_ABS, 4));
        push(jump(BPF_JMP | BPF_JEQ, AUDIT_ARCH, 1, 0));
        push(stmt(BPF_RET, DENY));
        push(stmt(BPF_LD | BPF_W | BPF_ABS, 0));
        for nr in blocked {
            // Equal: fall into RET deny. Otherwise skip that RET.
            push(jump(BPF_JMP | BPF_JEQ, nr, 0, 1));
            push(stmt(BPF_RET, DENY));
        }
        push(stmt(BPF_RET, libc::SECCOMP_RET_ALLOW));

        let mut prog = libc::sock_fprog {
            len: u16::try_from(len).map_err(|_| io::Error::from_raw_os_error(libc::E2BIG))?,
            filter: filter.as_mut_ptr(),
        };
        if unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &mut prog) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn drop_capabilities() -> io::Result<()> {
        // `linux/capability.h` version 3: two u32 words, covering every cap bit.
        #[repr(C)]
        struct CapHeader {
            version: u32,
            pid: i32,
        }
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct CapData {
            effective: u32,
            permitted: u32,
            inheritable: u32,
        }
        const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

        unsafe {
            let _ = libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_CLEAR_ALL,
                0,
                0,
                0,
            );
        }
        let header = CapHeader {
            version: CAPABILITY_VERSION_3,
            pid: 0,
        };
        let data = [CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        }; 2];
        // SAFETY: `header` and `data` match `capset`'s two user structs, and
        // `pid == 0` means the calling thread. This only clears our caps.
        let rc = unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn dup_state_fd(source: RawFd) -> io::Result<()> {
        if unsafe { libc::dup2(source, super::STATE_FD) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn add_rule(ruleset_fd: RawFd, parent_fd: RawFd, access: u64) -> io::Result<()> {
        let attr = PathBeneathAttr {
            allowed_access: access,
            parent_fd,
        };
        let rc = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset_fd,
                LANDLOCK_RULE_PATH_BENEATH,
                &attr as *const PathBeneathAttr,
                0u32,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn set_no_new_privs() -> io::Result<()> {
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn restrict_self(ruleset_fd: RawFd) -> io::Result<()> {
        let rc = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset_fd, 0u32) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn userns_blocked_from_values_detects_restricting_sysctls() {
            assert!(!userns_blocked_from_values(None, None, None));
            assert!(!userns_blocked_from_values(
                Some("0\n"),
                Some("1\n"),
                Some("1000\n")
            ));
            assert!(userns_blocked_from_values(Some("1\n"), None, None));
            assert!(userns_blocked_from_values(None, Some("0\n"), None));
            assert!(userns_blocked_from_values(None, None, Some("0\n")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubbed_env_vars_include_core_provider_keys() {
        for expected in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "NEURALWATT_API_KEY",
            "KEEL_CREDENTIALS",
        ] {
            assert!(
                SCRUBBED_ENV_VARS.contains(&expected),
                "missing {expected} from SCRUBBED_ENV_VARS"
            );
        }
    }
}
