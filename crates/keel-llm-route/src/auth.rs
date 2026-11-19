//! Credential resolution.
//!
//! Auth is separate from translation: a protocol describes the header contract,
//! this module finds the secret. The store is a trait so a persistent,
//! account-keyed implementation can land without touching protocols.
//!
//! Secrets are never logged. Errors name the environment variable, not its
//! value.

use std::env;

use keel_llm_types::{ErrorKind, LlmError};

/// Where a secret comes from. Environment variables are checked first, then the
/// account-keyed credential store. `CredentialRef::from_env` keeps the old
/// behavior; the catalog fills both sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialRef {
    /// First non-empty variable wins.
    pub env: Vec<String>,
    pub store_key: Option<String>,
}

impl CredentialRef {
    pub fn from_env(name: impl Into<String>) -> Self {
        Self {
            env: vec![name.into()],
            store_key: None,
        }
    }

    pub fn from_env_any(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            env: names.into_iter().map(Into::into).collect(),
            store_key: None,
        }
    }

    pub fn from_store(key: impl Into<String>) -> Self {
        Self {
            env: Vec::new(),
            store_key: Some(key.into()),
        }
    }

    pub fn or_env(mut self, name: impl Into<String>) -> Self {
        self.env.push(name.into());
        self
    }

    pub fn with_store_key(mut self, key: impl Into<String>) -> Self {
        self.store_key = Some(key.into());
        self
    }

    /// Human-readable sources for route output. Never contains a value.
    pub fn sources(&self) -> String {
        match (self.env.is_empty(), self.store_key.as_deref()) {
            (true, Some(key)) => format!("store key `{key}`"),
            (false, Some(key)) => format!("{} or store key `{key}`", self.env.join(" or ")),
            (false, None) => self.env.join(" or "),
            (true, None) => "<none>".to_owned(),
        }
    }
}

pub trait SecretResolver: Send + Sync {
    fn resolve(&self, reference: &CredentialRef) -> Option<String>;

    /// Like [`SecretResolver::resolve`], but a store failure (permissions, a
    /// malformed file) is reported instead of looking like a missing value.
    fn resolve_result(&self, reference: &CredentialRef) -> Result<Option<String>, LlmError> {
        Ok(self.resolve(reference))
    }
}

#[derive(Debug, Default)]
pub struct EnvResolver;

impl SecretResolver for EnvResolver {
    fn resolve(&self, reference: &CredentialRef) -> Option<String> {
        reference
            .env
            .iter()
            .find_map(|name| env::var(name).ok())
            .filter(|value| !value.trim().is_empty())
    }
}

#[derive(Clone, Debug)]
pub enum Auth {
    None,
    Bearer {
        credential: CredentialRef,
    },
    /// Sent when present. A missing variable, or a store that cannot be read,
    /// is not an error, for local servers and endpoints that accept anonymous
    /// requests.
    OptionalBearer {
        credential: CredentialRef,
    },
    Header {
        name: String,
        credential: CredentialRef,
    },
    Query {
        param: String,
        credential: CredentialRef,
    },
}

impl Auth {
    pub fn bearer(reference: CredentialRef) -> Self {
        Self::Bearer {
            credential: reference,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::None => "none".to_owned(),
            Self::Bearer { credential } => format!("bearer ({})", credential.sources()),
            Self::OptionalBearer { credential } => {
                format!("bearer, optional ({})", credential.sources())
            }
            Self::Header { name, credential } => {
                format!("header {name} ({})", credential.sources())
            }
            Self::Query { param, credential } => {
                format!("query {param} ({})", credential.sources())
            }
        }
    }

    pub fn apply(&self, resolver: &dyn SecretResolver) -> Result<ResolvedAuth, LlmError> {
        match self {
            Self::None => Ok(ResolvedAuth::default()),
            Self::Bearer { credential } => {
                let value = require(resolver, credential)?;
                Ok(ResolvedAuth {
                    headers: vec![("authorization".to_owned(), format!("Bearer {value}"))],
                    query: Vec::new(),
                })
            }
            Self::OptionalBearer { credential } => {
                // A broken store is treated like an absent value here: the
                // variant's contract is that a missing credential is not an
                // error, so a local anonymous server keeps working. Bearer,
                // Header and Query stay strict.
                let headers = resolver
                    .resolve_result(credential)
                    .ok()
                    .flatten()
                    .map(|value| vec![("authorization".to_owned(), format!("Bearer {value}"))])
                    .unwrap_or_default();
                Ok(ResolvedAuth {
                    headers,
                    query: Vec::new(),
                })
            }
            Self::Header { name, credential } => {
                let value = require(resolver, credential)?;
                Ok(ResolvedAuth {
                    headers: vec![(name.clone(), value)],
                    query: Vec::new(),
                })
            }
            Self::Query { param, credential } => {
                let value = require(resolver, credential)?;
                Ok(ResolvedAuth {
                    headers: Vec::new(),
                    query: vec![(param.clone(), value)],
                })
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ResolvedAuth {
    pub headers: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
}

fn require(resolver: &dyn SecretResolver, credential: &CredentialRef) -> Result<String, LlmError> {
    match resolver.resolve_result(credential)? {
        Some(value) => Ok(value),
        None => Err(missing(credential)),
    }
}

fn missing(credential: &CredentialRef) -> LlmError {
    let detail = match (credential.env.is_empty(), credential.store_key.as_deref()) {
        (true, Some(key)) => format!("store key `{key}` is not set"),
        (true, None) => "no source is configured".to_owned(),
        (false, Some(key)) => format!("set {} or store key `{key}`", credential.env.join(" or ")),
        (false, None) => format!("set {}", credential.env.join(" or ")),
    };
    LlmError::new(ErrorKind::Auth, format!("missing credential: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed;

    impl SecretResolver for Fixed {
        fn resolve(&self, _reference: &CredentialRef) -> Option<String> {
            Some("sk-test".to_owned())
        }
    }

    struct Missing;

    impl SecretResolver for Missing {
        fn resolve(&self, _reference: &CredentialRef) -> Option<String> {
            None
        }
    }

    #[test]
    fn bearer_sets_an_authorization_header() {
        let auth = Auth::bearer(CredentialRef::from_env("OPENAI_API_KEY"));
        let resolved = auth.apply(&Fixed).unwrap();
        assert_eq!(
            resolved.headers,
            vec![("authorization".to_owned(), "Bearer sk-test".to_owned())]
        );
    }

    #[test]
    fn query_auth_appends_a_parameter() {
        let auth = Auth::Query {
            param: "key".to_owned(),
            credential: CredentialRef::from_env("KEY"),
        };
        let resolved = auth.apply(&Fixed).unwrap();
        assert_eq!(
            resolved.query,
            vec![("key".to_owned(), "sk-test".to_owned())]
        );
    }

    #[test]
    fn missing_credentials_name_the_variable_not_a_value() {
        let auth = Auth::bearer(CredentialRef::from_env("OPENAI_API_KEY"));
        let error = auth.apply(&Missing).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Auth);
        assert!(error.message.contains("OPENAI_API_KEY"));
        assert!(!error.message.contains("sk-"));
    }
}
