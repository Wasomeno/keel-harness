//! Workspace sandbox.
//!
//! This is a logical sandbox, not an OS sandbox: paths are normalized and
//! canonicalized, and every resolved path must stay under the workspace root.
//! Symlinks that leave the root are rejected because canonicalization resolves
//! them. A TOCTOU window remains between resolution and the filesystem call;
//! closing it is the job of an OS-level sandbox later.

use std::path::{Component, Path, PathBuf};

use crate::tool::ToolError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    root: PathBuf,
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
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    /// Resolve a path that must already exist.
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

    /// Resolve a path that may not exist yet. The nearest existing ancestor is
    /// canonicalized and checked, so a missing file cannot be used to escape
    /// through a symlinked parent.
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
}
