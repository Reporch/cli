// Regression: QA-CLI-013 — a failed static check incorrectly recommended `reporch verify`.
// Found by /qa on 2026-09-04.
// Report: .gstack/qa-reports/qa-report-reporch-cli-2026-09-04.md

use std::fs;
use std::process::Command;

use serde_json::Value;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

#[test]
fn failed_static_check_points_back_to_remediation() {
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Check failure guidance",
            "--problem-type",
            "output-only",
            "--directory",
            project.path().to_str().unwrap(),
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");

    let removed = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "output",
            "remove",
            "known-wrong",
        ])
        .output()
        .unwrap();
    assert!(removed.status.success(), "{removed:?}");
    fs::remove_file(project.path().join("outputs/known-wrong.txt")).unwrap();

    let checked = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "--format",
            "json",
            "check",
        ])
        .output()
        .unwrap();
    assert_eq!(checked.status.code(), Some(1), "{checked:?}");
    let error: Value = serde_json::from_slice(&checked.stderr).unwrap();
    assert_eq!(error["error_code"], "check.failed");
    assert_eq!(error["details"]["valid"], false);
    assert_eq!(error["details"]["next_step"], "reporch check");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("run `reporch check` again")
    );
}
