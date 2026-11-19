//! Route composition: `Protocol + Endpoint + Auth + Framing`.
//!
//! A route is data assembled by the catalog layer. It never decides retries or
//! transcript shape; those belong to the session.

use std::fmt;
use std::sync::Arc;

use keel_llm_protocol::Protocol;
use keel_llm_types::{ModelId, ProtocolId, ProviderId};

use crate::auth::Auth;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    Sse,
}

#[derive(Clone)]
pub struct Route {
    pub provider: ProviderId,
    pub model: ModelId,
    pub endpoint: String,
    pub protocol: Arc<dyn Protocol>,
    pub auth: Auth,
    pub framing: Framing,
    pub headers: Vec<(String, String)>,
    /// The catalog's context window for this model. Zero means unknown, which
    /// disables session compaction.
    pub context_window: u32,
    /// The catalog's output limit. The session uses it as the next request's
    /// output budget when the caller sets no explicit `max_output_tokens`.
    pub max_output_tokens: u32,
}

impl Route {
    pub fn protocol_id(&self) -> ProtocolId {
        self.protocol.id()
    }
}

impl fmt::Debug for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Route")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .field("protocol", &self.protocol.id())
            .field("auth", &self.auth)
            .field("framing", &self.framing)
            .field("context_window", &self.context_window)
            .field("max_output_tokens", &self.max_output_tokens)
            .finish()
    }
}
