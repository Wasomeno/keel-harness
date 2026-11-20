//! Workspace sandbox.
//!
//! On Unix, file tools open paths descriptor-relatively: every component is
//! opened with `openat` and `O_NOFOLLOW` from an open root directory
//! descriptor. A symlink swapped in after a lookup cannot redirect the open.
//! The canonicalizing resolvers remain for tools that hand a path to a child
//! process (`bash`, `glob`, `search`); those are policy-scoped rather than
//! OS-confined. On non-Unix, the canonicalizing fallback is used.

use std::ffi::OsString;
use std::fs::File;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::sync::Arc;

use crate::tool::ToolError;

/// Temp names are chosen from a counter; a leftover file from a killed
/// process can collide, so try a few names before giving up.
const TEMP_ATTEMPTS: usize = 8;

#[derive(Clone)]
pub struct Workspace {
    root: PathBuf,
    #[cfg(unix)]
    root_dir: Arc<OwnedFd>,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("root", &self.root)
            .finish()
    }
}

impl PartialEq for Workspace {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
    }
}

impl Eq for Workspace {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    Read,
    Edit,
    Write,
}

impl Workspace {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ToolError> {
        let requested = root.as_ref();
        let root = std::fs::canonicalize(requested).map_err(|error| {
            ToolError::Io(format!(
                "could not open workspace `{}`: {error}; pass an existing directory as the workspace root",
                requested.display()
            ))
        })?;
        if !root.is_dir() {
            return Err(ToolError::Message(format!(
                "workspace `{}` is not a directory; pass an existing directory as the workspace root",
                root.display()
            )));
        }
        #[cfg(unix)]
        let root_dir = Arc::new(open_root(&root)?);
        Ok(Self {
            root,
            #[cfg(unix)]
            root_dir,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn relative(&self, path: &Path) -> String {
        slash_path(path.strip_prefix(&self.root).unwrap_or(path))
    }

    /// The workspace-relative spelling of a requested path, for messages.
    pub fn display_path(&self, requested: &str) -> String {
        let requested = requested.trim();
        let path = Path::new(requested);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let normalized = normalize(&joined);
        let relative = normalized.strip_prefix(&self.root).unwrap_or(&normalized);
        if relative.as_os_str().is_empty() {
            ".".to_owned()
        } else {
            slash_path(relative)
        }
    }

    /// Open an existing file without following symlinks on Unix.
    pub fn open_existing(&self, requested: &str) -> Result<File, ToolError> {
        #[cfg(unix)]
        {
            self.open_relative(requested, OpenMode::Read)
        }
        #[cfg(not(unix))]
        {
            let path = self.resolve_existing(requested)?;
            if path.is_dir() {
                return Err(ToolError::Message(directory_message(
                    &self.display_path(requested),
                    OpenMode::Read,
                )));
            }
            File::open(&path).map_err(|error| {
                ToolError::Io(format!(
                    "could not open `{requested}`: {error}; check that it is a readable file"
                ))
            })
        }
    }

    /// Open an existing file read-only for `edit`, with editing-specific
    /// errors and without following symlinks on Unix.
    pub fn open_for_edit(&self, requested: &str) -> Result<File, ToolError> {
        #[cfg(unix)]
        {
            self.open_relative(requested, OpenMode::Edit)
        }
        #[cfg(not(unix))]
        {
            let path = self.resolve_existing(requested)?;
            if path.is_dir() {
                return Err(ToolError::Message(directory_message(
                    &self.display_path(requested),
                    OpenMode::Edit,
                )));
            }
            OpenOptions::new().read(true).open(&path).map_err(|error| {
                ToolError::Io(format!(
                    "could not open `{requested}` for editing: {error}; check that it is a \
                     regular file and that you can write it"
                ))
            })
        }
    }

    /// Create or replace a file, creating missing parent directories.
    ///
    /// On Unix the bytes go to a temporary file in the target's directory,
    /// are fsynced, and are renamed over the target, so a failed write leaves
    /// the old file intact. The target is never followed through a symlink,
    /// and an existing target keeps its ordinary permission bits. A replaced
    /// file gets a new inode, so hard links keep the old contents. The
    /// non-Unix fallback uses a temp file and `std::fs::rename`; it follows
    /// symlinks and does not preserve the mode.
    pub fn write_file(&self, requested: &str, content: &[u8]) -> Result<(), ToolError> {
        #[cfg(unix)]
        {
            self.replace_file(requested, content, OpenMode::Write)
        }
        #[cfg(not(unix))]
        {
            self.replace_file_fallback(requested, content, false)
        }
    }

    /// Replace an existing file's complete contents through the same atomic
    /// path as [`Self::write_file`], with editing-specific errors. The caller
    /// reads the file and computes the replacement first.
    pub fn edit_file(&self, requested: &str, content: &[u8]) -> Result<(), ToolError> {
        #[cfg(unix)]
        {
            self.replace_file(requested, content, OpenMode::Edit)
        }
        #[cfg(not(unix))]
        {
            self.replace_file_fallback(requested, content, true)
        }
    }

    /// Resolve a path that must already exist.
    ///
    /// This returns a path for tools that hand it to a child process. File
    /// tools use [`Self::open_existing`] instead, whose open is race-free on
    /// Unix.
    pub fn resolve_existing(&self, requested: &str) -> Result<PathBuf, ToolError> {
        let candidate = self.candidate(requested)?;
        let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ToolError::NotFound {
                    path: requested.to_owned(),
                    root: self.root.display().to_string(),
                }
            } else {
                ToolError::Io(format!(
                    "could not resolve `{requested}`: {error}; check the path and its permissions"
                ))
            }
        })?;
        self.check(&canonical, requested)?;
        Ok(canonical)
    }

    /// Resolve a path that may not exist yet.
    ///
    /// This is the non-Unix fallback and a test helper: it canonicalizes the
    /// nearest existing ancestor and then leaves the caller to create the file,
    /// so its check and create are separate syscalls. Unix file tools use
    /// [`Self::write_file`], whose `openat` walk is race-free; do not add a
    /// Unix caller that treats this method's result as safe to open.
    pub fn resolve_for_write(&self, requested: &str) -> Result<PathBuf, ToolError> {
        let candidate = self.candidate(requested)?;

        if candidate.exists() {
            let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
                ToolError::Io(format!(
                    "could not resolve `{requested}` before writing: {error}; \
                     check the path and its permissions"
                ))
            })?;
            self.check(&canonical, requested)?;
            return Ok(canonical);
        }

        let mut ancestor = candidate.parent();
        while let Some(path) = ancestor {
            if path.exists() {
                let canonical = std::fs::canonicalize(path).map_err(|error| {
                    ToolError::Io(format!(
                        "could not resolve `{}` before writing `{requested}`: {error}; \
                         check the directory and its permissions",
                        path.display()
                    ))
                })?;
                self.check(&canonical, requested)?;
                if !canonical.is_dir() {
                    let component = self.relative(&canonical);
                    return Err(ToolError::Message(not_a_directory_message(
                        &component,
                        requested,
                        OpenMode::Write,
                    )));
                }
                let suffix = candidate.strip_prefix(path).unwrap_or(Path::new(""));
                return Ok(normalize(&canonical.join(suffix)));
            }
            ancestor = path.parent();
        }

        Err(ToolError::OutsideWorkspace {
            path: requested.to_owned(),
            root: self.root.display().to_string(),
        })
    }

    /// Resolve a directory for a command's working directory.
    pub fn resolve_dir(&self, requested: Option<&str>) -> Result<PathBuf, ToolError> {
        match requested {
            Some(path) => {
                let resolved = self.resolve_existing(path)?;
                if !resolved.is_dir() {
                    return Err(ToolError::Message(format!(
                        "`{path}` is not a directory; pass a directory path inside the workspace"
                    )));
                }
                Ok(resolved)
            }
            None => Ok(self.root.clone()),
        }
    }

    fn candidate(&self, requested: &str) -> Result<PathBuf, ToolError> {
        let requested = requested.trim();
        if requested.is_empty() {
            return Err(ToolError::InvalidArguments(
                "`path` must not be empty; pass a workspace-relative path".to_owned(),
            ));
        }
        let path = Path::new(requested);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let normalized = normalize(&joined);
        // Reject a lexical escape before touching the filesystem; canonical
        // checks below still catch symlinked parents.
        self.check(&normalized, requested)?;
        Ok(normalized)
    }

    fn check(&self, canonical: &Path, requested: &str) -> Result<(), ToolError> {
        if canonical.starts_with(&self.root) {
            Ok(())
        } else {
            Err(ToolError::OutsideWorkspace {
                path: requested.to_owned(),
                root: self.root.display().to_string(),
            })
        }
    }

    #[cfg(unix)]
    fn components(&self, requested: &str) -> Result<Vec<std::ffi::OsString>, ToolError> {
        let requested = requested.trim();
        if requested.is_empty() {
            return Err(ToolError::InvalidArguments(
                "`path` must not be empty; pass a workspace-relative path".to_owned(),
            ));
        }
        let path = Path::new(requested);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let normalized = normalize(&joined);
        self.check(&normalized, requested)?;
        let relative =
            normalized
                .strip_prefix(&self.root)
                .map_err(|_| ToolError::OutsideWorkspace {
                    path: requested.to_owned(),
                    root: self.root.display().to_string(),
                })?;
        Ok(relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name.to_os_string()),
                _ => None,
            })
            .collect())
    }

    #[cfg(unix)]
    fn open_relative(&self, requested: &str, mode: OpenMode) -> Result<File, ToolError> {
        use rustix::fs::{Mode, OFlags, openat};

        let components = self.components(requested)?;
        if components.is_empty() {
            let root = self.root_dir.try_clone().map_err(|error| {
                ToolError::Io(format!(
                    "could not reopen the workspace root for `{requested}`: {error}"
                ))
            })?;
            return Ok(File::from(root));
        }

        let (directory, target) = self.open_parent(requested, &components, mode)?;
        // Both callers read; the mode selects only the error wording.
        let file = openat(
            &directory,
            &target,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| open_error(error, requested, requested, &self.root, mode))?;
        Ok(File::from(file))
    }

    /// Walk to the target's parent, creating directories for a write, and
    /// return the open parent descriptor plus the final component.
    #[cfg(unix)]
    fn open_parent(
        &self,
        requested: &str,
        components: &[OsString],
        mode: OpenMode,
    ) -> Result<(OwnedFd, OsString), ToolError> {
        use rustix::fs::{Mode, OFlags, mkdirat, openat};
        use rustix::io::Errno;

        let mut directory = self.root_dir.try_clone().map_err(|error| {
            ToolError::Io(format!(
                "could not reopen the workspace root for `{requested}`: {error}"
            ))
        })?;
        for (index, component) in components
            .iter()
            .enumerate()
            .take(components.len().saturating_sub(1))
        {
            if mode == OpenMode::Write {
                match mkdirat(&directory, component, Mode::from_bits_truncate(0o777)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => {
                        let parent = components[..=index]
                            .iter()
                            .map(|part| part.to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("/");
                        return Err(ToolError::Io(format!(
                            "could not create directory `{parent}` for `{requested}`: {}; \
                             check permissions, or write to a path whose parent already exists",
                            io_error(error)
                        )));
                    }
                }
            }
            directory = openat(
                &directory,
                component,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| {
                let component_path = components[..=index]
                    .iter()
                    .map(|part| part.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                // O_NOFOLLOW with O_DIRECTORY reports ENOTDIR on some Unixes
                // instead of ELOOP, so check the component before choosing the
                // error text. The open has already failed, so this second
                // lookup cannot race a successful open into existence.
                if error == Errno::NOTDIR && is_symlink_at(&directory, component) {
                    return open_error(Errno::LOOP, requested, &component_path, &self.root, mode);
                }
                if matches!(error, Errno::LOOP | Errno::MLINK | Errno::NOTDIR) {
                    return open_error(error, requested, &component_path, &self.root, mode);
                }
                if mode == OpenMode::Write {
                    return ToolError::Io(format!(
                        "could not create directory `{component_path}` for `{requested}`: {}; \
                         check permissions, or write to a path whose parent already exists",
                        io_error(error)
                    ));
                }
                open_error(error, requested, &component_path, &self.root, mode)
            })?;
        }
        let target = components
            .last()
            .cloned()
            .expect("the parent walk needs a final component");
        Ok((directory, target))
    }

    /// Write through a temporary file in the target's directory, fsync it,
    /// rename it over the target, then fsync the directory.
    #[cfg(unix)]
    fn replace_file(
        &self,
        requested: &str,
        content: &[u8],
        mode: OpenMode,
    ) -> Result<(), ToolError> {
        use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, fsync, openat};
        use rustix::io::Errno;

        let editing = mode == OpenMode::Edit;
        let components = self.components(requested)?;
        if components.is_empty() {
            return Err(ToolError::Message(format!(
                "`{}` is the workspace root, not a file; pass a file path",
                requested.trim()
            )));
        }
        let (directory, target) = self.open_parent(requested, &components, mode)?;

        // Refuse a target this process cannot write before creating anything.
        // O_NOFOLLOW keeps a symlink at the final component a refusal instead
        // of a redirect, O_NONBLOCK keeps a FIFO from waiting for a reader,
        // and O_WRONLY without O_TRUNC changes nothing.
        let existing_mode = match openat(
            &directory,
            &target,
            OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(file) => {
                let stat = fstat(&file)
                    .map_err(|error| write_failure(editing, requested, io_error(error)))?;
                if !FileType::from_raw_mode(stat.st_mode).is_file() {
                    return Err(not_a_regular_file(requested, editing));
                }
                // Preserve ordinary mode bits only; restoring setuid/setgid
                // would let a replacement gain privileges the in-place write
                // used to clear.
                Some(Mode::from_bits_truncate(stat.st_mode & 0o777))
            }
            Err(Errno::NOENT) if mode == OpenMode::Write => None,
            // A FIFO or socket with no reader fails the non-blocking open
            // before it can be fstat'ed; it is still a non-regular target.
            // Linux reports ENXIO for a socket and macOS EOPNOTSUPP.
            Err(Errno::NXIO) | Err(Errno::OPNOTSUPP) => {
                return Err(not_a_regular_file(requested, editing));
            }
            Err(error) => return Err(open_error(error, requested, requested, &self.root, mode)),
        };

        let mut created = None;
        for _ in 0..TEMP_ATTEMPTS {
            let name = temp_name();
            match openat(
                &directory,
                &name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_bits_truncate(0o666),
            ) {
                Ok(file) => {
                    created = Some((name, File::from(file)));
                    break;
                }
                Err(Errno::EXIST) => continue,
                Err(error) => {
                    return Err(write_failure(editing, requested, io_error(error)));
                }
            }
        }
        let Some((name, mut temp)) = created else {
            return Err(write_failure(
                editing,
                requested,
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "could not choose a unique temporary file name",
                ),
            ));
        };
        let mut pending = PendingTemp {
            directory: &directory,
            name,
            armed: true,
        };

        // Match the target's ordinary mode immediately so the temp is not
        // more readable than the file it will replace. The open descriptor
        // stays writable even when the new mode is write-only.
        if let Some(existing) = existing_mode {
            fchmod(&temp, existing)
                .map_err(|error| write_failure(editing, requested, io_error(error)))?;
        }
        temp.write_all(content)
            .map_err(|error| write_failure(editing, requested, error))?;
        temp.sync_all()
            .map_err(|error| write_failure(editing, requested, error))?;
        drop(temp);
        commit_temp(&directory, &pending.name, &target)
            .map_err(|error| write_failure(editing, requested, io_error(error)))?;
        pending.armed = false;
        // The rename is already visible; a directory sync failure means the
        // entry may not survive a power loss, not that the write failed.
        let _ = fsync(&directory);
        Ok(())
    }
}

#[cfg(unix)]
fn open_root(root: &Path) -> Result<OwnedFd, ToolError> {
    use rustix::fs::{Mode, OFlags};

    rustix::fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| {
        ToolError::Io(format!(
            "could not open workspace `{}` as a directory: {}; pass an existing directory",
            root.display(),
            io_error(error)
        ))
    })
}

#[cfg(unix)]
fn is_symlink_at(directory: &OwnedFd, component: &std::ffi::OsStr) -> bool {
    use rustix::fs::{AtFlags, FileType, statat};

    statat(directory, component, AtFlags::SYMLINK_NOFOLLOW)
        .map(|stat| FileType::from_raw_mode(stat.st_mode).is_symlink())
        .unwrap_or(false)
}

#[cfg(unix)]
fn open_error(
    errno: rustix::io::Errno,
    requested: &str,
    component: &str,
    root: &Path,
    mode: OpenMode,
) -> ToolError {
    use rustix::io::Errno;

    match errno {
        Errno::NOENT => ToolError::NotFound {
            path: requested.to_owned(),
            root: root.display().to_string(),
        },
        Errno::LOOP | Errno::MLINK => {
            ToolError::Message(symlink_message(component, requested, mode))
        }
        Errno::ISDIR => ToolError::Message(directory_message(requested, mode)),
        Errno::NOTDIR => ToolError::Message(not_a_directory_message(component, requested, mode)),
        _ => ToolError::Io(match mode {
            OpenMode::Read => format!(
                "could not open `{requested}`: {}; check that the path is inside the workspace \
                 and that you can read it",
                io_error(errno)
            ),
            OpenMode::Edit => format!(
                "could not open `{requested}` for editing: {}; check that it is a regular file \
                 and that you can write it",
                io_error(errno)
            ),
            OpenMode::Write => format!(
                "could not write `{requested}`: {}; check permissions and disk space, or choose \
                 another path in the workspace",
                io_error(errno)
            ),
        }),
    }
}

fn directory_message(requested: &str, mode: OpenMode) -> String {
    match mode {
        OpenMode::Read => format!(
            "`{requested}` is a directory; use `glob` to list the files in it, then read one"
        ),
        OpenMode::Edit => format!("`{requested}` is a directory; choose a file to edit"),
        OpenMode::Write => format!("`{requested}` is a directory; choose a file path"),
    }
}

fn not_a_directory_message(component: &str, requested: &str, mode: OpenMode) -> String {
    let lead = if component == requested {
        format!("`{requested}` is not a directory")
    } else {
        format!("`{component}` is not a directory while resolving `{requested}`")
    };
    let next = match mode {
        OpenMode::Read => format!(
            "use `glob` to find the right path, or read `{component}` if that is the file you meant"
        ),
        OpenMode::Edit => {
            format!("choose a file to edit under a directory, or edit `{component}` instead")
        }
        OpenMode::Write => {
            format!("use `write` on `{component}` to replace that file, or choose another path")
        }
    };
    format!("{lead}; {next}")
}

#[cfg(unix)]
fn symlink_message(component: &str, requested: &str, mode: OpenMode) -> String {
    let location = if component == requested {
        format!("`{component}` is a symbolic link")
    } else {
        format!("`{component}` is a symbolic link while resolving `{requested}`")
    };
    let path = shell_quote(requested);
    let next = match mode {
        OpenMode::Read => format!("use `bash cat {path}` to read it"),
        OpenMode::Edit => {
            format!("use `bash cat {path}` to inspect it, then edit the link target directly")
        }
        OpenMode::Write => format!(
            "use `bash cat {path}` to inspect it, then write the link target directly or \
             replace the link with a regular file"
        ),
    };
    format!(
        "{location}; symlinks are not followed inside the workspace. \
         If you trust the target, {next}"
    )
}

#[cfg(unix)]
fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn io_error(error: rustix::io::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(error.raw_os_error())
}

/// Rename the temp entry over the target. `renameat` never follows a symlink
/// at either name, so a swap after the writability check replaces the link
/// entry rather than writing through it.
#[cfg(unix)]
fn commit_temp(
    directory: &OwnedFd,
    temp: &std::ffi::OsStr,
    target: &std::ffi::OsStr,
) -> rustix::io::Result<()> {
    rustix::fs::renameat(directory, temp, directory, target)
}

/// A unique name in the target's directory. The `.keel-` prefix makes a
/// leaked file recognizable in a directory listing.
fn temp_name() -> OsString {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    OsString::from(format!(".keel-{}-{sequence}.tmp", std::process::id()))
}

fn not_a_regular_file(requested: &str, editing: bool) -> ToolError {
    if editing {
        ToolError::Message(format!(
            "`{requested}` is not a regular file; choose a file to edit"
        ))
    } else {
        ToolError::Message(format!(
            "`{requested}` is not a regular file; choose a file path"
        ))
    }
}

fn write_failure(editing: bool, requested: &str, error: impl std::fmt::Display) -> ToolError {
    if editing {
        ToolError::Io(format!(
            "could not write `{requested}` after editing: {error}; check permissions and disk \
             space, then read the file and retry, or choose another file"
        ))
    } else {
        #[cfg(unix)]
        let next = "check permissions and disk space, or choose another path in the workspace";
        #[cfg(not(unix))]
        let next = "check permissions and disk space";
        ToolError::Io(format!("could not write `{requested}`: {error}; {next}"))
    }
}

/// Remove a temporary file unless the rename committed it.
#[cfg(unix)]
struct PendingTemp<'a> {
    directory: &'a OwnedFd,
    name: OsString,
    armed: bool,
}

#[cfg(unix)]
impl Drop for PendingTemp<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = rustix::fs::unlinkat(self.directory, &self.name, rustix::fs::AtFlags::empty());
        }
    }
}

/// Remove a temporary file unless the rename committed it.
#[cfg(not(unix))]
struct PendingPath {
    path: PathBuf,
    armed: bool,
}

#[cfg(not(unix))]
impl Drop for PendingPath {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(not(unix))]
impl Workspace {
    /// Non-Unix fallback: a temp file in the target's directory and a
    /// standard-library rename. Symlinks are already followed by
    /// [`Self::resolve_for_write`] here.
    fn replace_file_fallback(
        &self,
        requested: &str,
        content: &[u8],
        editing: bool,
    ) -> Result<(), ToolError> {
        let path = self.resolve_for_write(requested)?;
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return Err(ToolError::Io(format!(
                "could not create the parent directory for `{requested}`: {error}; \
                 check permissions, or write to a path whose parent already exists"
            )));
        }
        if path.is_dir() {
            return Err(ToolError::Message(directory_message(
                &self.display_path(requested),
                OpenMode::Write,
            )));
        }
        if path.exists() && !path.is_file() {
            return Err(not_a_regular_file(requested, editing));
        }
        match std::fs::OpenOptions::new().write(true).open(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !editing => {}
            Err(error) if editing => {
                return Err(ToolError::Io(format!(
                    "could not open `{requested}` for editing: {error}; check that it is a \
                     regular file and that you can write it"
                )));
            }
            Err(error) => {
                return Err(ToolError::Io(format!(
                    "could not write `{requested}`: {error}; check permissions and disk space"
                )));
            }
        }

        let mut created = None;
        for _ in 0..TEMP_ATTEMPTS {
            let temp = path.with_file_name(temp_name());
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
            {
                Ok(file) => {
                    created = Some((temp, file));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(write_failure(editing, requested, error)),
            }
        }
        let Some((temp, mut file)) = created else {
            return Err(write_failure(
                editing,
                requested,
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "could not choose a unique temporary file name",
                ),
            ));
        };
        let mut pending = PendingPath {
            path: temp,
            armed: true,
        };
        file.write_all(content)
            .map_err(|error| write_failure(editing, requested, error))?;
        file.sync_all()
            .map_err(|error| write_failure(editing, requested, error))?;
        drop(file);
        std::fs::rename(&pending.path, &path)
            .map_err(|error| write_failure(editing, requested, error))?;
        pending.armed = false;
        Ok(())
    }
}

/// Render a workspace-relative path with `/` separators on every platform.
/// Tool output and error text use this spelling so the same transcript means
/// the same thing on Windows and Unix.
pub(crate) fn slash_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    {
        text.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        text.into_owned()
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_escapes() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let escape = format!("{}etc/passwd", "../".repeat(40));
        assert!(matches!(
            workspace.resolve_existing(&escape),
            Err(ToolError::OutsideWorkspace { .. })
        ));
    }

    #[test]
    fn rejects_absolute_paths_outside_the_root() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        assert!(matches!(
            workspace.resolve_existing("/etc/passwd"),
            Err(ToolError::OutsideWorkspace { .. })
        ));
    }

    #[test]
    fn accepts_files_inside_the_root() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("a.txt"), "hello").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let resolved = workspace.resolve_existing("a.txt").unwrap();
        assert_eq!(
            resolved,
            std::fs::canonicalize(directory.path())
                .unwrap()
                .join("a.txt")
        );
    }

    #[test]
    fn resolves_a_file_that_does_not_exist_yet() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let resolved = workspace.resolve_for_write("nested/new.txt").unwrap();
        assert!(resolved.ends_with("nested/new.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_root_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join("link")).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        assert!(matches!(
            workspace.resolve_existing("link/secret.txt"),
            Err(ToolError::OutsideWorkspace { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn open_existing_refuses_an_internal_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("target.txt"), "target").unwrap();
        symlink("target.txt", directory.path().join("link.txt")).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let error = workspace.open_existing("link.txt").unwrap_err();
        assert!(
            error.to_string().contains("symbolic link"),
            "unexpected error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_existing_names_an_ancestor_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("a")).unwrap();
        symlink(outside.path(), directory.path().join("a/b")).unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();

        let error = workspace.open_existing("a/b/file").unwrap_err().to_string();
        assert!(
            error.contains("`a/b` is a symbolic link while resolving `a/b/file`"),
            "unexpected error: {error}"
        );
        assert!(
            error.contains("use `bash cat 'a/b/file'`"),
            "unexpected error: {error}"
        );
        assert!(
            !error.contains("`a/b/file` is a symbolic link"),
            "unexpected error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn not_a_directory_messages_are_mode_specific() {
        let read = not_a_directory_message("notes", "notes/file", OpenMode::Read);
        assert!(read.contains("`notes` is not a directory while resolving `notes/file`"));
        assert!(read.contains("use `glob`"));
        assert!(read.contains("read `notes`"));

        let edit = not_a_directory_message("notes", "notes/file", OpenMode::Edit);
        assert!(edit.contains("edit `notes`"));

        let write = not_a_directory_message("notes", "notes/file", OpenMode::Write);
        assert!(write.contains("use `write` on `notes`"));
    }

    #[cfg(unix)]
    #[test]
    fn write_file_creates_parents() {
        use std::io::Read;

        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        workspace.write_file("nested/new.txt", b"written").unwrap();

        let mut text = String::new();
        File::open(directory.path().join("nested/new.txt"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "written");
    }

    #[cfg(unix)]
    #[test]
    fn open_survives_a_concurrent_symlink_swap() {
        use std::io::Read;
        use std::os::unix::fs::symlink;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/file"), "inside").unwrap();
        std::fs::write(outside.path().join("file"), "secret").unwrap();
        let workspace = Workspace::open(root.path()).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let swapper_stop = stop.clone();
        let root_path = root.path().to_path_buf();
        let outside_path = outside.path().to_path_buf();
        let swapper = std::thread::spawn(move || {
            while !swapper_stop.load(Ordering::Relaxed) {
                let dir = root_path.join("dir");
                let parked = root_path.join("dir.real");
                let _ = std::fs::rename(&dir, &parked);
                let _ = symlink(&outside_path, &dir);
                let _ = std::fs::remove_file(&dir);
                let _ = std::fs::rename(&parked, &dir);
            }
        });

        // Smoke test, not a regression test for the old resolver: the real
        // window is sub-microsecond, so this loop rarely wins the race. It does
        // pin the property that a successful open is the inside file, and the
        // deterministic internal-symlink test above is the regression gate.
        for _ in 0..2_000 {
            std::thread::yield_now();
            if let Ok(mut file) = workspace.open_existing("dir/file") {
                let mut text = String::new();
                if file.read_to_string(&mut text).is_ok() {
                    assert_ne!(text, "secret", "a swapped symlink escaped the workspace");
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        swapper.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_swapped_in_before_the_rename_is_replaced_not_followed() {
        use rustix::fs::{Mode, OFlags};
        use std::ffi::OsStr;
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "outside").unwrap();
        // The real write path creates and fills its temp file first, then
        // renames it over the final component. This is the state a concurrent
        // swap leaves between the writability check and that rename.
        std::fs::write(root.path().join(".keel-swap.tmp"), "inside").unwrap();
        symlink(
            outside.path().join("secret.txt"),
            root.path().join("file.txt"),
        )
        .unwrap();

        let directory = rustix::fs::open(
            root.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        commit_temp(
            &directory,
            OsStr::new(".keel-swap.tmp"),
            OsStr::new("file.txt"),
        )
        .unwrap();

        let file = root.path().join("file.txt");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "inside");
        assert!(!std::fs::symlink_metadata(&file).unwrap().is_symlink());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
            "outside"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_uncommitted_temp_entry_is_removed_by_the_drop_guard() {
        use rustix::fs::{Mode, OFlags};

        let root = tempfile::tempdir().unwrap();
        let directory = rustix::fs::open(
            root.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let name = OsString::from(".keel-drop.tmp");
        std::fs::write(root.path().join(&name), "partial").unwrap();

        let pending = PendingTemp {
            directory: &directory,
            name: name.clone(),
            armed: true,
        };
        drop(pending);
        assert!(
            !root.path().join(&name).exists(),
            "an uncommitted temp entry survived the drop guard"
        );

        // A disarmed guard leaves the committed entry alone.
        std::fs::write(root.path().join(&name), "committed").unwrap();
        let mut pending = PendingTemp {
            directory: &directory,
            name: name.clone(),
            armed: true,
        };
        pending.armed = false;
        drop(pending);
        assert!(root.path().join(&name).exists());
    }
}
