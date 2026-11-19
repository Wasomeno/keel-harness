//! Merging the three catalog layers.
//!
//! The checked-in overlay is merged with the generated snapshot once, at
//! compile time through `include_str!`. A runtime patch then merges into a copy
//! of that catalog. Neither merge mutates its input.

use std::collections::HashSet;

use crate::schema::{
    CATALOG_VERSION, CatalogPatch, ModelPatch, OverlayCatalog, OverlayModel, Snapshot,
    SnapshotProvider,
};
use crate::{Capabilities, Catalog, CatalogError, CompatOverrides, ModelSpec, Provider};

/// Builds the embedded catalog from the checked-in overlay and snapshot.
///
/// Precedence: the overlay wins over the snapshot, and the snapshot wins over
/// struct defaults. A field the overlay omits comes from the snapshot.
pub(crate) fn from_parts(
    overlay: OverlayCatalog,
    snapshot: Snapshot,
) -> Result<Catalog, CatalogError> {
    check_version("overlay", overlay.version)?;
    check_version("models.dev snapshot", snapshot.version)?;
    validate_snapshot(&snapshot)?;

    let mut catalog = Catalog {
        version: overlay.version,
        notes: overlay.notes,
        providers: Vec::with_capacity(overlay.providers.len()),
    };

    for provider in overlay.providers {
        let generated = provider
            .models_dev_id
            .as_deref()
            .and_then(|id| snapshot.provider(id));
        let name = provider.name.or_else(|| generated.map(|p| p.name.clone()));
        let name = name.ok_or_else(|| {
            invalid(format!(
                "provider `{}` has no name in the overlay or the snapshot",
                provider.id
            ))
        })?;

        let mut merged = Provider {
            id: provider.id.clone(),
            name,
            base_url: provider.base_url.unwrap_or_default(),
            base_url_env: provider.base_url_env,
            auth: provider.auth.unwrap_or_default(),
            notes: provider.notes,
            models: Vec::with_capacity(provider.models.len()),
        };

        for model in provider.models {
            merged
                .models
                .push(merge_model(&provider.id, model, generated)?);
        }
        catalog.providers.push(merged);
    }

    validate(catalog)
}

fn merge_model(
    provider_id: &str,
    overlay: OverlayModel,
    generated: Option<&SnapshotProvider>,
) -> Result<ModelSpec, CatalogError> {
    let fact = generated.and_then(|provider| {
        provider.model(overlay.models_dev_id.as_deref().unwrap_or(&overlay.id))
    });

    let name = overlay
        .name
        .or_else(|| fact.map(|model| model.name.clone()))
        .ok_or_else(|| {
            invalid(format!(
                "model `{provider_id}/{}` has no name in the overlay or the snapshot",
                overlay.id
            ))
        })?;

    let base = fact.map_or_else(Capabilities::default, |model| Capabilities {
        tools: model.capabilities.tools,
        images: model.capabilities.images,
        reasoning: model.capabilities.reasoning,
        cache: false,
    });
    let capabilities = overlay.capabilities.unwrap_or_default().resolve(base);

    Ok(ModelSpec {
        id: overlay.id,
        name,
        protocol: overlay.protocol,
        path: overlay.path,
        context_window: overlay
            .context_window
            .or_else(|| fact.map(|model| model.context_window))
            .unwrap_or(0),
        max_output_tokens: overlay
            .max_output_tokens
            .or_else(|| fact.map(|model| model.max_output_tokens))
            .unwrap_or(0),
        capabilities,
        compat: overlay.compat.unwrap_or_default(),
        deprecated: overlay
            .deprecated
            .or_else(|| fact.map(|model| model.deprecated))
            .unwrap_or(false),
    })
}

/// Applies a runtime patch to a copy of `base`.
///
/// Existing providers and models keep their position; new ones append in the
/// order the patch lists them. Any problem rejects the whole patch. Duplicate
/// provider entries in one patch apply in order and are reported as warnings.
pub(crate) fn apply_patch(
    base: &Catalog,
    patch: CatalogPatch,
) -> Result<(Catalog, Vec<String>), CatalogError> {
    if let Some(version) = patch.version {
        check_version("runtime overlay", version)?;
    }

    let mut catalog = base.clone();
    let mut warnings = Vec::new();
    let mut seen_providers = HashSet::new();
    if let Some(notes) = patch.notes {
        catalog.notes = Some(notes);
    }

    for provider_patch in patch.providers {
        if !seen_providers.insert(provider_patch.id.clone()) {
            warnings.push(format!(
                "runtime overlay lists provider `{}` more than once; the patches apply in order",
                provider_patch.id
            ));
        }
        if let Some(provider) = catalog
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_patch.id)
        {
            apply_provider(provider, provider_patch)?;
        } else {
            let name = provider_patch.name.ok_or_else(|| {
                invalid(format!("new provider `{}` needs a name", provider_patch.id))
            })?;
            let mut provider = Provider {
                id: provider_patch.id.clone(),
                name,
                base_url: provider_patch.base_url.unwrap_or_default(),
                base_url_env: provider_patch.base_url_env,
                auth: provider_patch.auth.unwrap_or_default(),
                notes: provider_patch.notes,
                models: Vec::with_capacity(provider_patch.models.len()),
            };
            for model in provider_patch.models {
                apply_model(&mut provider, model)?;
            }
            catalog.providers.push(provider);
        }
    }

    Ok((validate(catalog)?, warnings))
}

fn apply_provider(
    provider: &mut Provider,
    patch: crate::schema::ProviderPatch,
) -> Result<(), CatalogError> {
    if let Some(name) = patch.name {
        provider.name = name;
    }
    if let Some(base_url) = patch.base_url {
        provider.base_url = base_url;
    }
    if let Some(base_url_env) = patch.base_url_env {
        provider.base_url_env = Some(base_url_env);
    }
    if let Some(auth) = patch.auth {
        provider.auth = auth;
    }
    if let Some(notes) = patch.notes {
        provider.notes = Some(notes);
    }
    for model in patch.models {
        apply_model(provider, model)?;
    }
    Ok(())
}

fn apply_model(provider: &mut Provider, patch: ModelPatch) -> Result<(), CatalogError> {
    if let Some(index) = provider
        .models
        .iter()
        .position(|model| model.id == patch.id)
    {
        let model = &mut provider.models[index];
        if let Some(name) = patch.name {
            model.name = name;
        }
        if let Some(protocol) = patch.protocol {
            model.protocol = protocol;
        }
        if let Some(path) = patch.path {
            model.path = Some(path);
        }
        if let Some(context_window) = patch.context_window {
            model.context_window = context_window;
        }
        if let Some(max_output_tokens) = patch.max_output_tokens {
            model.max_output_tokens = max_output_tokens;
        }
        if let Some(capabilities) = patch.capabilities {
            model.capabilities = capabilities.resolve(model.capabilities);
        }
        if let Some(compat) = patch.compat {
            model.compat = merge_compat(model.compat, compat);
        }
        if let Some(deprecated) = patch.deprecated {
            model.deprecated = deprecated;
        }
        return Ok(());
    }

    let name = patch.name.ok_or_else(|| {
        invalid(format!(
            "new model `{}/{}` needs a name",
            provider.id, patch.id
        ))
    })?;
    let protocol = patch.protocol.ok_or_else(|| {
        invalid(format!(
            "new model `{}/{}` needs a protocol",
            provider.id, patch.id
        ))
    })?;
    let capabilities = patch
        .capabilities
        .unwrap_or_default()
        .resolve(Capabilities::default());

    provider.models.push(ModelSpec {
        id: patch.id,
        name,
        protocol,
        path: patch.path,
        context_window: patch.context_window.unwrap_or(0),
        max_output_tokens: patch.max_output_tokens.unwrap_or(0),
        capabilities,
        compat: patch.compat.unwrap_or_default(),
        deprecated: patch.deprecated.unwrap_or(false),
    });
    Ok(())
}

fn merge_compat(mut base: CompatOverrides, patch: CompatOverrides) -> CompatOverrides {
    if patch.max_tokens_field.is_some() {
        base.max_tokens_field = patch.max_tokens_field;
    }
    if patch.supports_images.is_some() {
        base.supports_images = patch.supports_images;
    }
    if patch.supports_tools.is_some() {
        base.supports_tools = patch.supports_tools;
    }
    if patch.supports_reasoning.is_some() {
        base.supports_reasoning = patch.supports_reasoning;
    }
    if patch.supports_stream_options.is_some() {
        base.supports_stream_options = patch.supports_stream_options;
    }
    if patch.supports_temperature.is_some() {
        base.supports_temperature = patch.supports_temperature;
    }
    if patch.supports_tool_choice_required.is_some() {
        base.supports_tool_choice_required = patch.supports_tool_choice_required;
    }
    base
}

fn check_version(what: &str, version: u32) -> Result<(), CatalogError> {
    if version == CATALOG_VERSION {
        Ok(())
    } else {
        Err(invalid(format!(
            "unsupported {what} version {version}; this build understands version {CATALOG_VERSION}"
        )))
    }
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<(), CatalogError> {
    let mut provider_ids = HashSet::new();
    for provider in &snapshot.providers {
        if !provider_ids.insert(provider.id.as_str()) {
            return Err(invalid(format!(
                "duplicate provider id `{}` in the models.dev snapshot",
                provider.id
            )));
        }
        let mut model_ids = HashSet::new();
        for model in &provider.models {
            if !model_ids.insert(model.id.as_str()) {
                return Err(invalid(format!(
                    "duplicate model id `{}` in provider `{}` in the models.dev snapshot",
                    model.id, provider.id
                )));
            }
        }
    }
    Ok(())
}

fn validate(catalog: Catalog) -> Result<Catalog, CatalogError> {
    let issues = catalog.validate();
    if issues.is_empty() {
        Ok(catalog)
    } else {
        Err(invalid(issues.join("; ")))
    }
}

fn invalid(message: impl Into<String>) -> CatalogError {
    CatalogError::Invalid(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        CapabilitiesPatch, OverlayCatalog, OverlayModel, OverlayProvider, SnapshotCapabilities,
        SnapshotModel, SnapshotProvider,
    };
    use keel_llm_types::ProtocolId;

    fn snapshot() -> Snapshot {
        Snapshot {
            version: 1,
            source: "test".to_owned(),
            fetched_at: "2026-11-19".to_owned(),
            providers: vec![SnapshotProvider {
                id: "md".to_owned(),
                name: "Generated name".to_owned(),
                models: vec![SnapshotModel {
                    id: "alias".to_owned(),
                    name: "Generated model".to_owned(),
                    context_window: 1000,
                    max_output_tokens: 100,
                    capabilities: SnapshotCapabilities {
                        tools: false,
                        images: true,
                        reasoning: true,
                    },
                    deprecated: true,
                }],
            }],
        }
    }

    fn overlay() -> OverlayCatalog {
        OverlayCatalog {
            version: 1,
            notes: Some("test".to_owned()),
            providers: vec![OverlayProvider {
                id: "keel".to_owned(),
                name: None,
                base_url: Some("https://example.test/v1".to_owned()),
                base_url_env: None,
                auth: None,
                notes: None,
                models_dev_id: Some("md".to_owned()),
                models: vec![OverlayModel {
                    id: "model".to_owned(),
                    name: None,
                    protocol: ProtocolId::OpenAiChat,
                    models_dev_id: Some("alias".to_owned()),
                    path: None,
                    context_window: Some(2000),
                    max_output_tokens: None,
                    capabilities: Some(CapabilitiesPatch {
                        tools: Some(true),
                        images: None,
                        reasoning: None,
                        cache: Some(true),
                    }),
                    compat: None,
                    deprecated: None,
                }],
            }],
        }
    }

    #[test]
    fn a_models_dev_alias_fills_the_keel_id_with_generated_facts() {
        let catalog = from_parts(overlay(), snapshot()).expect("merge");
        let (provider, model) = catalog.find("keel", "model").expect("catalogued");

        assert_eq!(provider.name, "Generated name");
        assert_eq!(model.name, "Generated model");
        assert_eq!(model.context_window, 2000, "overlay pins the window");
        assert_eq!(model.max_output_tokens, 100, "snapshot fills the output");
        assert!(model.capabilities.tools, "overlay patch wins");
        assert!(model.capabilities.images, "snapshot fills images");
        assert!(model.capabilities.reasoning, "snapshot fills reasoning");
        assert!(model.capabilities.cache, "overlay patch adds cache");
        assert!(model.deprecated, "snapshot fills the status");
    }

    #[test]
    fn a_new_provider_appends_and_a_new_model_appends() {
        let base = from_parts(overlay(), snapshot()).expect("merge");
        let patch = toml::from_str(
            r#"
version = 1
[[providers]]
id = "keel"
[[providers.models]]
id = "second"
name = "Second"
protocol = "openai-chat"
[[providers]]
id = "new"
name = "New"
base_url = "https://new.test/v1"
[[providers.models]]
id = "one"
name = "One"
protocol = "openai-chat"
"#,
        )
        .expect("patch");

        let (catalog, warnings) = apply_patch(&base, patch).expect("apply");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(catalog.providers.len(), 2);
        assert_eq!(catalog.providers[0].id, "keel");
        assert_eq!(catalog.providers[0].models.len(), 2);
        assert_eq!(catalog.providers[1].id, "new");
    }

    #[test]
    fn a_failed_patch_leaves_the_base_untouched() {
        let base = from_parts(overlay(), snapshot()).expect("merge");
        let patch = toml::from_str(
            r#"
version = 2
[[providers]]
id = "keel"
"#,
        )
        .expect("patch");
        assert!(apply_patch(&base, patch).is_err());
        assert_eq!(base.providers.len(), 1);
        assert_eq!(base.providers[0].models.len(), 1);
    }

    #[test]
    fn a_compat_patch_changes_only_the_fields_it_sets() {
        let mut base = from_parts(overlay(), snapshot()).expect("merge");
        base.providers[0].models[0].compat = CompatOverrides {
            supports_temperature: Some(false),
            ..CompatOverrides::default()
        };
        let patch = toml::from_str(
            r#"
[[providers]]
id = "keel"
[[providers.models]]
id = "model"
[providers.models.compat]
supports_stream_options = false
"#,
        )
        .expect("patch");

        let (catalog, warnings) = apply_patch(&base, patch).expect("apply");
        assert!(warnings.is_empty(), "{warnings:?}");
        let model = &catalog.providers[0].models[0];
        assert_eq!(model.compat.supports_temperature, Some(false));
        assert_eq!(model.compat.supports_stream_options, Some(false));
    }
}
