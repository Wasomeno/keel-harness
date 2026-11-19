//! The fixture repository and per-run temp copies.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{EvalError, Result, Task};

pub fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn repo_dir() -> PathBuf {
    manifest_dir().join("fixtures/repo")
}

pub fn checks_dir() -> PathBuf {
    manifest_dir().join("fixtures/checks")
}

pub fn cassettes_dir() -> PathBuf {
    manifest_dir().join("fixtures/cassettes")
}

pub fn reports_dir() -> PathBuf {
    manifest_dir().join("reports")
}

/// A temp copy of the fixture with the task's check script at `tests/check.py`.
/// Dropping it removes the directory.
pub struct Fixture {
    directory: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    pub fn prepare(task: &Task) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        copy_dir(&repo_dir(), directory.path())?;

        let source = checks_dir().join(task.check_script);
        if !source.is_file() {
            return Err(EvalError::MissingCheck(task.check_script.to_owned()));
        }
        let tests = directory.path().join("tests");
        fs::create_dir_all(&tests)?;
        fs::copy(&source, tests.join("check.py"))?;

        // The workspace must be canonical before tools and path normalization
        // see it; on macOS `/var/...` is a symlink to `/private/var/...`.
        let root = fs::canonicalize(directory.path())?;
        Ok(Self { directory, root })
    }

    /// Canonical workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path the temp directory was created at, before canonicalization.
    /// Normalization replaces both spellings.
    pub fn raw_root(&self) -> &Path {
        self.directory.path()
    }
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_name() == "__pycache__" {
            continue;
        }
        let target = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
