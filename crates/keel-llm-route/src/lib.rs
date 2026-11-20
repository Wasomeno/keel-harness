//! Route composition, credentials, HTTP transport and SSE framing.
//!
//! Dependency rule: this crate may depend on protocols and types, never on the
//! catalog or the session loop.

pub mod auth;
pub mod client;
pub mod credentials;
pub mod retry;
pub mod route;
pub mod sse;

pub use auth::{Auth, CredentialRef, EnvResolver, ResolvedAuth, SecretResolver};
pub use client::{Client, DEFAULT_STREAM_IDLE_TIMEOUT, EventStream, STREAM_IDLE_TIMEOUT_ENV};
pub use credentials::{
    CREDENTIALS_ENV, CREDENTIALS_VERSION, CredentialError, CredentialSource, CredentialStore,
    StoreResolver, default_path,
};
pub use retry::{RetryDecision, RetryPolicy};
pub use route::{Framing, Route};
pub use sse::{SseDecoder, SseFrame};
