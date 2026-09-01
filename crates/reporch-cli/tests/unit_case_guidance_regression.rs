// Regression: QA-L10 — checker setup and multi-case execution lacked next-step and case context.
// Found by /qa on 2026-09-01.
// Report: .gstack/qa-reports/qa-report-reporch-cli-rc9-ux2-luna10-2026-09-01.md

use std::fs;
use std::process::Command;

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
  --version) printf '%s\n' 'podman version 5.0.0' ;;
  info) printf '%s\n' '{"host":{"security":{"rootless":true}}}' ;;
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
  rm) ;;
  *) exit 64 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn initialized_project() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Unit guidance",
            "--directory",
            directory.path().to_str().unwrap(),
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");
    directory
}

#[test]
fn test_help_explains_why_sample_groups_use_zero_points() {
    let output = reporch().args(["test", "--help"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(
        help.contains(
            "A 0-point sample group organizes tests and dependencies without changing the scored total"
        ),
        "{help}"
    );
}

#[test]
fn empty_checker_units_provide_a_copyable_starter_and_named_progress() {
    let project = initialized_project();
    let missing = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "--format",
            "json",
            "checker",
            "test",
        ])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(2), "{missing:?}");
    let message = String::from_utf8(missing.stderr).unwrap();
    assert!(message.contains("reporch checker unit-add --name accepts-sample"));
    assert!(message.contains("reporch checker test"));

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
    let checked = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "--format",
            "jsonl",
            "checker",
            "test",
        ])
        .output()
        .unwrap();
    assert!(checked.status.success(), "{checked:?}");
    let progress = String::from_utf8(checked.stderr).unwrap();
    assert!(
        progress.contains("Checking unit accepts-sample"),
        "{progress}"
    );
}

#[cfg(unix)]
#[test]
fn validator_progress_names_each_configured_case() {
    let runtime = tempfile::tempdir().unwrap();
    fake_runtime(runtime.path());
    let project = initialized_project();
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
    let progress = String::from_utf8(output.stderr).unwrap();
    assert!(
        progress.contains("Running validator validator · unit accepts-sample"),
        "{progress}"
    );
    assert!(
        progress.contains("Running validator validator · unit rejects-malformed"),
        "{progress}"
    );
}
