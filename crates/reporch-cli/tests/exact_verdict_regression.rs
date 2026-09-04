// Regression: ISSUE-005 — all non-accepted solution expectations collapsed to "rejected".
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

fn fake_wrong_answer_runtime(directory: &std::path::Path) {
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
  run) printf '%s\n' '0'; exit 0 ;;
  rm) ;;
  *) exit 64 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn fast_wrong_answer_does_not_satisfy_a_time_limit_expectation() {
    let runtime = tempfile::tempdir().unwrap();
    fake_wrong_answer_runtime(runtime.path());
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Exact verdict",
            "--directory",
            project.path().to_str().unwrap(),
            "--problem-type",
            "grader",
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
            "known-wrong",
            "--expected",
            "time-limit",
        ])
        .output()
        .unwrap();
    assert!(updated.status.success(), "{updated:?}");

    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![runtime.path().to_owned()];
    paths.extend(std::env::split_paths(&inherited_path));
    let graded = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "--format",
            "json",
            "grader",
            "run",
            "--solution",
            "known-wrong",
            "--test",
            "sample-1",
            "--runtime",
            "podman",
        ])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .output()
        .unwrap();

    assert_eq!(graded.status.code(), Some(1), "{graded:?}");
    let error: serde_json::Value = serde_json::from_slice(&graded.stderr).unwrap();
    assert_eq!(error["details"]["expected"], "time_limit");
    assert_eq!(error["details"]["actual"], "wrong_answer");
    assert_eq!(error["details"]["termination"], "exited");
    assert_eq!(error["details"]["passed"], false);
}
