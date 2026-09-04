// Regression: missing declared files escaped the check-result contract as raw filesystem errors.
// Found by /qa on 2026-09-04.
// Report: .gstack/qa-reports/qa-report-reporch-cli-followup-2026-09-04.md

use std::fs;
use std::process::Command;

use serde_json::Value;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

#[test]
fn missing_declared_files_include_machine_readable_recovery() {
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Missing file recovery",
            "--directory",
            project.path().to_str().unwrap(),
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");
    fs::remove_file(project.path().join("tests/1.in")).unwrap();

    for _ in 0..2 {
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
        assert_eq!(
            error["details"]["next_step"],
            "reporch project prune --apply"
        );
        assert!(
            error["details"]["issues"][0]["message"]
                .as_str()
                .unwrap()
                .contains("tests/1.in")
        );
        assert!(
            error["details"]["recovery_commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|command| command == "reporch check")
        );
    }
}
