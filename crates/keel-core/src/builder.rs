//! Resolve a catalog entry into a wire route.
//!
//! This is the one place where data (provider, model, compatibility) becomes
//! code (a protocol module with an endpoint and auth).

use std::sync::Arc;

use keel_llm_catalog::{AuthSpec, ModelSpec, Provider};
use keel_llm_route::{Auth, CredentialRef, Framing, Route};
use keel_llm_types::{ErrorKind, LlmError};

pub fn build_route(provider: &Provider, model: &ModelSpec) -> Result<Route, LlmError> {
    let protocol =
        keel_llm_protocol::for_protocol(model.protocol, model.compat()).ok_or_else(|| {
            LlmError::unsupported(format!(
                "protocol `{}` is catalogued but not implemented yet (model `{}/{}`)",
                model.protocol, provider.id, model.id
            ))
        })?;

    let base = provider.resolved_base_url().ok_or_else(|| {
        LlmError::new(
            ErrorKind::InvalidRequest,
            format!(
                "provider `{}` has no base URL; set {}",
                provider.id,
                provider.base_url_env.as_deref().unwrap_or("<base_url_env>")
            ),
        )
    })?;

    let endpoint = format!("{}{}", base.trim_end_matches('/'), model.endpoint_path());

    Ok(Route {
        provider: provider.id.clone().into(),
        model: model.id.clone().into(),
        endpoint,
        protocol: Arc::from(protocol),
        auth: auth_for(&provider.auth),
        framing: Framing::Sse,
        headers: Vec::new(),
    })
}

fn auth_for(spec: &AuthSpec) -> Auth {
    match spec {
        AuthSpec::None => Auth::None,
        AuthSpec::Bearer { env } => Auth::bearer(credential(env)),
        AuthSpec::OptionalBearer { env } => Auth::OptionalBearer {
            credential: credential(env),
        },
        AuthSpec::Header { name, env } => Auth::Header {
            name: name.clone(),
            credential: credential(env),
        },
        AuthSpec::Query { param, env } => Auth::Query {
            param: param.clone(),
            credential: credential(env),
        },
        // The phase-3 module will own the PKCE flow and refresh loop. Until
        // then an access token comes from the environment.
        AuthSpec::GoogleOauth { env } => Auth::bearer(credential(env)),
    }
}

fn credential(env: &[String]) -> CredentialRef {
    CredentialRef::from_env_any(env.iter().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_llm_catalog::Catalog;
    use keel_llm_types::{ErrorKind, ProtocolId};

    fn build(provider: &str, model: &str) -> Result<Route, LlmError> {
        let catalog = Catalog::builtin();
        let (provider, model) = catalog.find(provider, model).expect("catalog entry");
        build_route(provider, model)
    }

    #[test]
    fn builds_an_anthropic_route_end_to_end() {
        let route = build("anthropic", "claude-sonnet-4-5").unwrap();
        assert_eq!(route.protocol_id(), ProtocolId::AnthropicMessages);
        assert!(route.endpoint.ends_with("/messages"));
        assert!(route.endpoint.starts_with("https://api.anthropic.com/v1"));
        assert!(matches!(route.auth, Auth::Bearer { .. }));
    }

    #[test]
    fn builds_a_chat_route_for_a_gateway_model() {
        let route = build("commandcode", "gpt-4.1").unwrap();
        assert_eq!(route.protocol_id(), ProtocolId::OpenAiChat);
        assert!(route.endpoint.ends_with("/provider/v1/chat/completions"));
    }

    #[test]
    fn unimplemented_protocols_fail_with_a_typed_error() {
        let error = build("antigravity", "gemini-2.5-pro").unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert!(error.message.contains("google-generative"));
    }

    #[test]
    fn provider_notes_do_not_leak_into_route_selection() {
        let route = build("opencode", "claude-sonnet-4-5").unwrap();
        assert_eq!(route.protocol_id(), ProtocolId::AnthropicMessages);
    }
}
