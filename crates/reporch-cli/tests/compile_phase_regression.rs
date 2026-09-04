// Regression: a compiler failure could satisfy a solution declared as runtime-error.
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
      *does-not-compile.cpp*)
        printf '%s\n' 'source.cpp:1: error: expected initializer' >&2
        printf '%s\n' 'reporch:compilation-failed:v1' >&2
        exit 126
        ;;
      *wrong.py*) printf '4\n' ;;
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
fn compile_failure_is_a_judge_error_not_a_runtime_verdict() {
    let runtime = tempfile::tempdir().unwrap();
    fake_runtime(runtime.path());
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Compile phase",
            "--directory",
            project.path().to_str().unwrap(),
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");
    fs::write(
        project.path().join("solutions/does-not-compile.cpp"),
        "int main( {\n",
    )
    .unwrap();
    let added = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "solution",
            "add",
            "--name",
            "does-not-compile",
            "--source",
            "solutions/does-not-compile.cpp",
            "--language",
            "cpp",
            "--expected",
            "runtime-error",
            "--role",
            "alternative",
        ])
        .output()
        .unwrap();
    assert!(added.status.success(), "{added:?}");

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

    assert_eq!(verified.status.code(), Some(1), "{verified:?}");
    let error: Value = serde_json::from_slice(&verified.stderr).unwrap();
    let solution = error["details"]["solutions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|solution| solution["solution"] == "does-not-compile")
        .unwrap();
    assert_eq!(solution["expected"], "runtime_error");
    assert_eq!(solution["actual"], "judge_error");
    assert_eq!(solution["passed"], false);
    assert_eq!(solution["cases"][0]["exit_code"], 126);
    assert!(
        solution["cases"][0]["stderr"]
            .as_str()
            .unwrap()
            .contains("compilation-failed")
    );
}
