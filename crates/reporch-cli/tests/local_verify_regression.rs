// Regression: ISSUE-001 — `reporch verify --local` did not exist.
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

fn fake_matrix_runtime(directory: &std::path::Path) {
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
    joined=" $* "
    case "$joined" in
      *'/workspace/validator-tests/invalid.in'*) exit 1 ;;
      *'/workspace/solutions/wrong.py'*) printf '%s\n' '0'; exit 0 ;;
      *) printf '%s\n' '3'; exit 0 ;;
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
fn local_verify_runs_static_units_and_the_full_solution_matrix_without_auth() {
    let runtime = tempfile::tempdir().unwrap();
    fake_matrix_runtime(runtime.path());
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Local verify",
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
    let verified = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "--format",
            "json",
            "--no-input",
            "verify",
            "--local",
            "--runtime",
            "podman",
        ])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("REPORCH_STUDIO_URL", "http://127.0.0.1:9")
        .output()
        .unwrap();

    assert!(verified.status.success(), "{verified:?}");
    assert!(verified.stderr.is_empty(), "{verified:?}");
    let result: serde_json::Value = serde_json::from_slice(&verified.stdout).unwrap();
    assert_eq!(result["command"], "verify");
    assert_eq!(result["data"]["schema"], "reporch.local-verification.v1");
    assert_eq!(result["data"]["evidence"], "local_preflight_only");
    assert_eq!(result["data"]["passed"], true);
    assert_eq!(result["data"]["validator_units"], 2);
    assert_eq!(result["data"]["solutions"].as_array().unwrap().len(), 3);
    assert_eq!(result["data"]["solutions"][0]["actual"], "accepted");
    assert_eq!(result["data"]["solutions"][2]["actual"], "wrong_answer");
}
