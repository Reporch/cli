// Regression: CLI-090-001 — package profile collided with the global connection profile
// Found by /qa on 2026-08-14
// Report: .gstack/qa-reports/qa-report-cli-0.9.0-production-2026-08-14.md

use std::fs;
use std::process::Command;

use serde_json::Value;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env_remove("REPORCH_PROFILE");
    command
}

#[test]
fn package_profile_commands_complete_without_panicking_and_round_trip() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let archive = temporary.path().join("problem.zip");
    let imported = temporary.path().join("imported");

    let init = reporch()
        .args([
            "--quiet",
            "project",
            "init",
            "--title",
            "Package profile regression",
            "--directory",
            project.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");

    let manifest = project.join("reporch.problem.json");
    let compatibility = reporch()
        .args([
            "--format",
            "json",
            "manifest",
            "compatibility",
            manifest.to_str().unwrap(),
            "--profile",
            "reporch-native",
        ])
        .output()
        .unwrap();
    assert!(compatibility.status.success(), "{compatibility:?}");
    assert!(compatibility.stderr.is_empty(), "{compatibility:?}");
    let compatibility: Value = serde_json::from_slice(&compatibility.stdout).unwrap();
    assert_eq!(compatibility["command"], "manifest compatibility");
    assert_eq!(compatibility["data"]["exportable"], true);
    assert_eq!(compatibility["data"]["lossless"], true);

    let export = reporch()
        .args([
            "--format",
            "json",
            "package",
            "export",
            manifest.to_str().unwrap(),
            archive.to_str().unwrap(),
            "--profile",
            "reporch-native",
            "--source-root",
            project.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(export.status.success(), "{export:?}");
    assert!(export.stderr.is_empty(), "{export:?}");
    assert!(archive.is_file());

    let import = reporch()
        .args([
            "--format",
            "json",
            "package",
            "import",
            archive.to_str().unwrap(),
            imported.to_str().unwrap(),
            "--profile",
            "reporch-native",
        ])
        .output()
        .unwrap();
    assert!(import.status.success(), "{import:?}");
    assert!(import.stderr.is_empty(), "{import:?}");
    assert_eq!(
        fs::read(imported.join("reporch.problem.json")).unwrap(),
        fs::read(manifest).unwrap()
    );
}
