// Regression: ISSUE-001 — full local verification booted one VM per component case.
// Found by /qa on 2026-09-04 while verifying the ISSUE-001 fix.
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

fn fake_batch_runtime(directory: &std::path::Path) {
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
    printf 'RUN\n' >> "$REPORCH_FAKE_LOG"
    printf 'V\t0\t0\tvalid\t0\texited\t\t\n'
    printf 'V\t0\t1\tinvalid\t1\texited\t\t\n'
    printf 'S\t0\t0\taccepted\t0\texited\tMwo=\t\n'
    printf 'S\t1\t0\taccepted\t0\texited\tMwo=\t\n'
    printf 'S\t2\t0\twrong_answer\t0\texited\tMAo=\t\n'
    printf 'D\n'
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
fn standard_local_verify_runs_all_validator_and_solution_cases_in_one_vm() {
    let runtime = tempfile::tempdir().unwrap();
    fake_batch_runtime(runtime.path());
    let log = runtime.path().join("runs.log");
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Batched local verify",
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
            "verify",
            "--local",
            "--runtime",
            "podman",
        ])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("REPORCH_FAKE_LOG", &log)
        .env("REPORCH_DEBUG_ENABLE_LOCAL_VERIFY_BATCH", "1")
        .output()
        .unwrap();

    assert!(verified.status.success(), "{verified:?}");
    assert_eq!(fs::read_to_string(log).unwrap().lines().count(), 1);
    let result: serde_json::Value = serde_json::from_slice(&verified.stdout).unwrap();
    assert_eq!(result["data"]["passed"], true);
    assert_eq!(result["data"]["validator_units"], 2);
    assert_eq!(result["data"]["solutions"].as_array().unwrap().len(), 3);
    assert_eq!(result["data"]["solutions"][2]["actual"], "wrong_answer");
}
