// Regression: ISSUE-006 — failed component checks discarded their execution evidence.
// Found by /qa on 2026-09-04.
// Report: .gstack/qa-reports/qa-report-reporch-cli-deep-2026-09-04.md

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

fn fake_failing_runtime(directory: &std::path::Path) {
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
    printf '%s\n' 'VISIBLE_COMPONENT_OUTPUT'
    printf '%s\n' 'VISIBLE_COMPONENT_DIAGNOSTIC' >&2
    exit 9
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
fn failed_validator_json_preserves_exit_termination_stdout_and_stderr() {
    let runtime = tempfile::tempdir().unwrap();
    fake_failing_runtime(runtime.path());
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Component evidence",
            "--directory",
            project.path().to_str().unwrap(),
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");

    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![runtime.path().to_owned()];
    paths.extend(std::env::split_paths(&inherited_path));
    let validated = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "--format",
            "json",
            "validator",
            "run",
            "--name",
            "accepts-sample",
            "--runtime",
            "podman",
        ])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .output()
        .unwrap();

    assert_eq!(validated.status.code(), Some(1), "{validated:?}");
    let error: serde_json::Value = serde_json::from_slice(&validated.stderr).unwrap();
    assert_eq!(error["error_code"], "operation.failed");
    assert_eq!(error["details"]["passed"], false);
    assert_eq!(error["details"]["cases"][0]["exit_code"], 9);
    assert_eq!(error["details"]["cases"][0]["termination"], "exited");
    assert!(
        error["details"]["cases"][0]["stdout"]
            .as_str()
            .unwrap()
            .contains("VISIBLE_COMPONENT_OUTPUT")
    );
    assert!(
        error["details"]["cases"][0]["stderr"]
            .as_str()
            .unwrap()
            .contains("VISIBLE_COMPONENT_DIAGNOSTIC")
    );
}
