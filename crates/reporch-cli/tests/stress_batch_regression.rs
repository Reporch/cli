// Regression: ISSUE-008 — stress booted a new VM for every generator/oracle/candidate run.
// Found by /qa on 2026-09-04.
// Report: .gstack/qa-reports/qa-report-reporch-cli-deep-2026-09-04.md

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::process::{Command, Output};

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

fn run(project: &std::path::Path, arguments: &[&str]) -> Output {
    reporch()
        .args(["--cwd", project.to_str().unwrap(), "--format", "json"])
        .args(arguments)
        .output()
        .unwrap()
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
    printf 'RUN\n' >> "$REPORCH_FAKE_LOG"
    case " $* " in
      *batch.sh*) printf 'M\t0\t1\tMSAyCg==\tMwo=\tMAo=\nD\n' ;;
      *generators/stress.py*) printf '1 2\n' ;;
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

fn runtime_command(
    runtime: &std::path::Path,
    project: &std::path::Path,
    log: &std::path::Path,
    arguments: &[&str],
    batch: bool,
) -> Output {
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![runtime.to_owned()];
    paths.extend(std::env::split_paths(&inherited_path));
    let mut command = reporch();
    command
        .args(["--cwd", project.to_str().unwrap(), "--format", "json"])
        .args(arguments)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("REPORCH_FAKE_LOG", log);
    if batch {
        command.env("REPORCH_DEBUG_ENABLE_STRESS_BATCH", "1");
    }
    command.output().unwrap()
}

#[test]
fn supported_stress_suite_uses_one_runtime_job_for_all_seeds() {
    let runtime = tempfile::tempdir().unwrap();
    fake_runtime(runtime.path());
    let log = runtime.path().join("runs.log");
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Stress batch",
            "--directory",
            project.path().to_str().unwrap(),
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");
    fs::create_dir_all(project.path().join("generators")).unwrap();
    fs::write(
        project.path().join("generators/stress.py"),
        b"print('1 2')\n",
    )
    .unwrap();
    assert!(
        run(
            project.path(),
            &[
                "generator",
                "add",
                "--id",
                "stress-gen",
                "--source",
                "generators/stress.py",
                "--language",
                "python3",
            ],
        )
        .status
        .success()
    );
    let generated = runtime_command(
        runtime.path(),
        project.path(),
        &log,
        &[
            "generator",
            "run",
            "stress-gen",
            "--output",
            "tests/stress-seed.in",
            "--name",
            "stress-seed",
            "--seed",
            "1",
            "--runtime",
            "podman",
        ],
        false,
    );
    assert!(generated.status.success(), "{generated:?}");
    let configured = run(
        project.path(),
        &[
            "stress",
            "add",
            "--name",
            "known-wrong-check",
            "--generator",
            "stress-gen",
            "--recipe",
            "case-stress-seed",
            "--oracle",
            "accepted",
            "--candidate",
            "known-wrong",
            "--cases",
            "5",
        ],
    );
    assert!(configured.status.success(), "{configured:?}");

    fs::write(&log, b"").unwrap();
    let stressed = runtime_command(
        runtime.path(),
        project.path(),
        &log,
        &["stress", "run", "known-wrong-check", "--runtime", "podman"],
        true,
    );
    assert!(stressed.status.success(), "{stressed:?}");
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
    let result: serde_json::Value = serde_json::from_slice(&stressed.stdout).unwrap();
    assert_eq!(result["data"][0]["candidate"], "known-wrong");
    assert_eq!(result["data"][0]["counterexample_seed"], 1);
    assert_eq!(result["data"][0]["passed"], true);
}
