// Regression: ISSUE-003 — an ICPC checker crash passed a unit expecting rejection.
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

fn run(project: &std::path::Path, arguments: &[&str]) -> std::process::Output {
    reporch()
        .args(["--cwd", project.to_str().unwrap(), "--format", "json"])
        .args(arguments)
        .output()
        .unwrap()
}

fn fake_crashing_runtime(directory: &std::path::Path) {
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
  run) printf '%s\n' 'checker crashed' >&2; exit 1 ;;
  rm) ;;
  *) exit 64 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn icpc_judge_error_never_satisfies_an_expected_rejection() {
    let runtime = tempfile::tempdir().unwrap();
    fake_crashing_runtime(runtime.path());
    let project = tempfile::tempdir().unwrap();
    assert!(
        reporch()
            .args([
                "new",
                "--title",
                "Judge error",
                "--directory",
                project.path().to_str().unwrap(),
                "--yes",
                "--quiet",
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        run(
            project.path(),
            &[
                "checker",
                "set",
                "--kind",
                "custom",
                "--source",
                "solutions/accepted.py",
                "--language",
                "python3",
            ],
        )
        .status
        .success()
    );
    assert!(
        run(
            project.path(),
            &[
                "checker",
                "unit-add",
                "--name",
                "must-reject",
                "--input",
                "tests/1.in",
                "--answer",
                "tests/1.ans",
                "--output",
                "tests/1.ans",
                "--expected",
                "reject",
            ],
        )
        .status
        .success()
    );

    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![runtime.path().to_owned()];
    paths.extend(std::env::split_paths(&inherited_path));
    let checked = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "--format",
            "json",
            "checker",
            "run",
            "--runtime",
            "podman",
        ])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .output()
        .unwrap();

    assert_eq!(checked.status.code(), Some(1), "{checked:?}");
}
