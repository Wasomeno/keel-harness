//! Provider and model catalog.
//!
//! The catalog is data: one entry per provider endpoint, one entry per model
//! with the protocol that model speaks. Generated catalogs replace the seed
//! without changing this API.

use std::collections::HashSet;
use std::sync::OnceLock;

use serde::Deserialize;

use keel_llm_types::{Compat, MaxTokensField, ProtocolId};

const SEED: &str = include_str!("../catalog/seed.toml");

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("catalog parse failed: {0}")]
    Parse(#[from] toml::de::Error),
}

#[derive(Debug, Deserialize)]
pub struct Catalog {
    pub version: u32,
    #[serde(default)]
    pub notes: Option<String>,
    pub providers: Vec<Provider>,
}

#[derive(Debug, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub base_url_env: Option<String>,
    #[serde(default)]
    pub auth: AuthSpec,
    #[serde(default)]
    pub notes: Option<String>,
    pub models: Vec<ModelSpec>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthSpec {
    #[default]
    None,
    Bearer {
        env: Vec<String>,
    },
    /// Sent when present; a missing variable is not an error.
    OptionalBearer {
        env: Vec<String>,
    },
    Header {
        name: String,
        env: Vec<String>,
    },
    Query {
        param: String,
        env: Vec<String>,
    },
    /// Reserved for the phase-3 PKCE flow. The first slice reads an access token
    /// from the environment.
    GoogleOauth {
        env: Vec<String>,
    },
}

impl AuthSpec {
    pub fn describe(&self) -> String {
        let names = |env: &[String]| env.first().cloned().unwrap_or_else(|| "<none>".to_owned());
        match self {
            Self::None => "none".to_owned(),
            Self::Bearer { env } => format!("bearer ({})", names(env)),
            Self::OptionalBearer { env } => format!("bearer, optional ({})", names(env)),
            Self::Header { name, env } => format!("header {name} ({})", names(env)),
            Self::Query { param, env } => format!("query {param} ({})", names(env)),
            Self::GoogleOauth { env } => format!("google oauth ({})", names(env)),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ModelSpec {
    pub id: String,
    pub name: String,
    /// The wire protocol this model speaks. Not the provider's protocol.
    pub protocol: ProtocolId,
    /// Overrides the protocol's default path.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub context_window: u32,
    #[serde(default)]
    pub max_output_tokens: u32,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub compat: CompatOverrides,
    #[serde(default)]
    pub deprecated: bool,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Capabilities {
    #[serde(default = "enabled")]
    pub tools: bool,
    #[serde(default = "enabled")]
    pub images: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub cache: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            tools: true,
            images: true,
            reasoning: false,
            cache: false,
        }
    }
}

fn enabled() -> bool {
    true
}

/// Per-model deltas from the protocol defaults.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct CompatOverrides {
    pub max_tokens_field: Option<MaxTokensField>,
    pub supports_images: Option<bool>,
    pub supports_tools: Option<bool>,
    pub supports_reasoning: Option<bool>,
    pub supports_stream_options: Option<bool>,
    pub supports_temperature: Option<bool>,
    pub supports_tool_choice_required: Option<bool>,
}

impl ModelSpec {
    pub fn compat(&self) -> Compat {
        let mut compat = Compat::for_protocol(self.protocol);
        compat.supports_images = self.capabilities.images;
        compat.supports_tools = self.capabilities.tools;
        compat.supports_reasoning = self.capabilities.reasoning;

        if let Some(value) = self.compat.max_tokens_field {
            compat.max_tokens_field = value;
        }
        if let Some(value) = self.compat.supports_images {
            compat.supports_images = value;
        }
        if let Some(value) = self.compat.supports_tools {
            compat.supports_tools = value;
        }
        if let Some(value) = self.compat.supports_reasoning {
            compat.supports_reasoning = value;
        }
        if let Some(value) = self.compat.supports_stream_options {
            compat.supports_stream_options = value;
        }
        if let Some(value) = self.compat.supports_temperature {
            compat.supports_temperature = value;
        }
        if let Some(value) = self.compat.supports_tool_choice_required {
            compat.supports_tool_choice_required = value;
        }

        compat
    }

    pub fn endpoint_path(&self) -> &str {
        self.path
            .as_deref()
            .unwrap_or_else(|| default_path(self.protocol))
    }
}

pub const fn default_path(protocol: ProtocolId) -> &'static str {
    match protocol {
        ProtocolId::OpenAiChat => "/chat/completions",
        ProtocolId::OpenAiResponses => "/responses",
        ProtocolId::AnthropicMessages => "/messages",
        ProtocolId::GoogleGenerative => "/v1internal:streamGenerateContent?alt=sse",
    }
}

impl Provider {
    pub fn model(&self, id: &str) -> Option<&ModelSpec> {
        self.models.iter().find(|model| model.id == id)
    }

    /// Environment override first, then the catalog value. `None` means the
    /// provider needs configuration before it can be routed.
    pub fn resolved_base_url(&self) -> Option<String> {
        if let Some(name) = &self.base_url_env {
            if let Ok(value) = std::env::var(name) {
                if !value.trim().is_empty() {
                    return Some(value);
                }
            }
        }
        if self.base_url.trim().is_empty() {
            None
        } else {
            Some(self.base_url.clone())
        }
    }
}

impl Catalog {
    pub fn builtin() -> &'static Catalog {
        static CATALOG: OnceLock<Catalog> = OnceLock::new();
        CATALOG.get_or_init(|| Catalog::parse(SEED).expect("the built-in catalog must parse"))
    }

    pub fn parse(text: &str) -> Result<Self, CatalogError> {
        Ok(toml::from_str(text)?)
    }

    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|provider| provider.id == id)
    }

    pub fn models(&self) -> impl Iterator<Item = (&Provider, &ModelSpec)> {
        self.providers
            .iter()
            .flat_map(|provider| provider.models.iter().map(move |model| (provider, model)))
    }

    pub fn find(&self, provider_id: &str, model_id: &str) -> Option<(&Provider, &ModelSpec)> {
        self.provider(provider_id)
            .and_then(|provider| provider.model(model_id).map(|model| (provider, model)))
    }

    pub fn find_model(&self, model_id: &str) -> Vec<(&Provider, &ModelSpec)> {
        self.models()
            .filter(|(_, model)| model.id == model_id)
            .collect()
    }

    /// Structural problems that would make a route unbuildable or ambiguous.
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();
        let mut provider_ids = HashSet::new();

        for provider in &self.providers {
            if !provider_ids.insert(provider.id.as_str()) {
                issues.push(format!("duplicate provider id `{}`", provider.id));
            }
            if provider.base_url.trim().is_empty() && provider.base_url_env.is_none() {
                issues.push(format!(
                    "provider `{}` has neither a base URL nor a base_url_env",
                    provider.id
                ));
            }
            if provider.models.is_empty() {
                issues.push(format!("provider `{}` has no models", provider.id));
            }

            let mut model_ids = HashSet::new();
            for model in &provider.models {
                if !model_ids.insert(model.id.as_str()) {
                    issues.push(format!(
                        "duplicate model id `{}` in provider `{}`",
                        model.id, provider.id
                    ));
                }
                if !model.endpoint_path().starts_with('/') {
                    issues.push(format!(
                        "model `{}/{}` has a path that does not start with `/`",
                        provider.id, model.id
                    ));
                }
            }
        }

        issues
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_parses_and_is_structurally_valid() {
        let catalog = Catalog::builtin();
        let issues = catalog.validate();
        assert!(issues.is_empty(), "catalog issues: {issues:?}");
        assert_eq!(catalog.providers.len(), 8);
    }

    #[test]
    fn neuralwatt_serves_openai_chat_from_one_base_url() {
        let catalog = Catalog::builtin();
        let (provider, model) = catalog
            .find("neuralwatt", "nw-small")
            .expect("neuralwatt/nw-small is catalogued");
        assert_eq!(provider.base_url, "https://api.neuralwatt.com/v1");
        assert_eq!(
            provider.base_url_env.as_deref(),
            Some("KEEL_NEURALWATT_BASE_URL")
        );
        assert_eq!(model.protocol, ProtocolId::OpenAiChat);
        assert!(model.capabilities.tools);
        assert!(model.capabilities.images);
        assert!(model.capabilities.reasoning);
        assert_eq!(model.max_output_tokens, 131072);
    }

    #[test]
    fn protocol_belongs_to_the_model_not_the_provider() {
        let catalog = Catalog::builtin();
        let (_, claude) = catalog.find("opencode", "claude-sonnet-4-5").unwrap();
        let (_, gpt) = catalog.find("opencode", "gpt-4.1").unwrap();
        assert_eq!(claude.protocol, ProtocolId::AnthropicMessages);
        assert_eq!(gpt.protocol, ProtocolId::OpenAiChat);
    }

    #[test]
    fn endpoints_join_base_and_default_paths() {
        let catalog = Catalog::builtin();
        let (provider, model) = catalog.find("anthropic", "claude-sonnet-4-5").unwrap();
        assert_eq!(provider.base_url, "https://api.anthropic.com/v1");
        assert_eq!(model.endpoint_path(), "/messages");

        let (_, antigravity) = catalog.find("antigravity", "gemini-2.5-pro").unwrap();
        assert_eq!(
            antigravity.endpoint_path(),
            "/v1internal:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn compat_carries_capabilities_into_the_protocol_defaults() {
        let catalog = Catalog::builtin();
        let (_, model) = catalog.find("anthropic", "claude-sonnet-4-5").unwrap();
        let compat = model.compat();
        assert!(compat.supports_reasoning);
        assert!(compat.supports_tools);
        assert!(!compat.system_in_messages);
    }

    #[test]
    fn unknown_lookups_are_empty() {
        let catalog = Catalog::builtin();
        assert!(catalog.find("nope", "nope").is_none());
        assert!(catalog.find_model("nope").is_empty());
    }

    #[test]
    fn invalid_protocol_names_are_rejected() {
        let text = r#"
version = 1
[[providers]]
id = "x"
name = "X"
base_url = "https://example.test/v1"
[[providers.models]]
id = "m"
name = "M"
protocol = "not-a-protocol"
"#;
        assert!(Catalog::parse(text).is_err());
    }

    #[test]
    fn duplicate_ids_are_reported() {
        let text = r#"
version = 1
[[providers]]
id = "x"
name = "X"
base_url = "https://example.test/v1"
[[providers.models]]
id = "m"
name = "M"
protocol = "openai-chat"
[[providers.models]]
id = "m"
name = "M again"
protocol = "openai-chat"
"#;
        let catalog = Catalog::parse(text).unwrap();
        let issues = catalog.validate();
        assert!(issues.iter().any(|issue| issue.contains("duplicate model")));
    }
}
