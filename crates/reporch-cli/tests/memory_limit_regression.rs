// Regression: real language-runtime memory exhaustion was collapsed into runtime-error.
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
      *memory-limit.cpp*)
        printf "%s\n" "terminate called after throwing an instance of 'std::bad_alloc'" >&2
        exit 134
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
fn memory_exhaustion_satisfies_an_exact_memory_limit_expectation() {
    let runtime = tempfile::tempdir().unwrap();
    fake_runtime(runtime.path());
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Memory limit",
            "--directory",
            project.path().to_str().unwrap(),
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");
    fs::write(
        project.path().join("solutions/memory-limit.cpp"),
        "#include <vector>\nint main(){for(;;) std::vector<char> x(1ull<<40);}\n",
    )
    .unwrap();
    let added = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "solution",
            "add",
            "--name",
            "memory-limit",
            "--source",
            "solutions/memory-limit.cpp",
            "--language",
            "cpp",
            "--expected",
            "memory-limit",
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
    assert!(verified.status.success(), "{verified:?}");
    let report: Value = serde_json::from_slice(&verified.stdout).unwrap();
    let memory = report["data"]["solutions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|solution| solution["solution"] == "memory-limit")
        .unwrap();
    assert_eq!(memory["expected"], "memory_limit");
    assert_eq!(memory["actual"], "memory_limit");
    assert_eq!(memory["passed"], true);
}
