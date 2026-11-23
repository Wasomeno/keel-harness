//! `keel tools` must not execute workspace MCP config without an opt-in.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::process::Command;

fn write_config(directory: &Path, body: &str) {
    std::fs::create_dir(directory.join(".keel")).unwrap();
    std::fs::write(directory.join(".keel/mcp.toml"), body).unwrap();
}

fn toml_string(value: &Path) -> String {
    format!("{:?}", value.display().to_string())
}

/// A config whose command creates `marker` and then exits without speaking
/// MCP, so the test can tell that keel really spawned it. `touch` is a Unix
/// utility; `cmd /C copy NUL` is the Windows equivalent.
fn marker_config(marker: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "[servers.fixture]\ncommand = \"cmd\"\nargs = [\"/C\", \"copy\", \"NUL\", {}]\n",
            toml_string(marker)
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "[servers.fixture]\ncommand = \"touch\"\nargs = [{}]\n",
            toml_string(marker)
        )
    }
}

#[tokio::test]
async fn tools_without_mcp_does_not_start_configured_servers() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("started");
    write_config(directory.path(), &marker_config(&marker));

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
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    write_config(
        directory.path(),
        &format!("[servers.remote]\nurl = \"http://127.0.0.1:{port}/mcp\"\n"),
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
    assert!(
        tokio::time::timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_err(),
        "keel tools connected to an HTTP MCP server without --mcp"
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
    write_config(directory.path(), &marker_config(&marker));

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

fn listed_tools(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|line| line.starts_with(|c: char| c.is_ascii_lowercase()))
        .filter_map(|line| line.split_whitespace().next())
        .collect()
}

#[tokio::test]
async fn tools_lists_the_eight_builtins_and_jobs_only_when_opted_in() {
    let directory = tempfile::tempdir().unwrap();
    let default = Command::new(env!("CARGO_BIN_EXE_keel"))
        .args(["tools", "--workspace", directory.path().to_str().unwrap()])
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env_remove("KEEL_MCP")
        .env_remove("KEEL_JOBS")
        .output()
        .await
        .expect("keel tools runs");
    assert!(default.status.success());
    let stdout = String::from_utf8_lossy(&default.stdout);
    assert_eq!(
        listed_tools(&stdout),
        vec![
            "apply_patch",
            "bash",
            "edit",
            "glob",
            "read",
            "search",
            "view_image",
            "write"
        ],
        "the default listing must be exactly the eight built-in tools:\n{stdout}"
    );
    assert!(
        !stdout
            .lines()
            .any(|line| line.starts_with("terminal") && line.as_bytes().get(8) == Some(&b' ')),
        "the removed terminal tool appeared without an opt-in:\n{stdout}"
    );
    assert!(
        !stdout
            .lines()
            .any(|line| line.starts_with("jobs") && line.as_bytes().get(4) == Some(&b' ')),
        "the jobs tool appeared without an opt-in:\n{stdout}"
    );

    #[cfg(unix)]
    {
        let opted = Command::new(env!("CARGO_BIN_EXE_keel"))
            .args([
                "tools",
                "--jobs",
                "--workspace",
                directory.path().to_str().unwrap(),
            ])
            .env("HOME", directory.path())
            .env("USERPROFILE", directory.path())
            .env_remove("KEEL_MCP")
            .env_remove("KEEL_JOBS")
            .output()
            .await
            .expect("keel tools runs");
        assert!(
            opted.status.success(),
            "keel tools --jobs failed:\n{}",
            String::from_utf8_lossy(&opted.stderr)
        );
        let stdout = String::from_utf8_lossy(&opted.stdout);
        assert_eq!(
            listed_tools(&stdout),
            vec![
                "apply_patch",
                "bash",
                "edit",
                "glob",
                "jobs",
                "read",
                "search",
                "view_image",
                "write"
            ],
            "the jobs opt-in added the wrong tools:\n{stdout}"
        );
        assert!(stdout.contains("Unix-only"), "{stdout}");

        let from_env = Command::new(env!("CARGO_BIN_EXE_keel"))
            .args(["tools", "--workspace", directory.path().to_str().unwrap()])
            .env("HOME", directory.path())
            .env("USERPROFILE", directory.path())
            .env_remove("KEEL_MCP")
            .env("KEEL_JOBS", "1")
            .output()
            .await
            .expect("keel tools runs");
        assert!(
            from_env.status.success(),
            "keel tools with KEEL_JOBS=1 failed:\n{}",
            String::from_utf8_lossy(&from_env.stderr)
        );
        assert!(
            listed_tools(&String::from_utf8_lossy(&from_env.stdout)).contains(&"jobs"),
            "KEEL_JOBS=1 did not register jobs:\n{}",
            String::from_utf8_lossy(&from_env.stdout)
        );
    }

    #[cfg(not(unix))]
    {
        let refused = Command::new(env!("CARGO_BIN_EXE_keel"))
            .args([
                "tools",
                "--jobs",
                "--workspace",
                directory.path().to_str().unwrap(),
            ])
            .env("HOME", directory.path())
            .env("USERPROFILE", directory.path())
            .env_remove("KEEL_MCP")
            .env_remove("KEEL_JOBS")
            .output()
            .await
            .expect("keel tools runs");
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("only supported on Unix"),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }
}

fn fixture_binary() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let candidate = executable
        .parent()?
        .parent()?
        .join(format!("mcp_fixture{}", std::env::consts::EXE_SUFFIX));
    candidate.is_file().then_some(candidate)
}
