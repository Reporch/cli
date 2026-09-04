// Regression: compiled library/grader solutions spent their execution limit compiling and timed out.
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
    printf '%s\n' "$@" > "$REPORCH_TEST_RUNTIME_LOG"
    printf '%s\n' '3'
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
fn compiled_grader_applies_the_problem_limit_after_compilation() {
    let runtime = tempfile::tempdir().unwrap();
    fake_runtime(runtime.path());
    let log = runtime.path().join("run.log");
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Compiled limit",
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
            "accepted",
            "--test",
            "sample-1",
            "--runtime",
            "podman",
            "--timeout-seconds",
            "1",
        ])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("REPORCH_TEST_RUNTIME_LOG", &log)
        .output()
        .unwrap();
    assert!(graded.status.success(), "{graded:?}");

    let command = fs::read_to_string(log).unwrap();
    let compile = command
        .find("c++ -std=c++20")
        .expect("the grader should compile inside the VM");
    let execute = command
        .find("timeout --kill-after=1s 1.000s /run/reporch/program")
        .expect("the problem limit should wrap only the compiled program");
    assert!(compile < execute, "{command}");
}
