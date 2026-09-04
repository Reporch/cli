// Regression: interactor run ignored the manifest time and idle limits and waited 30 seconds.
// Found by /qa on 2026-09-04.
// Report: .gstack/qa-reports/qa-report-reporch-cli-followup-2026-09-04.md

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
    exit 124
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
fn interactor_run_wires_manifest_total_and_idle_limits_into_the_guest() {
    let runtime = tempfile::tempdir().unwrap();
    fake_runtime(runtime.path());
    let log = runtime.path().join("run.log");
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Interactive limits",
            "--directory",
            project.path().to_str().unwrap(),
            "--problem-type",
            "interactive",
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");
    let yaml_path = project.path().join("reporch.yaml");
    let yaml = fs::read_to_string(&yaml_path)
        .unwrap()
        .replace("time_ms: 1000", "time_ms: 500")
        .replace("idle_timeout_ms: 2000", "idle_timeout_ms: 200");
    fs::write(&yaml_path, yaml).unwrap();

    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![runtime.path().to_owned()];
    paths.extend(std::env::split_paths(&inherited_path));
    let executed = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "--format",
            "json",
            "interactor",
            "run",
            "--solution",
            "accepted",
            "--test",
            "sample-1",
            "--runtime",
            "podman",
        ])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("REPORCH_TEST_RUNTIME_LOG", &log)
        .output()
        .unwrap();
    assert_eq!(executed.status.code(), Some(1), "{executed:?}");

    let command = fs::read_to_string(log).unwrap();
    assert!(
        command.contains("timeout --kill-after=1s 0.500s"),
        "{command}"
    );
    assert!(command.contains("idle_timeout=0.200"), "{command}");
    assert!(
        command.contains("read -r -t \"$idle_timeout\" -u 5"),
        "{command}"
    );
}
