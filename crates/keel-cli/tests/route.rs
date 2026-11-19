//! `keel route` reports the catalog context window and output limit that the
//! compaction trigger reads.

use tokio::process::Command;

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
