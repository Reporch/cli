// Regression: ISSUE-007 — test case add accepted an existing input and left the project invalid.
// Found by /qa on 2026-09-04.
// Report: .gstack/qa-reports/qa-report-reporch-cli-deep-2026-09-04.md

use std::fs;
use std::process::Command;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

#[test]
fn test_case_add_rejects_same_path_and_same_digest_before_saving() {
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Duplicate tests",
            "--directory",
            project.path().to_str().unwrap(),
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");

    fs::write(project.path().join("tests/duplicate.in"), b"1 2\n").unwrap();
    fs::write(project.path().join("tests/duplicate.ans"), b"3\n").unwrap();
    for (name, input, answer) in [
        ("same-path", "tests/1.in", "tests/1.ans"),
        ("same-digest", "tests/duplicate.in", "tests/duplicate.ans"),
    ] {
        let added = reporch()
            .args([
                "--cwd",
                project.path().to_str().unwrap(),
                "--format",
                "json",
                "test",
                "case",
                "add",
                "--name",
                name,
                "--input",
                input,
                "--answer",
                answer,
            ])
            .output()
            .unwrap();
        assert_eq!(added.status.code(), Some(2), "{added:?}");
        let error: serde_json::Value = serde_json::from_slice(&added.stderr).unwrap();
        let message = error["message"].as_str().unwrap();
        assert!(
            message.contains("duplicates existing test sample-1"),
            "{message}"
        );
        assert!(message.contains("update the existing test"), "{message}");
    }

    let listed = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "--format",
            "json",
            "test",
            "case",
            "list",
        ])
        .output()
        .unwrap();
    assert!(listed.status.success(), "{listed:?}");
    let result: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(result["data"].as_array().unwrap().len(), 1);
}
