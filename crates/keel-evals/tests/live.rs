//! The live recording run. Ignored by default; refused under `CI`.
//!
//! ```text
//! KEEL_EVALS_LIVE=1 KEEL_CASSETTE_RECORD=1 \
//!   cargo test -p keel-evals --test live -- --ignored --nocapture
//! ```
//!
//! The key comes from `NEURALWATT_API_KEY`, `KEEL_RECORD_KEY_FILE`, or the
//! recorder's usual `~/.config/neuralwatt/key`. It is never printed.

use std::path::PathBuf;

use keel_evals::TASKS;
use keel_evals::fixture::{cassettes_dir, reports_dir};
use keel_evals::live::{LIVE_ENV, key_available, live_allowed, record_all, resolve_key_file};

#[tokio::test]
#[ignore = "records live NeuralWatt exchanges; needs KEEL_EVALS_LIVE=1, KEEL_CASSETTE_RECORD=1 and a key"]
async fn record_live_eval_run() {
    let opt_in = std::env::var(LIVE_ENV).as_deref() == Ok("1");
    if let Err(reason) = live_allowed(opt_in, std::env::var_os("CI").is_some()) {
        panic!("{reason}");
    }
    assert_eq!(
        std::env::var("KEEL_CASSETTE_RECORD").as_deref(),
        Ok("1"),
        "the recorder needs KEEL_CASSETTE_RECORD=1 for a real origin"
    );

    let key_file: Option<PathBuf> = resolve_key_file(None);
    assert!(
        key_available(key_file.as_deref()),
        "no NeuralWatt key; set NEURALWATT_API_KEY or KEEL_RECORD_KEY_FILE"
    );

    let report = record_all(TASKS, key_file, cassettes_dir())
        .await
        .expect("the live run completes");
    let markdown = reports_dir().join("live.md");
    let json = reports_dir().join("live.json");
    report.write_markdown(&markdown).expect("report writes");
    report.write_json(&json).expect("json report writes");

    eprintln!("live eval report: {}", markdown.display());
    eprintln!(
        "spent ${:.6} of ${:.6}",
        report.spent_micros.unwrap_or(0) as f64 / 1_000_000.0,
        report.cap_micros.unwrap_or(0) as f64 / 1_000_000.0
    );
    for task in &report.tasks {
        eprintln!(
            "  {:<24} {:<20} turns={} tools={} in={} out={} stop={:?}",
            task.id,
            task.status.label(),
            task.turns,
            task.tool_calls,
            task.usage.input_tokens,
            task.usage.output_tokens,
            task.stop
        );
    }

    assert_eq!(
        report.passed(),
        TASKS.len(),
        "every task must pass; see {}",
        markdown.display()
    );
}
