// Regression: QA-L02 — runtime doctor returned success for an unhealthy runtime.
// Found by /qa on 2026-09-01.
// Report: .gstack/qa-reports/qa-report-reporch-cli-rc9-luna10-2026-09-01.md

use std::process::Command;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

#[test]
fn unhealthy_runtime_doctor_is_a_single_domain_failure_envelope() {
    let temporary = tempfile::tempdir().unwrap();
    let runtime_home = temporary.path().join("runtime");
    let output = reporch()
        .env("REPORCH_RUNTIME_HOME", &runtime_home)
        .args(["--format", "json", "runtime", "doctor"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["schema"], "reporch.cli-error.v1");
    assert_eq!(error["command"], "runtime doctor");
    assert_eq!(error["error_code"], "runtime.doctor_failed");
    assert_eq!(error["retryable"], false);
    assert!(error["details"]["checks"].as_array().unwrap().len() >= 5);
    assert!(
        error["details"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["passed"] == false)
    );
}

#[test]
fn unhealthy_runtime_doctor_is_nonzero_in_human_mode() {
    let temporary = tempfile::tempdir().unwrap();
    let output = reporch()
        .env("REPORCH_RUNTIME_HOME", temporary.path().join("runtime"))
        .args(["runtime", "doctor"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("runtime.doctor_failed"), "{stderr}");
    assert!(stderr.contains("runtime checks passed"), "{stderr}");
}
