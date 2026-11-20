//! Persistent, account-keyed credentials.
//!
//! The store is one JSON file:
//!
//! ```json
//! { "version": 1, "credentials": { "openai": "..." } }
//! ```
//!
//! Keys are account labels chosen by the caller (`openai`, `anthropic.work`).
//! Values are secrets. The file is replaced atomically and, on Unix, written
//! with mode 0600 inside a 0700 directory. Neither [`CredentialStore`] nor
//! [`StoreResolver`] puts a value in its `Debug` output, and
//! [`CredentialError`] never carries one.
//!
//! Resolution order is environment first, then the store. [`StoreResolver`]
//! keeps an in-memory snapshot of the file; a miss reloads it once, and
//! concurrent callers share that reload. There is no time-based expiry: OAuth
//! refresh is M11, so an already-cached key stays cached until [`StoreResolver::reload`]
//! is called or the process ends.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind as IoErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde::{Deserialize, Serialize};

use keel_llm_types::{ErrorKind, LlmError};

use crate::auth::{CredentialRef, EnvResolver, SecretResolver};

/// Full path to the credential file. Overrides the platform default.
pub const CREDENTIALS_ENV: &str = "KEEL_CREDENTIALS";

/// Bumped when the file shape changes. A file from another version is refused
/// rather than half-read.
pub const CREDENTIALS_VERSION: u32 = 1;

const MAX_KEY_LENGTH: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: the credential file is not valid JSON (line {line}, column {column})")]
    Json {
        path: PathBuf,
        line: usize,
        column: usize,
    },
    #[error("{path}: the credential file has an unreadable shape (line {line}, column {column})")]
    Shape {
        path: PathBuf,
        line: usize,
        column: usize,
    },
    #[error("{path}: the credential store must not be a symbolic link")]
    Symlink { path: PathBuf },
    #[error("{path}: the credential path is a {kind}, not a regular file")]
    NotAFile { path: PathBuf, kind: &'static str },
    #[error("{path}: could not create a temporary credential file after 8 attempts")]
    TempExhausted { path: PathBuf },
    #[error(
        "{path}: credential file version {version} is not supported; this keel reads version {expected}"
    )]
    Version {
        path: PathBuf,
        version: u32,
        expected: u32,
    },
    #[error("the credential store path is unknown; set {CREDENTIALS_ENV}")]
    NoPath,
    #[error("invalid credential key `{key}`: {reason}")]
    InvalidKey { key: String, reason: &'static str },
    #[error("credential `{key}` needs a non-empty secret")]
    EmptySecret { key: String },
    #[error("could not encode the credential file")]
    Encode,
}

#[derive(Deserialize, Serialize)]
struct CredentialFile {
    version: u32,
    #[serde(default)]
    credentials: BTreeMap<String, String>,
}

/// The file holding stored credentials.
#[derive(Clone, Debug)]
pub struct CredentialStore {
    path: PathBuf,
    /// The platform default directory belongs to keel, so an existing real one
    /// is tightened to 0700. An explicit `KEEL_CREDENTIALS` parent is left
    /// alone, and a symlinked parent is followed but never chmodded. Only the
    /// Unix path reads it; Windows inherits the per-user `%APPDATA%` ACL.
    #[cfg(unix)]
    private_parent: bool,
}

impl CredentialStore {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            #[cfg(unix)]
            private_parent: false,
        }
    }

    /// `KEEL_CREDENTIALS` when set and non-empty, otherwise the platform config
    /// path.
    pub fn discover() -> Result<Self, CredentialError> {
        Self::from_override(env::var_os(CREDENTIALS_ENV))
    }

    fn from_override(override_path: Option<OsString>) -> Result<Self, CredentialError> {
        if let Some(path) = override_path.filter(|value| !value.is_empty()) {
            return Ok(Self::at(PathBuf::from(path)));
        }
        let path = default_path().ok_or(CredentialError::NoPath)?;
        Ok(Self {
            path,
            #[cfg(unix)]
            private_parent: true,
        })
    }

    /// Check a key before prompting for its secret, so a typo costs no prompt.
    pub fn validate_key(key: &str) -> Result<(), CredentialError> {
        validate_key(key)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every stored key, sorted.
    pub fn list(&self) -> Result<Vec<String>, CredentialError> {
        Ok(self.read_all()?.into_keys().collect())
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, CredentialError> {
        Ok(self.read_all()?.get(key).cloned())
    }

    pub fn set(&self, key: &str, secret: &str) -> Result<(), CredentialError> {
        validate_key(key)?;
        if secret.trim().is_empty() {
            return Err(CredentialError::EmptySecret {
                key: key.to_owned(),
            });
        }
        let mut credentials = self.read_all()?;
        credentials.insert(key.to_owned(), secret.to_owned());
        self.write(&credentials)
    }

    /// Returns whether the key was present.
    pub fn remove(&self, key: &str) -> Result<bool, CredentialError> {
        let mut credentials = self.read_all()?;
        if credentials.remove(key).is_none() {
            return Ok(false);
        }
        self.write(&credentials)?;
        Ok(true)
    }

    fn read_all(&self) -> Result<BTreeMap<String, String>, CredentialError> {
        let bytes = match self.read_file()? {
            Some(bytes) => bytes,
            None => return Ok(BTreeMap::new()),
        };
        let file: CredentialFile = serde_json::from_slice(&bytes).map_err(|error| {
            if error.classify() == serde_json::error::Category::Data {
                CredentialError::Shape {
                    path: self.path.clone(),
                    line: error.line(),
                    column: error.column(),
                }
            } else {
                CredentialError::Json {
                    path: self.path.clone(),
                    line: error.line(),
                    column: error.column(),
                }
            }
        })?;
        if file.version != CREDENTIALS_VERSION {
            return Err(CredentialError::Version {
                path: self.path.clone(),
                version: file.version,
                expected: CREDENTIALS_VERSION,
            });
        }
        Ok(file.credentials)
    }

    /// Read the file without following a symlink at the credential path. The
    /// pre-check gives a clear error; `O_NOFOLLOW` closes the race on Unix.
    /// `O_NONBLOCK` keeps a FIFO from blocking the open, and the fstat below
    /// requires a regular file. The reader does not check the file mode: 0600
    /// is a guarantee for files keel writes, not for files it finds.
    fn read_file(&self) -> Result<Option<Vec<u8>>, CredentialError> {
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(CredentialError::Symlink {
                    path: self.path.clone(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == IoErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(CredentialError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        }

        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut file = match options.open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == IoErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(CredentialError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let metadata = file.metadata().map_err(|source| CredentialError::Io {
            path: self.path.clone(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(not_a_file(&self.path, &metadata));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| CredentialError::Io {
                path: self.path.clone(),
                source,
            })?;
        Ok(Some(bytes))
    }

    fn write(&self, credentials: &BTreeMap<String, String>) -> Result<(), CredentialError> {
        let file = CredentialFile {
            version: CREDENTIALS_VERSION,
            credentials: credentials.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&file).map_err(|_| CredentialError::Encode)?;
        bytes.push(b'\n');

        self.ensure_parent()?;
        let (temp, mut handle) = self.create_temp()?;

        let written = handle.write_all(&bytes).and_then(|_| handle.sync_all());
        if let Err(source) = written {
            drop(handle);
            let _ = fs::remove_file(&temp);
            return Err(CredentialError::Io { path: temp, source });
        }
        drop(handle);

        if let Err(source) = fs::rename(&temp, &self.path) {
            let _ = fs::remove_file(&temp);
            return Err(CredentialError::Io {
                path: self.path.clone(),
                source,
            });
        }

        #[cfg(unix)]
        {
            if let Some(parent) = self.path.parent()
                && let Ok(directory) = File::open(parent)
            {
                let _ = directory.sync_all();
            }
        }
        Ok(())
    }

    /// Create a 0600 temp file in the target directory. A killed process can
    /// leave a temp file behind, and a recycled pid can collide with its name;
    /// retry with a fresh counter instead of failing forever.
    fn create_temp(&self) -> Result<(PathBuf, File), CredentialError> {
        let mut last = None;
        for _ in 0..8 {
            let temp = self.temp_path();
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&temp) {
                Ok(file) => {
                    #[cfg(unix)]
                    if let Err(source) = private_temp(&file) {
                        let _ = fs::remove_file(&temp);
                        return Err(CredentialError::Io { path: temp, source });
                    }
                    return Ok((temp, file));
                }
                Err(error) if error.kind() == IoErrorKind::AlreadyExists => {
                    last = Some((temp, error));
                }
                Err(source) => return Err(CredentialError::Io { path: temp, source }),
            }
        }
        let path = last
            .map(|(path, _)| path)
            .unwrap_or_else(|| self.temp_path());
        Err(CredentialError::TempExhausted { path })
    }

    fn ensure_parent(&self) -> Result<(), CredentialError> {
        let Some(parent) = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        else {
            return Ok(());
        };
        let metadata = match fs::symlink_metadata(parent) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == IoErrorKind::NotFound => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    std::fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(parent)
                        .map_err(|source| CredentialError::Io {
                            path: parent.to_path_buf(),
                            source,
                        })?;
                    self.tighten_private_directory(parent)?;
                    return Ok(());
                }
                #[cfg(not(unix))]
                {
                    return fs::create_dir_all(parent).map_err(|source| CredentialError::Io {
                        path: parent.to_path_buf(),
                        source,
                    });
                }
            }
            Err(source) => {
                return Err(CredentialError::Io {
                    path: parent.to_path_buf(),
                    source,
                });
            }
        };

        if metadata.file_type().is_symlink() {
            // A dotfiles-managed or synced config directory is usually a
            // symlink. Follow it for reads and writes, but never chmod it:
            // the target is not necessarily ours to tighten.
            let followed = fs::metadata(parent).map_err(|source| CredentialError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
            if !followed.is_dir() {
                return Err(not_a_directory(parent));
            }
            return Ok(());
        }
        if !metadata.is_dir() {
            return Err(not_a_directory(parent));
        }
        #[cfg(unix)]
        if self.private_parent {
            self.tighten_private_directory(parent)?;
        }
        Ok(())
    }

    /// Chmod through an opened descriptor with `O_NOFOLLOW`, so a symlink
    /// swapped in after the metadata check cannot redirect the chmod.
    #[cfg(unix)]
    fn tighten_private_directory(&self, path: &Path) -> Result<(), CredentialError> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut options = OpenOptions::new();
        options.read(true);
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
        let directory = options.open(path).map_err(|source| CredentialError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let metadata = directory.metadata().map_err(|source| CredentialError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if metadata.permissions().mode() & 0o777 != 0o700 {
            directory
                .set_permissions(fs::Permissions::from_mode(0o700))
                .map_err(|source| CredentialError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
        }
        Ok(())
    }

    fn temp_path(&self) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "credentials".to_owned());
        self.path
            .with_file_name(format!(".{name}.{}.{sequence}.tmp", std::process::id()))
    }
}

/// The platform config path for the credential file.
pub fn default_path() -> Option<PathBuf> {
    let non_empty = |value: std::ffi::OsString| {
        if value.is_empty() {
            None
        } else {
            Some(PathBuf::from(value))
        }
    };
    #[cfg(target_os = "macos")]
    {
        env::var_os("HOME")
            .and_then(non_empty)
            .map(|home| home.join("Library/Application Support/keel/credentials.json"))
    }
    #[cfg(target_os = "windows")]
    {
        env::var_os("APPDATA")
            .and_then(non_empty)
            .map(|appdata| appdata.join("keel/credentials.json"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        env::var_os("XDG_CONFIG_HOME")
            .and_then(non_empty)
            .or_else(|| {
                env::var_os("HOME")
                    .and_then(non_empty)
                    .map(|home| home.join(".config"))
            })
            .map(|base| base.join("keel/credentials.json"))
    }
}

fn not_a_file(path: &Path, metadata: &fs::Metadata) -> CredentialError {
    let kind = if metadata.is_dir() {
        "directory"
    } else {
        "special file"
    };
    CredentialError::NotAFile {
        path: path.to_path_buf(),
        kind,
    }
}

fn not_a_directory(path: &Path) -> CredentialError {
    CredentialError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            IoErrorKind::InvalidInput,
            "the credential store's parent is not a directory",
        ),
    }
}

/// Some filesystems honor the create mode but reject `fchmod` (FAT/exFAT,
/// many SMB/CIFS and some FUSE mounts). Only chmod when the mode is not
/// already 0600, and fail closed if the file cannot be secured.
#[cfg(unix)]
fn private_temp(file: &File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if file.metadata()?.permissions().mode() & 0o777 != 0o600 {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn validate_key(key: &str) -> Result<(), CredentialError> {
    let reason = if key.is_empty() {
        Some("it is empty")
    } else if key.len() > MAX_KEY_LENGTH {
        Some("it is longer than 128 bytes")
    } else if !key
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
    {
        Some("use only ASCII letters, digits, '.', '_' and '-'")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(CredentialError::InvalidKey {
            key: key.to_owned(),
            reason,
        }),
        None => Ok(()),
    }
}

/// Where a resolver reads a snapshot. Implemented by [`CredentialStore`] and by
/// tests that count reloads.
pub trait CredentialSource: Send + Sync {
    fn load(&self) -> Result<BTreeMap<String, String>, CredentialError>;
}

impl CredentialSource for CredentialStore {
    fn load(&self) -> Result<BTreeMap<String, String>, CredentialError> {
        self.read_all()
    }
}

#[derive(Default)]
struct Snapshot {
    values: BTreeMap<String, String>,
    generation: u64,
}

/// Environment first, then the persistent store.
pub struct StoreResolver {
    source: Arc<dyn CredentialSource>,
    env: EnvResolver,
    snapshot: RwLock<Snapshot>,
    refresh: Mutex<()>,
}

impl StoreResolver {
    pub fn new(store: CredentialStore) -> Self {
        Self::with_source(Arc::new(store))
    }

    pub fn with_source(source: Arc<dyn CredentialSource>) -> Self {
        Self {
            source,
            env: EnvResolver,
            snapshot: RwLock::new(Snapshot::default()),
            refresh: Mutex::new(()),
        }
    }

    /// Replace the cached snapshot with a fresh read. A cached key otherwise
    /// stays cached for the process lifetime.
    pub fn reload(&self) -> Result<(), CredentialError> {
        let _guard = lock(&self.refresh);
        let values = self.source.load()?;
        self.replace_snapshot(values);
        Ok(())
    }

    fn resolve_store(&self, key: &str) -> Result<Option<String>, CredentialError> {
        let observed = {
            let snapshot = read(&self.snapshot);
            if let Some(value) = available(&snapshot.values, key) {
                return Ok(Some(value));
            }
            snapshot.generation
        };

        // The refresh mutex intentionally spans the blocking read: it is the
        // single-flight gate. Cache hits never take it, and `resolve` is
        // synchronous, called before the client's first await.
        let _guard = lock(&self.refresh);
        {
            let snapshot = read(&self.snapshot);
            if snapshot.generation != observed {
                return Ok(available(&snapshot.values, key));
            }
        }
        let values = self.source.load()?;
        let mut snapshot = write(&self.snapshot);
        snapshot.values = values;
        snapshot.generation = snapshot.generation.wrapping_add(1);
        Ok(available(&snapshot.values, key))
    }

    fn replace_snapshot(&self, values: BTreeMap<String, String>) {
        let mut snapshot = write(&self.snapshot);
        snapshot.values = values;
        snapshot.generation = snapshot.generation.wrapping_add(1);
    }
}

impl SecretResolver for StoreResolver {
    fn resolve(&self, reference: &CredentialRef) -> Option<String> {
        self.resolve_result(reference).ok().flatten()
    }

    fn resolve_result(&self, reference: &CredentialRef) -> Result<Option<String>, LlmError> {
        if let Some(value) = self.env.resolve(reference) {
            return Ok(Some(value));
        }
        let Some(key) = reference.store_key.as_deref() else {
            return Ok(None);
        };
        self.resolve_store(key)
            .map_err(|error| LlmError::new(ErrorKind::Auth, error.to_string()))
    }
}

impl std::fmt::Debug for StoreResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = read(&self.snapshot);
        f.debug_struct("StoreResolver")
            .field("credentials", &snapshot.values.len())
            .field("generation", &snapshot.generation)
            .finish_non_exhaustive()
    }
}

fn available(values: &BTreeMap<String, String>, key: &str) -> Option<String> {
    values
        .get(key)
        .filter(|value| !value.trim().is_empty())
        .cloned()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_restricted_to_safe_label_characters() {
        for key in ["openai", "anthropic.work", "team_1", "a-b"] {
            assert!(validate_key(key).is_ok(), "{key}");
        }
        for key in [
            "",
            "with space",
            "slash/key",
            "new\nline",
            "x".repeat(129).as_str(),
        ] {
            assert!(validate_key(key).is_err(), "{key}");
        }
    }

    #[test]
    fn debug_never_prints_a_value() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = CredentialStore::at(directory.path().join("credentials.json"));
        store.set("openai", "sk-test-debug-value").expect("set");
        let resolver = StoreResolver::new(store.clone());
        assert_eq!(
            resolver.resolve(&CredentialRef::from_store("openai")),
            Some("sk-test-debug-value".to_owned())
        );

        let text = format!("{store:?} {resolver:?}");
        assert!(!text.contains("sk-test-debug-value"), "{text}");
    }

    #[test]
    fn the_default_path_is_the_platform_config_file() {
        let Some(path) = default_path() else {
            return;
        };
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("credentials.json")
        );
        assert_eq!(
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            Some("keel")
        );
    }

    #[test]
    fn a_set_override_is_used_without_owning_its_parent() {
        let store =
            CredentialStore::from_override(Some(OsString::from("/tmp/keel-test-credentials.json")))
                .expect("an override needs no default path");
        assert_eq!(store.path, PathBuf::from("/tmp/keel-test-credentials.json"));
        #[cfg(unix)]
        assert!(!store.private_parent);
    }

    #[test]
    fn an_empty_override_falls_back_to_the_default() {
        let store =
            CredentialStore::from_override(Some(OsString::new())).expect("the default path");
        assert_eq!(store.path, default_path().expect("default path"));
        #[cfg(unix)]
        assert!(store.private_parent);
    }

    #[cfg(unix)]
    #[test]
    fn a_private_parent_is_tightened_to_0700() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("temp dir");
        let directory = root.path().join("keel");
        fs::create_dir(&directory).expect("create the directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))
            .expect("loosen the directory");
        let store = CredentialStore {
            path: directory.join("credentials.json"),
            private_parent: true,
        };

        store.set("openai", "sk-test-private-parent").expect("set");
        let mode = fs::metadata(&directory)
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        let file_mode = fs::metadata(store.path())
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_private_parent_is_followed_and_not_chmodded() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = tempfile::tempdir().expect("temp dir");
        let target = root.path().join("real");
        fs::create_dir(&target).expect("create the real directory");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
            .expect("loosen the real directory");
        let link = root.path().join("keel");
        symlink(&target, &link).expect("symlink the private parent");
        let store = CredentialStore {
            path: link.join("credentials.json"),
            private_parent: true,
        };

        store
            .set("openai", "sk-test-symlink-parent")
            .expect("a symlinked private parent is followed");
        assert_eq!(
            store.get("openai").expect("get through the symlink"),
            Some("sk-test-symlink-parent".to_owned())
        );
        let mode = fs::metadata(&target)
            .expect("target metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755, "the symlink target was not chmodded");
        let file_mode = fs::metadata(target.join("credentials.json"))
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
    }
}
