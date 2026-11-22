//! OS confinement for `bash` and `jobs` children.
//!
//! - **Linux 5.13+**: Landlock ruleset built in the parent and installed in the
//!   child after `fork` and before `exec`, so the keel process is never
//!   restricted. On Linux 6.12+ (ABI v6), signal and abstract Unix socket
//!   scoping are enabled too.
//! - **macOS**: Seatbelt policy (`(deny default)` SBPL profile) enforced via
//!   `/usr/bin/sandbox-exec`, confining writes to the canonicalized workspace
//!   and private `TMPDIR` while keeping `.git` and `.keel` read-only.
//!
//! On both platforms, network stays open, sensitive provider credential env
//! vars are scrubbed from confined children, and each confined command gets a
//! private mode-`0700` writable `TMPDIR` while host `/tmp` and `/home` stay
//! unreachable. On unsupported platforms (or when Landlock / `sandbox-exec` is
//! unavailable), enabling the sandbox is an error; the command is never started
//! unsandboxed.

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
    "KEEL_ANTIGRAVITY_ACCESS_TOKEN",
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

/// A prepared OS sandbox paired with its private `TMPDIR`.
///
/// Hold it until `spawn` returns (and for foreground `bash`, until the command
/// finishes so `temp_dir` stays alive). Background `jobs` move `temp_dir` onto
/// the `Job` via [`Sandbox::take_temp_dir`].
#[derive(Debug)]
pub(crate) struct Sandbox {
    #[cfg(target_os = "linux")]
    ruleset: std::os::fd::OwnedFd,
    #[cfg(target_os = "macos")]
    seatbelt: macos::SeatbeltConfig,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    temp_dir: Option<SandboxTempDir>,
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

/// Builds a sandbox configuration when `enabled` is set.
///
/// The command must not start when this returns an error: the OS sandbox is
/// missing or a required path could not be configured, and running unsandboxed
/// would hide that.
pub(crate) fn for_child(
    tool: &str,
    enabled: bool,
    workspace: &Path,
) -> Result<Option<Sandbox>, ToolError> {
    if !enabled {
        return Ok(None);
    }
    prepare(workspace, READ_ONLY_PREFIXES)
        .map(Some)
        .map_err(|error| ToolError::Message(format!("{tool}: {error}")))
}

fn prepare(workspace: &Path, read_only: &[&str]) -> Result<Sandbox, ToolError> {
    #[cfg(target_os = "linux")]
    {
        linux::prepare(workspace, read_only)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = read_only;
        macos::prepare(workspace)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (workspace, read_only);
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

/// Scrubs sensitive credentials and points `TMPDIR`/`TEMP`/`TMP` at the
/// sandbox's private scratch directory on a tokio command.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn apply_env_tokio(command: &mut tokio::process::Command, sandbox: &Sandbox) {
    for var in SCRUBBED_ENV_VARS {
        command.env_remove(var);
    }
    if let Some(dir) = sandbox.temp_dir_path() {
        command.env("TMPDIR", dir).env("TEMP", dir).env("TMP", dir);
    }
}

/// Scrubs sensitive credentials and points `TMPDIR`/`TEMP`/`TMP` at the
/// sandbox's private scratch directory on a std command.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn apply_env_std(command: &mut std::process::Command, sandbox: &Sandbox) {
    for var in SCRUBBED_ENV_VARS {
        command.env_remove(var);
    }
    if let Some(dir) = sandbox.temp_dir_path() {
        command.env("TMPDIR", dir).env("TEMP", dir).env("TMP", dir);
    }
}

/// Installs `sandbox` on a tokio command, in the child, before `exec`.
#[cfg(target_os = "linux")]
pub(crate) fn confine_tokio(command: &mut tokio::process::Command, sandbox: &Sandbox) {
    let ruleset_fd = sandbox.ruleset.as_raw_fd();
    // SAFETY: `pre_exec` runs in the forked child before exec. The closure
    // only calls `prctl` and `landlock_restrict_self`, which are
    // async-signal-safe and do not allocate.
    unsafe {
        command.pre_exec(move || linux::confine_current(ruleset_fd));
    }
}

/// Installs `sandbox` on a std command, in the child, before `exec`.
#[cfg(target_os = "linux")]
pub(crate) fn confine_std(command: &mut std::process::Command, sandbox: &Sandbox) {
    use std::os::unix::process::CommandExt;

    let ruleset_fd = sandbox.ruleset.as_raw_fd();
    // SAFETY: same contract as [`confine_tokio`].
    unsafe {
        command.pre_exec(move || linux::confine_current(ruleset_fd));
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

    /// Base Seatbelt policy (SBPL) for macOS.
    ///
    /// Paths are supplied via `-D KEY=VALUE` parameters rather than string
    /// interpolation so arbitrary workspace characters cannot alter the Scheme
    /// syntax. Last-match-wins semantics make `.git` and `.keel` read-only
    /// even inside the writable workspace (preventing both direct edits and
    /// lockfile-rename updates to `.git/config` or hooks; note that Seatbelt
    /// subpath matching is case-sensitive on case-insensitive APFS volumes).
    const BASE_PROFILE: &str = r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow sysctl-read)
(allow file-read-metadata)
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
  (subpath (param "KEEL_DIR")))
"#;

    const STATE_FILE_RULE: &str = "\n(allow file-write-data (literal (param \"STATE_FILE\")))\n";

    #[derive(Debug)]
    pub(super) struct SeatbeltConfig {
        workspace: PathBuf,
        temp_dir: PathBuf,
        state_file: Option<PathBuf>,
    }

    impl SeatbeltConfig {
        pub(super) fn set_state_file(&mut self, path: &Path) {
            let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            self.state_file = Some(resolved);
        }

        pub(super) fn profile(&self) -> String {
            let mut profile = BASE_PROFILE.to_owned();
            if self.state_file.is_some() {
                profile.push_str(STATE_FILE_RULE);
            }
            profile
        }

        pub(super) fn args(&self, program: &OsStr) -> Vec<OsString> {
            let git_dir = self.workspace.join(".git");
            let keel_dir = self.workspace.join(".keel");

            let mut args = vec![
                OsString::from("-p"),
                OsString::from(self.profile()),
                param("WORKSPACE", &self.workspace),
                param("TMPDIR", &self.temp_dir),
                param("GIT_DIR", &git_dir),
                param("KEEL_DIR", &keel_dir),
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

    pub(super) fn prepare(workspace: &Path) -> Result<Sandbox, ToolError> {
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
            },
            temp_dir: Some(temp_dir),
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn seatbelt_profile_denies_default_and_protects_git_and_keel() {
            let workspace = tempfile::tempdir().unwrap();
            let mut sandbox = prepare(workspace.path()).unwrap();
            let profile = sandbox.seatbelt.profile();

            assert!(profile.starts_with("(version 1)\n(deny default)\n"));
            assert!(profile.contains("(subpath (param \"WORKSPACE\"))"));
            assert!(profile.contains("(subpath (param \"TMPDIR\"))"));
            assert!(profile.contains("(subpath (param \"GIT_DIR\"))"));
            assert!(profile.contains("(subpath (param \"KEEL_DIR\"))"));
            assert!(profile.contains("(literal \"/dev/null\")"));
            assert!(profile.contains("(literal \"/dev/urandom\")"));
            assert!(!profile.contains("STATE_FILE"));

            let state = workspace.path().join("state");
            std::fs::write(&state, b"").unwrap();
            sandbox.allow_state_file(&state);
            assert!(sandbox.seatbelt.profile().contains("STATE_FILE"));
        }

        #[test]
        fn seatbelt_args_pass_canonical_parameters_and_program() {
            let workspace = tempfile::tempdir().unwrap();
            let sandbox = prepare(workspace.path()).unwrap();
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
            assert_eq!(rendered[rendered.len() - 2], "--");
            assert_eq!(rendered[rendered.len() - 1], "bash");
        }

        #[test]
        fn std_command_scrubs_seeded_credentials_and_sets_private_tmpdir() {
            let workspace = tempfile::tempdir().unwrap();
            let sandbox = prepare(workspace.path()).unwrap();
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

    /// Landlock ABI v6 scope flags (Linux 6.12+): block abstract Unix domain
    /// socket connects and signals to processes outside the sandbox domain.
    const LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    const LANDLOCK_SCOPE_SIGNAL: u64 = 1 << 1;

    /// `landlock_create_ruleset` flag: return the ABI version instead of a ruleset.
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;

    /// Matches `struct landlock_ruleset_attr` through ABI v6.
    ///
    /// On kernels below ABI v6 we pass `size_of::<u64>()` (only
    /// `handled_access_fs`); on ABI v6+ we pass the full 24-byte struct with
    /// `handled_access_net = 0` (network stays unhandled) and `scoped` set.
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

    pub(super) fn prepare(workspace: &Path, read_only: &[&str]) -> Result<Sandbox, ToolError> {
        let abi = landlock_abi().map_err(unavailable)?;
        let handled = handled_access(abi);
        let read_only_access = handled & ACCESS_FS_READ_EXECUTE;
        if read_only_access == 0 {
            return Err(ToolError::Message(
                "Landlock reported an ABI without filesystem read access. Refusing to start the command"
                    .to_owned(),
            ));
        }

        let ruleset = create_ruleset(abi, handled).map_err(|error| {
            ToolError::Message(format!(
                "could not create a Landlock ruleset ({error}). Refusing to start the command"
            ))
        })?;

        add_path(&ruleset, workspace, handled).map_err(|error| {
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
            temp_dir: Some(temp_dir),
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

    fn landlock_abi() -> io::Result<i32> {
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

    fn create_ruleset(abi: i32, handled: u64) -> io::Result<OwnedFd> {
        let (attr, size) = if abi >= 6 {
            (
                RulesetAttr {
                    handled_access_fs: handled,
                    handled_access_net: 0,
                    scoped: LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET | LANDLOCK_SCOPE_SIGNAL,
                },
                std::mem::size_of::<RulesetAttr>(),
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

    pub(super) fn confine_current(ruleset_fd: RawFd) -> io::Result<()> {
        set_no_new_privs()?;
        restrict_self(ruleset_fd)
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
