//! Developer entry point for regenerating the committed catalog snapshot.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use keel_catalog_gen::{DEFAULT_SOURCE, regenerate, today_utc};
use keel_llm_catalog::schema::OverlayCatalog;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("keel-catalog-gen: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let Some(args) = parse_args()? else {
        print_help();
        return Ok(());
    };

    let overlay_text = std::fs::read_to_string(&args.overlay)
        .map_err(|error| format!("read {}: {error}", args.overlay.display()))?;
    let overlay: OverlayCatalog = toml::from_str(&overlay_text)
        .map_err(|error| format!("parse {}: {error}", args.overlay.display()))?;

    let json = match &args.from {
        Some(path) => {
            std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?
        }
        None => fetch(&args.source)?,
    };
    let fetched_at = args.fetched_at.unwrap_or_else(today_utc);

    let report = regenerate(&json, &overlay, &args.output, &args.source, &fetched_at)
        .map_err(|error| error.to_string())?;

    println!("wrote {}", args.output.display());
    println!("providers: {}, models: {}", report.providers, report.models);
    for skipped in &report.skipped_providers {
        println!("skipped provider {skipped}");
    }
    for skipped in &report.skipped_models {
        println!("skipped model {skipped}");
    }
    for warning in &report.warnings {
        eprintln!("keel-catalog-gen: warning: {warning}");
    }
    Ok(())
}

struct Args {
    overlay: PathBuf,
    output: PathBuf,
    source: String,
    from: Option<PathBuf>,
    fetched_at: Option<String>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut args = Args {
        overlay: manifest.join("../keel-llm-catalog/catalog/overlay.toml"),
        output: manifest.join("../keel-llm-catalog/catalog/models.dev.toml"),
        source: DEFAULT_SOURCE.to_owned(),
        from: None,
        fetched_at: None,
    };

    let mut iter = std::env::args().skip(1);
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--overlay" => {
                args.overlay = PathBuf::from(iter.next().ok_or("--overlay needs a value")?);
            }
            "--output" => {
                args.output = PathBuf::from(iter.next().ok_or("--output needs a value")?);
            }
            "--source" => {
                args.source = iter.next().ok_or("--source needs a value")?;
            }
            "--from" => {
                args.from = Some(PathBuf::from(iter.next().ok_or("--from needs a value")?));
            }
            "--fetched-at" => {
                args.fetched_at = Some(iter.next().ok_or("--fetched-at needs a value")?);
            }
            "--help" | "-h" => return Ok(None),
            other => {
                return Err(format!(
                    "unknown argument `{other}`; run `keel-catalog-gen --help`"
                ));
            }
        }
    }
    Ok(Some(args))
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    if std::env::var_os("CI").is_some() {
        return Err(
            "refusing to fetch models.dev under CI; pass --from FILE with a local API dump"
                .to_owned(),
        );
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("build the HTTP runtime: {error}"))?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(format!("keel-catalog-gen/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| format!("build the HTTP client: {error}"))?;
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|error| format!("fetch {url}: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("fetch {url}: HTTP {status}"));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("read {url}: {error}"))?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(format!("fetch {url}: response is larger than 64 MiB"));
        }
        Ok(bytes.to_vec())
    })
}

fn print_help() {
    println!(
        "keel-catalog-gen — regenerate the committed models.dev snapshot

usage:
  keel-catalog-gen [--overlay PATH] [--output PATH]
                   [--from FILE] [--source URL] [--fetched-at YYYY-MM-DD]

The overlay lists the providers and model ids keel routes. The generator writes
only facts models.dev states (names, context/output limits, capabilities) and
atomically replaces the output file. Any fetch, parse or schema failure leaves
the previous last-known-good file untouched.

defaults:
  overlay    crates/keel-llm-catalog/catalog/overlay.toml
  output     crates/keel-llm-catalog/catalog/models.dev.toml
  source     {DEFAULT_SOURCE}
  fetched-at today (UTC)

Running without --from fetches models.dev and refuses under CI. --from FILE
reads a local API dump and never touches the network."
    );
}
