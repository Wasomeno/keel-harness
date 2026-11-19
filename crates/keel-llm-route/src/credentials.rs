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
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind as IoErrorKind, Write};
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
    /// The platform default directory belongs to keel, so an existing one is
    /// tightened to 0700. An explicit `KEEL_CREDENTIALS` parent is left alone.
    private_parent: bool,
}

impl CredentialStore {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            private_parent: false,
        }
    }

    /// `KEEL_CREDENTIALS` when set, otherwise the platform config path.
    pub fn discover() -> Result<Self, CredentialError> {
        if let Some(path) = env::var_os(CREDENTIALS_ENV).filter(|value| !value.is_empty()) {
            return Ok(Self::at(PathBuf::from(path)));
        }
        let path = default_path().ok_or(CredentialError::NoPath)?;
        Ok(Self {
            path,
            private_parent: true,
        })
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
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == IoErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(source) => {
                return Err(CredentialError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let file: CredentialFile =
            serde_json::from_slice(&bytes).map_err(|error| CredentialError::Json {
                path: self.path.clone(),
                line: error.line(),
                column: error.column(),
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

    fn write(&self, credentials: &BTreeMap<String, String>) -> Result<(), CredentialError> {
        let file = CredentialFile {
            version: CREDENTIALS_VERSION,
            credentials: credentials.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&file).map_err(|_| CredentialError::Encode)?;
        bytes.push(b'\n');

        self.ensure_parent()?;
        let temp = self.temp_path();
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut handle = options.open(&temp).map_err(|source| CredentialError::Io {
            path: temp.clone(),
            source,
        })?;

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
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
            if let Some(parent) = self.path.parent()
                && let Ok(directory) = File::open(parent)
            {
                let _ = directory.sync_all();
            }
        }
        Ok(())
    }

    fn ensure_parent(&self) -> Result<(), CredentialError> {
        let Some(parent) = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        else {
            return Ok(());
        };
        match fs::metadata(parent) {
            Ok(metadata) if metadata.is_dir() => {
                #[cfg(unix)]
                if self.private_parent {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o777 != 0o700 {
                        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(
                            |source| CredentialError::Io {
                                path: parent.to_path_buf(),
                                source,
                            },
                        )?;
                    }
                }
                let _ = metadata;
                Ok(())
            }
            Ok(_) => Err(CredentialError::Io {
                path: parent.to_path_buf(),
                source: std::io::Error::new(
                    IoErrorKind::AlreadyExists,
                    "the credential store's parent is not a directory",
                ),
            }),
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
                        })
                }
                #[cfg(not(unix))]
                {
                    fs::create_dir_all(parent).map_err(|source| CredentialError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })
                }
            }
            Err(source) => Err(CredentialError::Io {
                path: parent.to_path_buf(),
                source,
            }),
        }
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
    #[cfg(target_os = "macos")]
    {
        env::var_os("HOME").map(|home| {
            PathBuf::from(home).join("Library/Application Support/keel/credentials.json")
        })
    }
    #[cfg(target_os = "windows")]
    {
        env::var_os("APPDATA").map(|appdata| PathBuf::from(appdata).join("keel/credentials.json"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|base| base.join("keel/credentials.json"))
    }
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
            if let Some(value) = snapshot.values.get(key) {
                return Ok(Some(value.clone()));
            }
            snapshot.generation
        };

        // Single flight: one reload per generation. Callers that saw a newer
        // generation use that result instead of reading the file again.
        let _guard = lock(&self.refresh);
        {
            let snapshot = read(&self.snapshot);
            if snapshot.generation != observed {
                return Ok(snapshot.values.get(key).cloned());
            }
        }
        let values = self.source.load()?;
        let mut snapshot = write(&self.snapshot);
        snapshot.values = values;
        snapshot.generation = snapshot.generation.wrapping_add(1);
        Ok(snapshot.values.get(key).cloned())
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
        assert!(text.contains("credentials"));
    }
}
