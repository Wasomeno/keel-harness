//! `keel auth` and the store-backed chat path.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use keel_testkit::{Chunk, FauxProvider, Turn};

fn keel(credentials: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_keel"));
    command.env("KEEL_CREDENTIALS", credentials);
    command
}

fn run(mut command: Command, input: &str) -> Output {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn keel");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for keel")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn auth_set_list_and_remove_round_trip_without_exposing_the_secret() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("private/credentials.json");
    let secret = "sk-test-cli-0123456789";

    let output = run(
        {
            let mut command = keel(&path);
            command.args(["auth", "set", "openai"]);
            command
        },
        &format!("{secret}\n"),
    );
    assert!(output.status.success(), "{}", text(&output));
    assert!(text(&output).contains("stored credential `openai`"));
    assert!(!text(&output).contains(secret), "{}", text(&output));

    let output = run(
        {
            let mut command = keel(&path);
            command.args(["auth", "list"]);
            command
        },
        "",
    );
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "openai\n");
    assert!(!text(&output).contains(secret), "{}", text(&output));

    let output = run(
        {
            let mut command = keel(&path);
            command.args(["auth", "set", "anthropic.work"]);
            command
        },
        "sk-test-cli-anthropic\n",
    );
    assert!(output.status.success(), "{}", text(&output));

    let output = run(
        {
            let mut command = keel(&path);
            command.args(["auth", "list"]);
            command
        },
        "",
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "anthropic.work\nopenai\n"
    );

    let output = run(
        {
            let mut command = keel(&path);
            command.args(["auth", "remove", "openai"]);
            command
        },
        "",
    );
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "removed credential `openai`\n"
    );

    let output = run(
        {
            let mut command = keel(&path);
            command.args(["auth", "list"]);
            command
        },
        "",
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "anthropic.work\n");

    let file = fs::read_to_string(&path).expect("the credential file");
    assert!(!file.contains(secret), "the removed secret is gone");
    assert!(file.contains("sk-test-cli-anthropic"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let parent = fs::metadata(path.parent().unwrap())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(parent, 0o700);
    }
}

#[test]
fn auth_set_refuses_a_secret_in_argv() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("credentials.json");
    let mut command = keel(&path);
    command.args(["auth", "set", "openai", "sk-test-cli-leaked"]);
    let output = run(command, "");
    assert!(!output.status.success());
    assert!(text(&output).contains("do not pass it on the command line"));
    assert!(!path.exists(), "nothing was written");
}

#[test]
fn auth_remove_of_an_unknown_key_is_reported_but_succeeds() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("credentials.json");
    let mut command = keel(&path);
    command.args(["auth", "remove", "openai"]);
    let output = run(command, "");
    assert!(output.status.success(), "{}", text(&output));
    assert!(text(&output).contains("was not set"));
}

#[tokio::test]
async fn chat_uses_a_stored_provider_key() {
    let provider = FauxProvider::start(vec![Turn::sse(vec![
        Chunk::text(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"stored hello\"},\"finish_reason\":\"stop\"}]}\n\n",
        ),
        Chunk::text("data: [DONE]\n\n"),
    ])])
    .await;

    let credentials = tempfile::tempdir().expect("credentials dir");
    let path = credentials.path().join("credentials.json");
    fs::write(
        &path,
        r#"{"version":1,"credentials":{"neuralwatt":"sk-test-cli-stored"}}"#,
    )
    .expect("seed the store");
    let data = tempfile::tempdir().expect("data dir");
    let workspace = tempfile::tempdir().expect("workspace dir");

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_keel"))
        .args([
            "chat",
            "neuralwatt/deepseek-v4.1-flash",
            "--prompt",
            "say hello",
            "--yes",
            "--workspace",
            workspace.path().to_str().expect("workspace path"),
        ])
        .env("KEEL_NEURALWATT_BASE_URL", provider.url())
        .env("KEEL_CREDENTIALS", &path)
        .env("KEEL_DATA_DIR", data.path())
        .env_remove("NEURALWATT_API_KEY")
        .output()
        .await
        .expect("run keel chat");

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{text}");
    assert!(text.contains("stored hello"), "{text}");
    assert!(!text.contains("sk-test-cli-stored"), "{text}");

    let requests = provider.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer sk-test-cli-stored")
    );
}
