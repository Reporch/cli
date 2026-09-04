// Regression: ISSUE-010 — test update/remove forced users to copy internal UUIDs.
// Found by /qa on 2026-09-04.
// Report: .gstack/qa-reports/qa-report-reporch-cli-deep-2026-09-04.md

use std::process::Command;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

fn run(project: &std::path::Path, arguments: &[&str]) -> std::process::Output {
    reporch()
        .args(["--cwd", project.to_str().unwrap(), "--format", "json"])
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn test_cases_can_be_updated_and_removed_by_human_name() {
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Test selectors",
            "--directory",
            project.path().to_str().unwrap(),
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");

    let added = run(
        project.path(),
        &[
            "test",
            "case",
            "add",
            "--name",
            "second",
            "--input-text",
            "10 20\n",
            "--answer-text",
            "30\n",
        ],
    );
    assert!(added.status.success(), "{added:?}");

    let updated = run(
        project.path(),
        &["test", "case", "update", "second", "--name", "renamed"],
    );
    assert!(updated.status.success(), "{updated:?}");

    let removed = run(project.path(), &["test", "case", "remove", "renamed"]);
    assert!(removed.status.success(), "{removed:?}");

    let listed = run(project.path(), &["test", "case", "list"]);
    assert!(listed.status.success(), "{listed:?}");
    let result: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let tests = result["data"].as_array().unwrap();
    assert_eq!(tests.len(), 1);
    assert_eq!(tests[0]["name"], "sample-1");
}
