// Regression: QA-L01 — independent CLI and Runtime releases looked incompatible.
// Found by /qa on 2026-09-01.
// Report: .gstack/qa-reports/qa-report-reporch-cli-rc9-luna10-2026-09-01.md

use std::process::Command;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

#[test]
fn json_status_explains_independent_versions_and_protocol_compatibility() {
    let temporary = tempfile::tempdir().unwrap();
    let output = reporch()
        .env("REPORCH_RUNTIME_HOME", temporary.path())
        .args(["--format", "json", "runtime", "status"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let data = &result["data"];
    assert_eq!(data["cli_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(data["runtime_version_is_independent"], true);
    assert_eq!(data["compatibility_basis"], "protocol_version");
    assert_eq!(data["protocol_compatible"], true);
}

#[test]
fn human_status_explains_that_runtime_and_cli_versions_are_independent() {
    let temporary = tempfile::tempdir().unwrap();
    let output = reporch()
        .env("REPORCH_RUNTIME_HOME", temporary.path())
        .args(["runtime", "status"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Runtime versions are independent from CLI"));
    assert!(stdout.contains("protocol compatibility governs execution"));
}
