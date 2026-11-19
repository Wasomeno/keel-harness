//! Pins the eight providers and every model keel shipped before the generated
//! catalog. A refresh of `models.dev.toml` must not move these routes, windows
//! or capabilities; the overlay pins any fact that would.

use keel_llm_catalog::Catalog;
use keel_llm_types::ProtocolId;

struct ExpectedProvider {
    id: &'static str,
    name: &'static str,
    base_url: &'static str,
    base_url_env: &'static str,
    auth: &'static str,
    models: usize,
}

const PROVIDERS: &[ExpectedProvider] = &[
    ExpectedProvider {
        id: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        base_url_env: "KEEL_OPENAI_BASE_URL",
        auth: "bearer (OPENAI_API_KEY)",
        models: 4,
    },
    ExpectedProvider {
        id: "anthropic",
        name: "Anthropic",
        base_url: "https://api.anthropic.com/v1",
        base_url_env: "KEEL_ANTHROPIC_BASE_URL",
        auth: "bearer (ANTHROPIC_API_KEY)",
        models: 3,
    },
    ExpectedProvider {
        id: "opencode",
        name: "OpenCode",
        base_url: "https://opencode.ai/zen/v1",
        base_url_env: "KEEL_OPENCODE_BASE_URL",
        auth: "bearer (OPENCODE_API_KEY)",
        models: 3,
    },
    ExpectedProvider {
        id: "commandcode",
        name: "CommandCode",
        base_url: "https://api.commandcode.ai/provider/v1",
        base_url_env: "KEEL_COMMANDCODE_BASE_URL",
        auth: "bearer (COMMANDCODE_API_KEY)",
        models: 2,
    },
    ExpectedProvider {
        id: "grok",
        name: "Grok (xAI)",
        base_url: "https://api.x.ai/v1",
        base_url_env: "KEEL_GROK_BASE_URL",
        auth: "bearer (XAI_API_KEY)",
        models: 2,
    },
    ExpectedProvider {
        id: "antigravity",
        name: "Antigravity",
        base_url: "https://daily-cloudcode-pa.googleapis.com",
        base_url_env: "KEEL_ANTIGRAVITY_BASE_URL",
        auth: "google oauth (KEEL_ANTIGRAVITY_ACCESS_TOKEN)",
        models: 3,
    },
    ExpectedProvider {
        id: "neuralwatt",
        name: "NeuralWatt",
        base_url: "https://api.neuralwatt.com/v1",
        base_url_env: "KEEL_NEURALWATT_BASE_URL",
        auth: "bearer (NEURALWATT_API_KEY)",
        models: 3,
    },
    ExpectedProvider {
        id: "custom",
        name: "Custom endpoint",
        base_url: "",
        base_url_env: "KEEL_CUSTOM_BASE_URL",
        auth: "bearer, optional (KEEL_CUSTOM_API_KEY)",
        models: 1,
    },
];

struct ExpectedModel {
    provider: &'static str,
    model: &'static str,
    protocol: ProtocolId,
    context_window: u32,
    max_output_tokens: u32,
    path: &'static str,
    tools: bool,
    images: bool,
    reasoning: bool,
    cache: bool,
}

const MODELS: &[ExpectedModel] = &[
    ExpectedModel {
        provider: "openai",
        model: "gpt-4.1",
        protocol: ProtocolId::OpenAiChat,
        context_window: 1047576,
        max_output_tokens: 32768,
        path: "/chat/completions",
        tools: true,
        images: true,
        reasoning: false,
        cache: false,
    },
    ExpectedModel {
        provider: "openai",
        model: "gpt-4.1-mini",
        protocol: ProtocolId::OpenAiChat,
        context_window: 1047576,
        max_output_tokens: 32768,
        path: "/chat/completions",
        tools: true,
        images: true,
        reasoning: false,
        cache: false,
    },
    ExpectedModel {
        provider: "openai",
        model: "gpt-4o",
        protocol: ProtocolId::OpenAiChat,
        context_window: 128000,
        max_output_tokens: 16384,
        path: "/chat/completions",
        tools: true,
        images: true,
        reasoning: false,
        cache: false,
    },
    ExpectedModel {
        provider: "openai",
        model: "gpt-5.5",
        protocol: ProtocolId::OpenAiResponses,
        context_window: 400000,
        max_output_tokens: 128000,
        path: "/responses",
        tools: true,
        images: true,
        reasoning: true,
        cache: true,
    },
    ExpectedModel {
        provider: "anthropic",
        model: "claude-sonnet-4-5",
        protocol: ProtocolId::AnthropicMessages,
        context_window: 200000,
        max_output_tokens: 64000,
        path: "/messages",
        tools: true,
        images: true,
        reasoning: true,
        cache: true,
    },
    ExpectedModel {
        provider: "anthropic",
        model: "claude-opus-4-1",
        protocol: ProtocolId::AnthropicMessages,
        context_window: 200000,
        max_output_tokens: 32000,
        path: "/messages",
        tools: true,
        images: true,
        reasoning: true,
        cache: true,
    },
    ExpectedModel {
        provider: "anthropic",
        model: "claude-haiku-4-5",
        protocol: ProtocolId::AnthropicMessages,
        context_window: 200000,
        max_output_tokens: 32000,
        path: "/messages",
        tools: true,
        images: true,
        reasoning: true,
        cache: true,
    },
    ExpectedModel {
        provider: "opencode",
        model: "claude-sonnet-4-5",
        protocol: ProtocolId::AnthropicMessages,
        context_window: 200000,
        max_output_tokens: 64000,
        path: "/messages",
        tools: true,
        images: true,
        reasoning: true,
        cache: true,
    },
    ExpectedModel {
        provider: "opencode",
        model: "gpt-4.1",
        protocol: ProtocolId::OpenAiChat,
        context_window: 1047576,
        max_output_tokens: 32768,
        path: "/chat/completions",
        tools: true,
        images: true,
        reasoning: false,
        cache: false,
    },
    ExpectedModel {
        provider: "opencode",
        model: "gemini-2.5-pro",
        protocol: ProtocolId::GoogleGenerative,
        context_window: 1048576,
        max_output_tokens: 65536,
        path: "/v1internal:streamGenerateContent?alt=sse",
        tools: true,
        images: true,
        reasoning: true,
        cache: false,
    },
    ExpectedModel {
        provider: "commandcode",
        model: "claude-sonnet-4-5",
        protocol: ProtocolId::AnthropicMessages,
        context_window: 200000,
        max_output_tokens: 64000,
        path: "/messages",
        tools: true,
        images: true,
        reasoning: true,
        cache: true,
    },
    ExpectedModel {
        provider: "commandcode",
        model: "gpt-4.1",
        protocol: ProtocolId::OpenAiChat,
        context_window: 1047576,
        max_output_tokens: 32768,
        path: "/chat/completions",
        tools: true,
        images: true,
        reasoning: false,
        cache: false,
    },
    ExpectedModel {
        provider: "grok",
        model: "grok-4",
        protocol: ProtocolId::OpenAiChat,
        context_window: 256000,
        max_output_tokens: 32768,
        path: "/chat/completions",
        tools: true,
        images: true,
        reasoning: true,
        cache: false,
    },
    ExpectedModel {
        provider: "grok",
        model: "grok-3-mini",
        protocol: ProtocolId::OpenAiChat,
        context_window: 131072,
        max_output_tokens: 16384,
        path: "/chat/completions",
        tools: true,
        images: true,
        reasoning: false,
        cache: false,
    },
    ExpectedModel {
        provider: "antigravity",
        model: "gemini-2.5-pro",
        protocol: ProtocolId::GoogleGenerative,
        context_window: 1048576,
        max_output_tokens: 65536,
        path: "/v1internal:streamGenerateContent?alt=sse",
        tools: true,
        images: true,
        reasoning: true,
        cache: false,
    },
    ExpectedModel {
        provider: "antigravity",
        model: "gemini-2.5-flash",
        protocol: ProtocolId::GoogleGenerative,
        context_window: 1048576,
        max_output_tokens: 65536,
        path: "/v1internal:streamGenerateContent?alt=sse",
        tools: true,
        images: true,
        reasoning: true,
        cache: false,
    },
    ExpectedModel {
        provider: "antigravity",
        model: "claude-sonnet-4-5",
        protocol: ProtocolId::GoogleGenerative,
        context_window: 200000,
        max_output_tokens: 64000,
        path: "/v1internal:streamGenerateContent?alt=sse",
        tools: true,
        images: true,
        reasoning: true,
        cache: false,
    },
    ExpectedModel {
        provider: "neuralwatt",
        model: "nw-small",
        protocol: ProtocolId::OpenAiChat,
        context_window: 262128,
        max_output_tokens: 131072,
        path: "/chat/completions",
        tools: true,
        images: true,
        reasoning: true,
        cache: false,
    },
    ExpectedModel {
        provider: "neuralwatt",
        model: "qwen-3.8-27b",
        protocol: ProtocolId::OpenAiChat,
        context_window: 262128,
        max_output_tokens: 131072,
        path: "/chat/completions",
        tools: true,
        images: true,
        reasoning: true,
        cache: false,
    },
    ExpectedModel {
        provider: "neuralwatt",
        model: "deepseek-v4.1-flash",
        protocol: ProtocolId::OpenAiChat,
        context_window: 1048560,
        max_output_tokens: 393216,
        path: "/chat/completions",
        tools: true,
        images: true,
        reasoning: true,
        cache: false,
    },
    ExpectedModel {
        provider: "custom",
        model: "default",
        protocol: ProtocolId::OpenAiChat,
        context_window: 0,
        max_output_tokens: 0,
        path: "/chat/completions",
        tools: true,
        images: false,
        reasoning: false,
        cache: false,
    },
];

#[test]
fn the_eight_existing_providers_keep_their_endpoints_auth_and_membership() {
    let catalog = Catalog::builtin();
    assert_eq!(catalog.providers.len(), PROVIDERS.len());

    for (actual, expected) in catalog.providers.iter().zip(PROVIDERS) {
        assert_eq!(actual.id, expected.id);
        assert_eq!(actual.name, expected.name);
        assert_eq!(actual.base_url, expected.base_url);
        assert_eq!(actual.base_url_env.as_deref(), Some(expected.base_url_env));
        assert_eq!(actual.auth.describe(), expected.auth);
        assert_eq!(actual.models.len(), expected.models, "{}", expected.id);
    }
}

#[test]
fn every_existing_model_keeps_its_protocol_window_path_and_capabilities() {
    let catalog = Catalog::builtin();
    assert_eq!(catalog.models().count(), MODELS.len());

    for expected in MODELS {
        let (_, model) = catalog
            .find(expected.provider, expected.model)
            .unwrap_or_else(|| panic!("{}/{} is missing", expected.provider, expected.model));

        assert_eq!(
            model.protocol, expected.protocol,
            "{}/{} protocol",
            expected.provider, expected.model
        );
        assert_eq!(
            model.context_window, expected.context_window,
            "{}/{} context window",
            expected.provider, expected.model
        );
        assert_eq!(
            model.max_output_tokens, expected.max_output_tokens,
            "{}/{} output limit",
            expected.provider, expected.model
        );
        assert_eq!(
            model.endpoint_path(),
            expected.path,
            "{}/{} endpoint path",
            expected.provider,
            expected.model
        );
        assert_eq!(
            model.capabilities.tools, expected.tools,
            "{}/{} tools",
            expected.provider, expected.model
        );
        assert_eq!(
            model.capabilities.images, expected.images,
            "{}/{} images",
            expected.provider, expected.model
        );
        assert_eq!(
            model.capabilities.reasoning, expected.reasoning,
            "{}/{} reasoning",
            expected.provider, expected.model
        );
        assert_eq!(
            model.capabilities.cache, expected.cache,
            "{}/{} cache",
            expected.provider, expected.model
        );

        let compat = model.compat();
        assert_eq!(compat.supports_tools, expected.tools);
        assert_eq!(compat.supports_images, expected.images);
        assert_eq!(compat.supports_reasoning, expected.reasoning);
    }
}
