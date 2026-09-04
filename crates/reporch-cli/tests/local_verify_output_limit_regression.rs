// Regression: local verify replaced the manifest output limit with a fixed 1 MiB cap.
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
    case " $* " in
      *validator-tests/invalid.in*) exit 1 ;;
      *wrong.py*) printf '4\n' ;;
      *)
        printf '3\n'
        dd if=/dev/zero bs=1048576 count=2 2>/dev/null | tr '\000' ' '
        ;;
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
fn local_verify_honors_the_authored_output_limit() {
    let runtime = tempfile::tempdir().unwrap();
    fake_runtime(runtime.path());
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Output limit",
            "--directory",
            project.path().to_str().unwrap(),
            "--problem-type",
            "standard",
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");
    for name in ["accepted-alt"] {
        let removed = reporch()
            .args([
                "--cwd",
                project.path().to_str().unwrap(),
                "solution",
                "remove",
                name,
            ])
            .output()
            .unwrap();
        assert!(removed.status.success(), "{removed:?}");
    }

    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![runtime.path().to_owned()];
    paths.extend(std::env::split_paths(&inherited_path));
    let verified = reporch()
        .args([
            "--cwd",
            project.path().to_str().unwrap(),
            "--quiet",
            "verify",
            "--local",
            "--runtime",
            "podman",
        ])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .output()
        .unwrap();

    assert!(verified.status.success(), "{verified:?}");
}
