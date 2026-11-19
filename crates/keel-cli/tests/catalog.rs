//! `keel models` and `keel providers` are stable surfaces, and the runtime
//! catalog overlay applies without a rebuild without ever turning a bad file
//! into a failed command.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn keel(overlay: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_keel"));
    command.env("KEEL_CATALOG", overlay);
    command
}

fn overlay_file(body: &str) -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("catalog.toml");
    std::fs::write(&path, body).expect("write overlay");
    (directory, path)
}

fn run(command: &mut Command) -> Output {
    command.output().expect("run keel")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn models_output_is_stable() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let missing = directory.path().join("absent.toml");

    let output = run(keel(&missing).arg("models"));
    assert!(
        output.status.success(),
        "keel models failed:\n{}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), include_str!("fixtures/models.txt"));
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
}

#[test]
fn providers_output_is_stable() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let missing = directory.path().join("absent.toml");

    let output = run(keel(&missing).arg("providers"));
    assert!(
        output.status.success(),
        "keel providers failed:\n{}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), include_str!("fixtures/providers.txt"));
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
}

#[test]
fn a_runtime_overlay_adds_a_provider_and_model_to_models() {
    let (_directory, overlay) = overlay_file(
        r#"
version = 1
[[providers]]
id = "local"
name = "Local"
base_url = "http://127.0.0.1:9/v1"
[[providers.models]]
id = "tiny"
name = "Tiny"
protocol = "openai-chat"
context_window = 4096
max_output_tokens = 512
[providers.models.capabilities]
images = false
"#,
    );

    let output = run(keel(&overlay).args(["models", "local"]));
    assert!(
        output.status.success(),
        "keel models failed:\n{}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(text.contains("local/tiny"), "{text}");
    assert!(text.contains("4096"), "{text}");
    assert!(text.contains("512"), "{text}");
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
}

#[test]
fn a_runtime_overlay_provider_routes_without_a_rebuild() {
    let (_directory, overlay) = overlay_file(
        r#"
version = 1
[[providers]]
id = "local"
name = "Local"
base_url = "http://127.0.0.1:9/v1"
[[providers.models]]
id = "tiny"
name = "Tiny"
protocol = "openai-chat"
context_window = 4096
max_output_tokens = 512
"#,
    );

    let output = run(keel(&overlay).args(["route", "local/tiny"]));
    assert!(
        output.status.success(),
        "keel route failed:\n{}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(text.contains("provider   local"), "{text}");
    assert!(
        text.contains("endpoint   http://127.0.0.1:9/v1/chat/completions"),
        "{text}"
    );
    assert!(
        text.contains("window     4096 context, 512 output"),
        "{text}"
    );
}

#[test]
fn a_malformed_runtime_overlay_warns_and_lists_the_embedded_catalog() {
    let (_directory, overlay) = overlay_file("this is not toml");

    let output = run(keel(&overlay).arg("models"));
    assert!(
        output.status.success(),
        "keel models failed:\n{}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), include_str!("fixtures/models.txt"));
    let warnings = stderr(&output);
    assert!(
        warnings.contains("using the embedded catalog"),
        "missing fallback warning:\n{warnings}"
    );
}

#[test]
fn an_unknown_provider_filter_is_an_error() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let missing = directory.path().join("absent.toml");

    let output = run(keel(&missing).args(["models", "nope"]));
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("unknown provider `nope`"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_unknown_model_is_an_error() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let missing = directory.path().join("absent.toml");

    let output = run(keel(&missing).args(["route", "openai/nope"]));
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("provider `openai` has no model `nope`"),
        "{}",
        stderr(&output)
    );
}
