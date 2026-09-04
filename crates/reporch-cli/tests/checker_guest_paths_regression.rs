// Regression: ISSUE-002 — native custom-checker jobs validated input files but did not stage them.
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

fn fake_rootless_runtime(directory: &std::path::Path) {
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
    for argument in "$@"; do printf 'ARG=%s\n' "$argument" >> "$REPORCH_FAKE_LOG"; done
    exit 42
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
fn icpc_checker_receives_staged_workspace_paths_for_all_three_files() {
    let runtime = tempfile::tempdir().unwrap();
    fake_rootless_runtime(runtime.path());
    let log = runtime.path().join("checker-arguments.log");
    let project = tempfile::tempdir().unwrap();

    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Checker guest paths",
            "--directory",
            project.path().to_str().unwrap(),
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");

    let configured = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "checker",
            "set",
            "--kind",
            "custom",
            "--source",
            "solutions/accepted.py",
            "--language",
            "python3",
        ])
        .output()
        .unwrap();
    assert!(configured.status.success(), "{configured:?}");

    let added = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "checker",
            "unit-add",
            "--name",
            "accepts-sample",
            "--input",
            "tests/1.in",
            "--answer",
            "tests/1.ans",
            "--output",
            "tests/1.ans",
            "--expected",
            "accept",
        ])
        .output()
        .unwrap();
    assert!(added.status.success(), "{added:?}");

    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![runtime.path().to_owned()];
    paths.extend(std::env::split_paths(&inherited_path));
    let checked = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "checker",
            "run",
            "--runtime",
            "podman",
        ])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("REPORCH_FAKE_LOG", &log)
        .output()
        .unwrap();
    assert!(checked.status.success(), "{checked:?}");

    let arguments = fs::read_to_string(log).unwrap();
    assert!(
        arguments.contains("ARG=/workspace/tests/1.in"),
        "{arguments}"
    );
    assert!(
        arguments.contains("ARG=/workspace/tests/1.ans"),
        "{arguments}"
    );
    assert!(
        arguments.contains("ARG=/workspace/solutions/accepted.py"),
        "{arguments}"
    );
}
