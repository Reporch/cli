use std::fs;
use std::process::Command;

use serde_json::Value;

fn reporch() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reporch"))
}

#[test]
fn json_mode_emits_one_stable_success_envelope() {
    let temporary = tempfile::tempdir().unwrap();
    let output = reporch()
        .args([
            "--format",
            "json",
            "project",
            "init",
            "--title",
            "CLI contract",
            "--directory",
            temporary.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema"], "reporch.cli-result.v1");
    assert_eq!(value["command"], "project init");
    assert_eq!(value["data"]["dirty"], false);
    assert!(temporary.path().join("reporch.yaml").is_file());
    assert!(temporary.path().join("reporch.problem.json").is_file());
}

#[test]
fn json_parse_errors_use_exit_two_and_the_error_envelope() {
    let output = reporch()
        .args(["--format", "json", "project", "unknown"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["schema"], "reporch.cli-error.v1");
    assert_eq!(value["command"], "parse");
    assert_eq!(value["error_code"], "input.invalid");
    assert_eq!(value["retryable"], false);
}

#[test]
fn check_is_networkless_and_finds_the_project_from_a_child_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let init = reporch()
        .args([
            "--quiet",
            "project",
            "init",
            "--title",
            "Nested check",
            "--directory",
            temporary.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");
    let child = temporary.path().join("solutions");
    let output = reporch()
        .args([
            "--cwd",
            child.to_str().unwrap(),
            "--format",
            "json",
            "check",
        ])
        .env("REPORCH_STUDIO_API_URL", "http://127.0.0.1:1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["command"], "check");
    assert_eq!(value["data"]["valid"], true);
}

#[test]
fn migrate_requires_yes_in_ci_and_is_idempotent() {
    let temporary = tempfile::tempdir().unwrap();
    reporch()
        .args([
            "--quiet",
            "project",
            "init",
            "--title",
            "Legacy",
            "--directory",
            temporary.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    fs::remove_file(temporary.path().join("reporch.yaml")).unwrap();

    let refused = reporch()
        .args([
            "--format",
            "json",
            "migrate",
            "--directory",
            temporary.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");

    for expected_migrated in [true, false] {
        let output = reporch()
            .args([
                "--yes",
                "--format",
                "json",
                "migrate",
                "--directory",
                temporary.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["data"]["migrated"], expected_migrated);
    }
    assert!(
        temporary
            .path()
            .join("reporch.problem.pre-1.0.json")
            .is_file()
    );
}
