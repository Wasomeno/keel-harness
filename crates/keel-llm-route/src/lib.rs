//! Route composition, credentials, HTTP transport and SSE framing.
//!
//! Dependency rule: this crate may depend on protocols and types, never on the
//! catalog or the session loop.

pub mod auth;
pub mod client;
pub mod retry;
pub mod route;
pub mod sse;

pub use auth::{Auth, CredentialRef, EnvResolver, ResolvedAuth, SecretResolver};
pub use client::{Client, EventStream};
pub use retry::{RetryDecision, RetryPolicy};
pub use route::{Framing, Route};
pub use sse::{SseDecoder, SseFrame};
