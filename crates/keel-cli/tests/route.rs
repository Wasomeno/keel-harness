//! `keel route` reports the catalog context window and output limit that the
//! compaction trigger reads.

use tokio::process::Command;

#[tokio::test]
async fn route_shows_configured_mcp_servers() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join(".keel")).unwrap();
    std::fs::write(
        directory.path().join(".keel/mcp.toml"),
        "[servers.fixture]\ncommand = \"true\"\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_keel"))
        .args([
            "route",
            "neuralwatt/deepseek-v4.1-flash",
            "--workspace",
            directory.path().to_str().unwrap(),
        ])
        .output()
        .await
        .expect("keel route runs");

    assert!(
        output.status.success(),
        "keel route failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("mcp        fixture"),
        "the configured server is missing:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
async fn route_prints_the_catalog_window_and_output_limit() {
    let output = Command::new(env!("CARGO_BIN_EXE_keel"))
        .args(["route", "neuralwatt/deepseek-v4.1-flash"])
        .output()
        .await
        .expect("keel route runs");

    assert!(
        output.status.success(),
        "keel route failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("window") && stdout.contains("1048560 context"),
        "the window line is missing:\n{stdout}"
    );
    assert!(
        stdout.contains("393216 output"),
        "the output limit is missing:\n{stdout}"
    );
}
