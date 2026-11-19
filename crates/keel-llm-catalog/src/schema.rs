//! On-disk catalog formats.
//!
//! There are three formats, and each has one definition here so the runtime and
//! the `keel-catalog-gen` developer tool cannot drift apart:
//!
//! - [`Snapshot`]: the committed `catalog/models.dev.toml` facts. It is written
//!   only by the generator and embedded at compile time.
//! - [`OverlayCatalog`]: the checked-in `catalog/overlay.toml`. It carries what
//!   models.dev does not know (endpoints, auth, per-model protocol) and pins
//!   facts that must not drift when the snapshot is refreshed.
//! - [`CatalogPatch`]: the optional runtime overlay. Every field is optional;
//!   a patch adds providers and models or overrides fields on existing entries.

use serde::{Deserialize, Serialize};

use keel_llm_types::ProtocolId;

use crate::{AuthSpec, Capabilities, CompatOverrides};

/// The only catalog schema version this build understands.
pub const CATALOG_VERSION: u32 = 1;

/// The generated models.dev facts, committed as `catalog/models.dev.toml`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub version: u32,
    /// Where the facts came from, for provenance.
    pub source: String,
    /// The UTC date the generator fetched them.
    pub fetched_at: String,
    pub providers: Vec<SnapshotProvider>,
}

/// One provider in a [`Snapshot`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotProvider {
    pub id: String,
    pub name: String,
    pub models: Vec<SnapshotModel>,
}

/// One model in a [`Snapshot`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotModel {
    pub id: String,
    pub name: String,
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub capabilities: SnapshotCapabilities,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deprecated: bool,
}

/// Facts models.dev states directly. `cache` is not here on purpose:
/// models.dev's `cost.cache_read` is pricing, not evidence that a provider
/// exposes the explicit cache controls keel's capability selects.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotCapabilities {
    pub tools: bool,
    pub images: bool,
    pub reasoning: bool,
}

impl Snapshot {
    pub fn provider(&self, id: &str) -> Option<&SnapshotProvider> {
        self.providers.iter().find(|provider| provider.id == id)
    }
}

impl SnapshotProvider {
    pub fn model(&self, id: &str) -> Option<&SnapshotModel> {
        self.models.iter().find(|model| model.id == id)
    }
}

/// The checked-in overlay, committed as `catalog/overlay.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayCatalog {
    pub version: u32,
    #[serde(default)]
    pub notes: Option<String>,
    pub providers: Vec<OverlayProvider>,
}

/// One provider in the checked-in overlay.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayProvider {
    pub id: String,
    /// Display name. Required unless the generated snapshot carries one.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub base_url_env: Option<String>,
    #[serde(default)]
    pub auth: Option<AuthSpec>,
    #[serde(default)]
    pub notes: Option<String>,
    /// The provider key in models.dev, when it differs from keel's id
    /// (`grok` reads from `xai`).
    #[serde(default)]
    pub models_dev_id: Option<String>,
    pub models: Vec<OverlayModel>,
}

/// One model in the checked-in overlay.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayModel {
    pub id: String,
    /// Display name. Required unless the generated snapshot carries one.
    #[serde(default)]
    pub name: Option<String>,
    /// models.dev does not carry this: every model names its protocol here.
    pub protocol: ProtocolId,
    /// The model key in models.dev, when it differs from keel's id.
    #[serde(default)]
    pub models_dev_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    /// A value here pins the field over the generated snapshot.
    #[serde(default)]
    pub context_window: Option<u32>,
    /// A value here pins the field over the generated snapshot.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Per-field overrides of the generated (or default) capabilities.
    #[serde(default)]
    pub capabilities: Option<CapabilitiesPatch>,
    #[serde(default)]
    pub compat: Option<CompatOverrides>,
    #[serde(default)]
    pub deprecated: Option<bool>,
}

/// A per-field capability patch. `None` means "leave the base value alone".
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesPatch {
    #[serde(default)]
    pub tools: Option<bool>,
    #[serde(default)]
    pub images: Option<bool>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    #[serde(default)]
    pub cache: Option<bool>,
}

impl CapabilitiesPatch {
    pub(crate) fn resolve(self, base: Capabilities) -> Capabilities {
        Capabilities {
            tools: self.tools.unwrap_or(base.tools),
            images: self.images.unwrap_or(base.images),
            reasoning: self.reasoning.unwrap_or(base.reasoning),
            cache: self.cache.unwrap_or(base.cache),
        }
    }
}

/// The optional runtime overlay. Unlike [`OverlayCatalog`], every field except
/// an entry's `id` is optional, and models may omit `protocol` when they patch
/// an existing model.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogPatch {
    #[serde(default)]
    pub version: Option<u32>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub providers: Vec<ProviderPatch>,
}

/// A provider entry in a runtime patch.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderPatch {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub base_url_env: Option<String>,
    #[serde(default)]
    pub auth: Option<AuthSpec>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelPatch>,
}

/// A model entry in a runtime patch.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPatch {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub protocol: Option<ProtocolId>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub capabilities: Option<CapabilitiesPatch>,
    #[serde(default)]
    pub compat: Option<CompatOverrides>,
    #[serde(default)]
    pub deprecated: Option<bool>,
}

fn is_false(value: &bool) -> bool {
    !value
}
