// Regression: QA-L05 — first-time users could not recover from an unknown `samples` group.
// Found by /qa on 2026-09-01.
// Report: .gstack/qa-reports/qa-report-reporch-cli-rc9-fixed-luna10-2026-09-01.md

use std::process::Command;

use serde_json::Value;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

#[test]
fn unknown_groups_explain_creation_listing_and_ungrouped_samples() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("problem");
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "A + B",
            "--directory",
            project.to_str().unwrap(),
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");

    let output = reporch()
        .args([
            "--cwd",
            project.to_str().unwrap(),
            "--format",
            "json",
            "test",
            "case",
            "add",
            "--name",
            "second",
            "--input",
            "tests/1.in",
            "--answer",
            "tests/1.ans",
            "--group",
            "samples",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("reporch test group add samples --points 0"),
        "{message}"
    );
    assert!(message.contains("reporch test group list"), "{message}");
    assert!(message.contains("omit --group"), "{message}");
}

#[test]
fn test_help_creates_a_group_before_using_it_and_explains_optional_groups() {
    let output = reporch().args(["test", "--help"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let help = String::from_utf8(output.stdout).unwrap();
    let create = help
        .find("reporch test group add samples --points 0")
        .unwrap();
    let use_group = help.find("reporch test case add --name sample-1").unwrap();
    assert!(create < use_group, "{help}");
    assert!(help.contains("`--group` is optional"), "{help}");
    assert!(help.contains("Sample tests can remain ungrouped"), "{help}");
}
