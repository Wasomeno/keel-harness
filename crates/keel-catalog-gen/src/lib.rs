//! Regenerates the committed models.dev catalog snapshot.
//!
//! This is a developer tool, not part of the runtime. It reads the checked-in
//! overlay to learn which providers and model ids keel routes, fetches
//! models.dev, and writes `catalog/models.dev.toml`. The write is atomic: any
//! fetch, parse or schema failure leaves the previous snapshot untouched, which
//! is the last-known-good file every build embeds.
//!
//! Tests never touch the network. They drive [`generate`] with a fixed JSON
//! fixture and [`write_atomically`] with a temporary directory.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use keel_llm_catalog::schema::{
    CATALOG_VERSION, OverlayCatalog, OverlayModel, OverlayProvider, Snapshot, SnapshotCapabilities,
    SnapshotModel, SnapshotProvider,
};
use serde::Deserialize;

/// The canonical models.dev API endpoint.
pub const DEFAULT_SOURCE: &str = "https://models.dev/api.json";

/// The comment block at the top of the committed snapshot.
pub const SNAPSHOT_HEADER: &str = "\
# Generated from models.dev by `cargo run -p keel-catalog-gen`.
# Do not edit by hand; regenerate instead. Fact provenance and the MIT license
# are in PROVENANCE.md next to this file.
";

#[derive(Debug, thiserror::Error)]
pub enum GenError {
    #[error("models.dev JSON parse failed: {0}")]
    Parse(String),
    #[error("models.dev schema error: {0}")]
    Schema(String),
    #[error("no catalog models matched; refusing to replace the last-known-good snapshot")]
    NoMatches,
    #[error("snapshot render failed: {0}")]
    Render(String),
    #[error("snapshot write failed: {0}")]
    Write(String),
}

/// What a regeneration found. The binary prints it; tests assert on it.
#[derive(Debug, Default)]
pub struct Report {
    pub providers: usize,
    pub models: usize,
    pub skipped_providers: Vec<String>,
    pub skipped_models: Vec<String>,
    pub warnings: Vec<String>,
}

/// A rendered snapshot and the report that goes with it.
#[derive(Debug)]
pub struct Generated {
    pub snapshot: Snapshot,
    pub report: Report,
}

/// Builds a snapshot from a models.dev API JSON document and the overlay.
///
/// Provider and model order follows the overlay, so repeated runs over the same
/// inputs produce the same file. Facts the overlay does not pin come from
/// models.dev; a mapped model that models.dev no longer carries is reported and
/// omitted, never guessed.
pub fn generate(
    json: &[u8],
    overlay: &OverlayCatalog,
    source: &str,
    fetched_at: &str,
) -> Result<Generated, GenError> {
    if overlay.version != CATALOG_VERSION {
        return Err(GenError::Schema(format!(
            "unsupported overlay version {}; this tool understands version {CATALOG_VERSION}",
            overlay.version
        )));
    }

    let api: BTreeMap<String, ApiProvider> =
        serde_json::from_slice(json).map_err(|error| GenError::Parse(error.to_string()))?;
    let mut report = Report::default();
    let mut snapshot = Snapshot {
        version: CATALOG_VERSION,
        source: source.to_owned(),
        fetched_at: fetched_at.to_owned(),
        providers: Vec::new(),
    };

    for provider in &overlay.providers {
        let Some(models_dev_id) = provider.models_dev_id.as_deref() else {
            report
                .skipped_providers
                .push(format!("{} (no models_dev_id)", provider.id));
            check_provider_facts(provider, &mut report);
            continue;
        };
        let Some(api_provider) = api.get(models_dev_id) else {
            report.skipped_providers.push(format!(
                "{} (models.dev has no provider `{models_dev_id}`)",
                provider.id
            ));
            check_provider_facts(provider, &mut report);
            continue;
        };
        let name = api_provider.name.clone().ok_or_else(|| {
            GenError::Schema(format!("models.dev provider `{models_dev_id}` has no name"))
        })?;
        let mut generated = SnapshotProvider {
            id: provider.id.clone(),
            name,
            models: Vec::new(),
        };
        for model in &provider.models {
            let source_id = model.models_dev_id.as_deref().unwrap_or(&model.id);
            let Some(api_model) = api_provider.models.get(source_id) else {
                report.skipped_models.push(format!(
                    "{}/{} (models.dev has no model `{source_id}`)",
                    provider.id, model.id
                ));
                check_model_facts(provider, model, &mut report);
                continue;
            };
            generated
                .models
                .push(snapshot_model(provider, model, source_id, api_model)?);
        }
        if generated.models.is_empty() {
            report
                .skipped_providers
                .push(format!("{} (no model ids matched)", provider.id));
        } else {
            report.providers += 1;
            report.models += generated.models.len();
            snapshot.providers.push(generated);
        }
    }

    if snapshot.providers.is_empty() && !overlay.providers.is_empty() {
        return Err(GenError::NoMatches);
    }

    Ok(Generated { snapshot, report })
}

fn snapshot_model(
    provider: &OverlayProvider,
    model: &OverlayModel,
    source_id: &str,
    api: &ApiModel,
) -> Result<SnapshotModel, GenError> {
    let name = api.name.clone().ok_or_else(|| {
        GenError::Schema(format!(
            "models.dev model `{models_dev_id}/{source_id}` has no name",
            models_dev_id = provider.models_dev_id.as_deref().unwrap_or(&provider.id)
        ))
    })?;
    let limit = api.limit.as_ref();
    let context = limit.and_then(|limit| limit.context).ok_or_else(|| {
        GenError::Schema(format!(
            "models.dev model `{models_dev_id}/{source_id}` has no limit.context",
            models_dev_id = provider.models_dev_id.as_deref().unwrap_or(&provider.id)
        ))
    })?;
    let context_window = u32::try_from(context).map_err(|_| {
        GenError::Schema(format!(
            "models.dev model `{models_dev_id}/{source_id}` reports context {context}, which does not fit u32",
            models_dev_id = provider.models_dev_id.as_deref().unwrap_or(&provider.id)
        ))
    })?;
    let output = limit.and_then(|limit| limit.output).unwrap_or(0);
    let max_output_tokens = u32::try_from(output).map_err(|_| {
        GenError::Schema(format!(
            "models.dev model `{models_dev_id}/{source_id}` reports output {output}, which does not fit u32",
            models_dev_id = provider.models_dev_id.as_deref().unwrap_or(&provider.id)
        ))
    })?;

    let images = api.attachment.unwrap_or(false)
        || api
            .modalities
            .as_ref()
            .is_some_and(|modalities| modalities.input.iter().any(|input| input == "image"));

    Ok(SnapshotModel {
        id: model.id.clone(),
        name,
        context_window,
        max_output_tokens,
        capabilities: SnapshotCapabilities {
            tools: api.tool_call.unwrap_or(false),
            images,
            reasoning: api.reasoning.unwrap_or(false),
        },
        deprecated: api.status.as_deref() == Some("deprecated"),
    })
}

fn check_provider_facts(provider: &OverlayProvider, report: &mut Report) {
    for model in &provider.models {
        check_model_facts(provider, model, report);
    }
}

fn check_model_facts(provider: &OverlayProvider, model: &OverlayModel, report: &mut Report) {
    let missing = model.name.is_none()
        || model.context_window.is_none()
        || model.max_output_tokens.is_none()
        || model.capabilities.is_none();
    if missing {
        report.warnings.push(format!(
            "model `{}/{}` is not in the snapshot and the overlay does not pin name, context_window, max_output_tokens and capabilities",
            provider.id, model.id
        ));
    }
}

/// Renders a snapshot as the committed TOML document.
pub fn render(snapshot: &Snapshot) -> Result<String, GenError> {
    let body =
        toml::to_string_pretty(snapshot).map_err(|error| GenError::Render(error.to_string()))?;
    Ok(format!("{SNAPSHOT_HEADER}\n{body}"))
}

/// Runs generation and, only if every step succeeded, atomically replaces
/// `output`. A failure at any point leaves the previous file byte-for-byte
/// untouched.
pub fn regenerate(
    json: &[u8],
    overlay: &OverlayCatalog,
    output: &Path,
    source: &str,
    fetched_at: &str,
) -> Result<Report, GenError> {
    let generated = generate(json, overlay, source, fetched_at)?;
    let text = render(&generated.snapshot)?;
    write_atomically(output, &text)?;
    Ok(generated.report)
}

/// Writes through a temporary file in the destination directory and renames it
/// into place. The temporary file is removed on any failure.
pub fn write_atomically(path: &Path, contents: &str) -> Result<(), GenError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| GenError::Write(format!("create {}: {error}", parent.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| GenError::Write(format!("`{}` has no file name", path.display())))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));

    let mut file = match fs::File::create(&temporary) {
        Ok(file) => file,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(GenError::Write(format!(
                "create {}: {error}",
                temporary.display()
            )));
        }
    };
    if let Err(error) = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
    {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(GenError::Write(format!(
            "write {}: {error}",
            temporary.display()
        )));
    }
    drop(file);

    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(GenError::Write(format!(
            "replace {}: {error}",
            path.display()
        )));
    }
    sync_directory(parent);
    Ok(())
}

#[cfg(unix)]
fn sync_directory(directory: &Path) {
    if let Ok(file) = fs::File::open(directory) {
        let _ = file.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) {}

/// The current UTC date, used as the snapshot's `fetched_at` stamp.
pub fn today_utc() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    utc_date_from_unix(seconds)
}

fn utc_date_from_unix(seconds: u64) -> String {
    // Howard Hinnant's civil-from-days algorithm. Avoids a date dependency in a
    // tool that is not on the runtime path.
    let days = (seconds / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

#[derive(Debug, Deserialize)]
struct ApiProvider {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, ApiModel>,
}

#[derive(Debug, Deserialize)]
struct ApiModel {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    attachment: Option<bool>,
    #[serde(default)]
    reasoning: Option<bool>,
    #[serde(default)]
    tool_call: Option<bool>,
    #[serde(default)]
    modalities: Option<ApiModalities>,
    #[serde(default)]
    limit: Option<ApiLimit>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiModalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ApiLimit {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const API: &str = r#"{
        "openai": {
            "id": "openai",
            "name": "OpenAI",
            "models": {
                "gpt-4.1": {
                    "id": "gpt-4.1",
                    "name": "GPT-4.1",
                    "attachment": true,
                    "reasoning": false,
                    "tool_call": true,
                    "modalities": {"input": ["text", "image"]},
                    "limit": {"context": 1047576, "output": 32768},
                    "status": "deprecated",
                    "cost": {"input": 2, "output": 8, "cache_read": 0.5}
                }
            }
        },
        "xai": {
            "id": "xai",
            "name": "xAI",
            "models": {
                "grok-4": {
                    "id": "grok-4",
                    "name": "Grok 4",
                    "attachment": true,
                    "reasoning": true,
                    "tool_call": true,
                    "limit": {"context": 256000, "output": 32768}
                }
            }
        }
    }"#;

    fn overlay() -> OverlayCatalog {
        toml::from_str(
            r#"
version = 1
[[providers]]
id = "openai"
name = "OpenAI"
base_url = "https://api.openai.com/v1"
models_dev_id = "openai"
[[providers.models]]
id = "gpt-4.1"
name = "GPT-4.1"
protocol = "openai-chat"

[[providers]]
id = "grok"
name = "Grok"
base_url = "https://api.x.ai/v1"
models_dev_id = "xai"
[[providers.models]]
id = "grok-4"
name = "Grok 4"
protocol = "openai-chat"
context_window = 1
max_output_tokens = 1
[providers.models.capabilities]
tools = true
images = true
reasoning = true
cache = false
"#,
        )
        .expect("overlay fixture")
    }

    #[test]
    fn generation_follows_the_overlay_and_keeps_generated_facts() {
        let generated = generate(API.as_bytes(), &overlay(), "test", "2026-11-19").unwrap();
        assert_eq!(generated.report.providers, 2);
        assert_eq!(generated.report.models, 2);
        assert!(generated.report.skipped_models.is_empty());

        let openai = generated.snapshot.provider("openai").expect("openai");
        let model = openai.model("gpt-4.1").expect("gpt-4.1");
        assert_eq!(model.name, "GPT-4.1");
        assert_eq!(model.context_window, 1047576);
        assert_eq!(model.max_output_tokens, 32768);
        assert!(model.capabilities.tools);
        assert!(model.capabilities.images);
        assert!(!model.capabilities.reasoning);
        assert!(model.deprecated);

        let grok = generated.snapshot.provider("grok").expect("grok");
        assert_eq!(grok.models[0].id, "grok-4");
        assert_eq!(grok.models[0].context_window, 256000);
        assert!(grok.models[0].capabilities.reasoning);
    }

    #[test]
    fn a_model_alias_keeps_the_keel_id_and_takes_the_source_facts() {
        let overlay: OverlayCatalog = toml::from_str(
            r#"
version = 1
[[providers]]
id = "gateway"
name = "Gateway"
base_url = "https://gateway.test/v1"
models_dev_id = "openai"
[[providers.models]]
id = "keel-name"
models_dev_id = "gpt-4.1"
name = "Keel name"
protocol = "openai-chat"
"#,
        )
        .unwrap();

        let generated = generate(API.as_bytes(), &overlay, "test", "2026-11-19").unwrap();
        let model = generated
            .snapshot
            .provider("gateway")
            .and_then(|provider| provider.model("keel-name"))
            .expect("alias generated");
        assert_eq!(model.context_window, 1047576);
        assert_eq!(model.max_output_tokens, 32768);
    }

    #[test]
    fn a_model_missing_from_models_dev_is_reported_and_omitted() {
        let overlay: OverlayCatalog = toml::from_str(
            r#"
version = 1
[[providers]]
id = "openai"
name = "OpenAI"
base_url = "https://api.openai.com/v1"
models_dev_id = "openai"
[[providers.models]]
id = "gpt-4.1"
name = "GPT-4.1"
protocol = "openai-chat"
[[providers.models]]
id = "gone"
name = "Gone"
protocol = "openai-chat"
context_window = 1
max_output_tokens = 1
[providers.models.capabilities]
tools = true
images = true
reasoning = false
cache = false
"#,
        )
        .unwrap();

        let generated = generate(API.as_bytes(), &overlay, "test", "2026-11-19").unwrap();
        assert_eq!(generated.report.models, 1);
        assert_eq!(generated.report.skipped_models.len(), 1);
        assert!(generated.report.skipped_models[0].contains("openai/gone"));
        assert!(generated.report.warnings.is_empty());
        assert!(
            generated
                .snapshot
                .provider("openai")
                .unwrap()
                .model("gone")
                .is_none()
        );
    }

    #[test]
    fn a_skipped_model_without_pinned_facts_is_a_warning() {
        let overlay: OverlayCatalog = toml::from_str(
            r#"
version = 1
[[providers]]
id = "openai"
name = "OpenAI"
base_url = "https://api.openai.com/v1"
models_dev_id = "openai"
[[providers.models]]
id = "gpt-4.1"
name = "GPT-4.1"
protocol = "openai-chat"
[[providers.models]]
id = "gone"
name = "Gone"
protocol = "openai-chat"
"#,
        )
        .unwrap();

        let generated = generate(API.as_bytes(), &overlay, "test", "2026-11-19").unwrap();
        assert_eq!(generated.report.warnings.len(), 1);
        assert!(generated.report.warnings[0].contains("openai/gone"));
    }

    #[test]
    fn a_provider_missing_from_models_dev_is_reported() {
        let overlay: OverlayCatalog = toml::from_str(
            r#"
version = 1
[[providers]]
id = "offline"
name = "Offline"
base_url = "https://offline.test/v1"
[[providers.models]]
id = "model"
name = "Model"
protocol = "openai-chat"
context_window = 1
max_output_tokens = 1
[providers.models.capabilities]
tools = true
images = true
reasoning = false
cache = false
"#,
        )
        .unwrap();

        assert!(matches!(
            generate(API.as_bytes(), &overlay, "test", "2026-11-19"),
            Err(GenError::NoMatches)
        ));
    }

    #[test]
    fn a_context_that_does_not_fit_u32_is_rejected() {
        let api = API.replace("1047576", "4294967296");
        assert!(matches!(
            generate(api.as_bytes(), &overlay(), "test", "2026-11-19"),
            Err(GenError::Schema(_))
        ));
    }

    #[test]
    fn a_failed_regeneration_leaves_the_previous_snapshot_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("models.dev.toml");
        fs::write(&output, "previous").unwrap();

        let error =
            regenerate(b"{ not json", &overlay(), &output, "test", "2026-11-19").unwrap_err();
        assert!(matches!(error, GenError::Parse(_)));
        assert_eq!(fs::read_to_string(&output).unwrap(), "previous");
    }

    #[test]
    fn a_successful_regeneration_replaces_the_snapshot_and_leaves_no_temp_file() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("models.dev.toml");
        fs::write(&output, "previous").unwrap();

        let report = regenerate(
            API.as_bytes(),
            &overlay(),
            &output,
            "test-source",
            "2026-11-19",
        )
        .unwrap();
        assert_eq!(report.providers, 2);

        let text = fs::read_to_string(&output).unwrap();
        assert!(text.starts_with(SNAPSHOT_HEADER));
        let snapshot: Snapshot = toml::from_str(&text).unwrap();
        assert_eq!(snapshot.source, "test-source");
        assert_eq!(snapshot.fetched_at, "2026-11-19");
        assert_eq!(snapshot.providers.len(), 2);

        let leftovers = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn a_context_overflow_failure_leaves_the_previous_snapshot_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("models.dev.toml");
        fs::write(&output, "previous").unwrap();
        let api = API.replace("1047576", "4294967296");

        let error =
            regenerate(api.as_bytes(), &overlay(), &output, "test", "2026-11-19").unwrap_err();
        assert!(matches!(error, GenError::Schema(_)));
        assert_eq!(fs::read_to_string(&output).unwrap(), "previous");
    }

    #[test]
    fn the_snapshot_renders_deterministically() {
        let first = generate(API.as_bytes(), &overlay(), "test", "2026-11-19").unwrap();
        let second = generate(API.as_bytes(), &overlay(), "test", "2026-11-19").unwrap();
        assert_eq!(
            render(&first.snapshot).unwrap(),
            render(&second.snapshot).unwrap()
        );
    }

    #[test]
    fn utc_dates_are_derived_without_a_date_dependency() {
        assert_eq!(utc_date_from_unix(0), "1970-01-01");
        assert_eq!(utc_date_from_unix(1_700_000_000), "2023-11-14");
        assert_eq!(utc_date_from_unix(1_763_510_400), "2025-11-19");
    }
}
