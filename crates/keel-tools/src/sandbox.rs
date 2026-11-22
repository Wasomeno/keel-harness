//! Landlock confinement for `bash` and `jobs` children.
//!
//! The ruleset is built in this process. The child installs it after `fork`
//! and before `exec`, so the keel process is never restricted. Network is not
//! a handled access, so it stays open. A missing Landlock implementation is an
//! error when the sandbox flag is on; the command is not started unsandboxed.

#![cfg_attr(target_os = "linux", allow(unsafe_code))]

use std::path::Path;

use crate::tool::ToolError;

/// System directories a toolchain has to read and execute.
///
/// Everything else, including `/tmp`, `/home`, and `/proc`, is unreachable.
/// A rule on the shell's own `/proc/<pid>` does not cover the commands that
/// shell runs, and a private proc mount needs a pid namespace, which would
/// replace the process id `Command` tracks. `/proc` stays unreachable.
pub(crate) const READ_ONLY_PREFIXES: &[&str] = &["/usr", "/bin", "/lib", "/lib64", "/etc"];

/// File descriptor the sandboxed shell uses for the working-directory record.
///
/// The record lives in the temporary directory, which the ruleset does not
/// allow the child to open by path. The parent keeps the file open and the
/// child inherits this descriptor, so the write does not depend on `/tmp`.
#[cfg(target_os = "linux")]
pub(crate) const STATE_FD: i32 = 253;

/// A ruleset that has not been enforced yet.
///
/// Hold it until `spawn` returns. The child inherits the ruleset descriptor
/// at `fork`; dropping this value closes the parent's copy.
#[derive(Debug)]
pub(crate) struct Sandbox {
    #[cfg(target_os = "linux")]
    ruleset: std::os::fd::OwnedFd,
}

/// Builds a ruleset when `enabled` is set.
///
/// The command must not start when this returns an error: Landlock is missing
/// or a required path could not be added, and running unsandboxed would hide
/// that.
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (workspace, read_only);
        Err(ToolError::Message(
            "Landlock is not available on this operating system; the OS sandbox needs Linux 5.13 or newer with Landlock enabled. Refusing to start the command".to_owned(),
        ))
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

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::OpenOptions;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    use super::Sandbox;
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

    /// `landlock_create_ruleset` flag: return the ABI version instead of a ruleset.
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;

    /// Matches `struct landlock_ruleset_attr` for the filesystem field only.
    ///
    /// Passing an 8-byte attribute leaves network unhandled, so sockets stay open.
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }

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

        let ruleset = create_ruleset(handled).map_err(|error| {
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
        for prefix in read_only {
            match add_path(&ruleset, Path::new(prefix), read_only_access) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(ToolError::Message(format!(
                        "could not add a read-only Landlock rule for `{prefix}` ({error}). Refusing to start the command"
                    )));
                }
            }
        }

        Ok(Sandbox { ruleset })
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

    fn create_ruleset(handled: u64) -> io::Result<OwnedFd> {
        let attr = RulesetAttr {
            handled_access_fs: handled,
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
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
