// Regression: QA-L08 — local execution exposed one coarse preparation message for long work.
// Found by /qa on 2026-09-01.
// Report: .gstack/qa-reports/qa-report-reporch-cli-rc9-fixed-luna10-2026-09-01.md

use std::fs;
use std::process::Command;

use serde_json::Value;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

#[cfg(unix)]
fn fake_runtime(directory: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let path = directory.join("podman");
    fs::write(
        &path,
        r#"#!/bin/sh
set -eu
case "$1" in
  --version)
    printf '%s\n' 'podman version 5.0.0'
    ;;
  info)
    printf '%s\n' '{"host":{"security":{"rootless":true}}}'
    ;;
  image)
    last=''
    for argument in "$@"; do last=$argument; done
    printf '["%s"]\n' "$last"
    ;;
  run)
    case " $* " in
      *'/workspace/validator-tests/invalid.in'*) exit 1 ;;
      *) exit 0 ;;
    esac
    ;;
  rm)
    ;;
  *)
    exit 64
    ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn local_execution_reports_toolchain_vm_and_run_phases_without_polluting_stdout() {
    let runtime = tempfile::tempdir().unwrap();
    fake_runtime(runtime.path());
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Progress regression",
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
    let output = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "--format",
            "jsonl",
            "validator",
            "run",
            "--runtime",
            "podman",
        ])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    let stdout: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["schema"], "reporch.cli-result.v1");
    assert_eq!(stdout["command"], "validator run");

    let stderr = String::from_utf8(output.stderr).unwrap();
    let progress = stderr
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        progress
            .iter()
            .all(|value| value["schema"] == "reporch.cli-progress.v1"),
        "{stderr}"
    );
    let messages = progress
        .iter()
        .filter_map(|value| value["message"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "Initializing local verification",
        "Resolving the signed python3 toolchain",
        "Installing or verifying signed toolchain",
        "Preparing the isolated Reporch VM",
        "Running the isolated Reporch VM job",
    ] {
        assert!(
            messages.contains(expected),
            "missing {expected}:\n{messages}"
        );
    }
}
