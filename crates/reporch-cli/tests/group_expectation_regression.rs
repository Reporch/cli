// Regression: scored solutions exposed group_expectations but the CLI could not author or verify them.
// Found by /qa on 2026-09-04.
// Report: .gstack/qa-reports/qa-report-reporch-cli-followup-2026-09-04.md

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use serde_json::Value;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

fn fake_runtime(directory: &std::path::Path) {
    let path = directory.join("podman");
    fs::write(
        &path,
        r#"#!/bin/sh
set -eu
case "$1" in
  --version) printf '%s\n' 'podman version 5.0.0' ;;
  info) printf '%s\n' '{"host":{"security":{"rootless":true}}}' ;;
  image)
    last=''
    for argument in "$@"; do last=$argument; done
    printf '["%s"]\n' "$last"
    ;;
  run)
    case " $* " in
      *validator-tests/invalid.in*) exit 1 ;;
      *solutions/partial.py*)
        case " $* " in
          *tests/2.in*) printf '0\n' ;;
          *) printf '3\n' ;;
        esac
        ;;
      *solutions/wrong.py*) printf '0\n' ;;
      *tests/2.in*) printf '300\n' ;;
      *) printf '3\n' ;;
    esac
    ;;
  rm) ;;
  *) exit 64 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn scored_solution_group_expectations_round_trip_and_are_verified() {
    let runtime = tempfile::tempdir().unwrap();
    fake_runtime(runtime.path());
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Group expectations",
            "--directory",
            project.path().to_str().unwrap(),
            "--problem-type",
            "scored",
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");

    let updated = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "solution",
            "update",
            "partial-50",
            "--group-expectation",
            "easy=accepted",
            "--group-expectation",
            "hard=wrong-answer",
        ])
        .output()
        .unwrap();
    assert!(updated.status.success(), "{updated:?}");

    let matrix = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "--format",
            "json",
            "solution",
            "matrix",
        ])
        .output()
        .unwrap();
    assert!(matrix.status.success(), "{matrix:?}");
    let matrix: Value = serde_json::from_slice(&matrix.stdout).unwrap();
    let partial = matrix["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|solution| solution["program"]["name"] == "partial-50")
        .unwrap();
    assert_eq!(partial["group_expectations"].as_array().unwrap().len(), 2);

    let before = fs::read(project.path().join("reporch.yaml")).unwrap();
    let invalid = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "solution",
            "update",
            "partial-50",
            "--group-expectation",
            "missing=accepted",
        ])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2), "{invalid:?}");
    assert_eq!(
        before,
        fs::read(project.path().join("reporch.yaml")).unwrap()
    );

    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![runtime.path().to_owned()];
    paths.extend(std::env::split_paths(&inherited_path));
    let verified = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "--format",
            "json",
            "verify",
            "--local",
            "--runtime",
            "podman",
        ])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .output()
        .unwrap();
    assert!(verified.status.success(), "{verified:?}");
    let report: Value = serde_json::from_slice(&verified.stdout).unwrap();
    let partial = report["data"]["solutions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|solution| solution["solution"] == "partial-50")
        .unwrap();
    assert_eq!(partial["group_expectations"].as_array().unwrap().len(), 2);
    assert!(
        partial["group_expectations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|group| group["passed"] == true)
    );
}
