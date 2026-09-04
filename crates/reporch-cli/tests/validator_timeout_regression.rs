// Regression: ISSUE-004 — a timed-out validator passed a unit expecting invalid input.
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

fn fake_hanging_runtime(directory: &std::path::Path) {
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
  run) sleep 10 ;;
  rm) ;;
  *) exit 64 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn validator_timeout_is_a_domain_failure_even_when_invalid_was_expected() {
    let runtime = tempfile::tempdir().unwrap();
    fake_hanging_runtime(runtime.path());
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Validator timeout",
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
            "rejects-malformed",
            "--runtime",
            "podman",
            "--timeout-seconds",
            "1",
        ])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .output()
        .unwrap();

    assert_eq!(validated.status.code(), Some(1), "{validated:?}");
    assert!(validated.stdout.is_empty(), "{validated:?}");
    let error: serde_json::Value = serde_json::from_slice(&validated.stderr).unwrap();
    assert_eq!(error["error_code"], "runtime.execution_timed_out");
    assert_eq!(error["retryable"], false);
    assert_eq!(error["details"]["passed"], false);
    assert_eq!(error["details"]["cases"][0]["actual"], "timed_out");
    assert_eq!(error["details"]["cases"][0]["termination"], "timed_out");
}
