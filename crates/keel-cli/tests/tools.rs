//! `keel tools` must not execute workspace MCP config without an opt-in.

use std::path::{Path, PathBuf};

use tokio::process::Command;

fn write_config(directory: &Path, body: &str) {
    std::fs::create_dir(directory.join(".keel")).unwrap();
    std::fs::write(directory.join(".keel/mcp.toml"), body).unwrap();
}

fn toml_string(value: &Path) -> String {
    format!("{:?}", value.display().to_string())
}

#[tokio::test]
async fn tools_without_mcp_does_not_start_configured_servers() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("started");
    write_config(
        directory.path(),
        &format!(
            "[servers.fixture]\ncommand = \"touch\"\nargs = [{}]\n",
            toml_string(&marker)
        ),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_keel"))
        .args(["tools", "--workspace", directory.path().to_str().unwrap()])
        .env_remove("KEEL_MCP")
        .output()
        .await
        .expect("keel tools runs");

    assert!(
        output.status.success(),
        "keel tools failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !marker.exists(),
        "the configured MCP command ran without --mcp"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("configured but not started"),
        "the opt-in warning is missing:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("fixture__"),
        "MCP tools were listed without --mcp:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
async fn tools_without_mcp_does_not_contact_http_servers() {
    let directory = tempfile::tempdir().unwrap();
    write_config(
        directory.path(),
        "[servers.remote]\nurl = \"http://127.0.0.1:9/mcp\"\n",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_keel"))
        .args(["tools", "--workspace", directory.path().to_str().unwrap()])
        .env_remove("KEEL_MCP")
        .output()
        .await
        .expect("keel tools runs");

    assert!(
        output.status.success(),
        "keel tools failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("configured but not started"),
        "the opt-in warning is missing:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("remote__"),
        "an HTTP MCP server contributed tools without --mcp:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
async fn tools_without_mcp_reports_config_warnings() {
    let directory = tempfile::tempdir().unwrap();
    write_config(directory.path(), "[servers.fixture]\ncommand = 7\n");

    let output = Command::new(env!("CARGO_BIN_EXE_keel"))
        .args(["tools", "--workspace", directory.path().to_str().unwrap()])
        .env_remove("KEEL_MCP")
        .output()
        .await
        .expect("keel tools runs");

    assert!(
        output.status.success(),
        "keel tools failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("MCP server `fixture` was skipped"),
        "the config warning is missing:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("fixture__"),
        "a broken config contributed a tool:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
async fn keel_mcp_env_is_the_scripted_opt_in() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("started");
    write_config(
        directory.path(),
        &format!(
            "[servers.fixture]\ncommand = \"touch\"\nargs = [{}]\n",
            toml_string(&marker)
        ),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_keel"))
        .args(["tools", "--workspace", directory.path().to_str().unwrap()])
        .env("KEEL_MCP", "1")
        .output()
        .await
        .expect("keel tools runs");

    assert!(
        output.status.success(),
        "keel tools failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        marker.exists(),
        "KEEL_MCP=1 did not start the configured command"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("did not start"),
        "the failed-server warning is missing:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn tools_with_mcp_reports_a_broken_server() {
    let directory = tempfile::tempdir().unwrap();
    write_config(
        directory.path(),
        "[servers.fixture]\ncommand = \"/definitely/not/a/server\"\n",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_keel"))
        .args([
            "tools",
            "--mcp",
            "--workspace",
            directory.path().to_str().unwrap(),
        ])
        .env_remove("KEEL_MCP")
        .output()
        .await
        .expect("keel tools runs");

    assert!(
        output.status.success(),
        "keel tools failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("did not start"),
        "the broken-server warning is missing:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("fixture__"),
        "a broken server contributed a tool:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
async fn tools_with_mcp_lists_fixture_tools() {
    let Some(fixture) = fixture_binary() else {
        eprintln!("skipping: mcp_fixture was not built for this test run");
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    write_config(
        directory.path(),
        &format!("[servers.fixture]\ncommand = {}\n", toml_string(&fixture)),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_keel"))
        .args([
            "tools",
            "--mcp",
            "--workspace",
            directory.path().to_str().unwrap(),
        ])
        .env_remove("KEEL_MCP")
        .output()
        .await
        .expect("keel tools runs");

    assert!(
        output.status.success(),
        "keel tools failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fixture__echo"), "{stdout}");
    assert!(stdout.contains("mcp"), "{stdout}");
}

fn fixture_binary() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let candidate = executable.parent()?.parent()?.join("mcp_fixture");
    candidate.is_file().then_some(candidate)
}
