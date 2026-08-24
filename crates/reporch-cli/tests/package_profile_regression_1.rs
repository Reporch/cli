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

#[test]
fn every_external_v2_sidecar_restores_the_exact_manifest_and_files() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let init = reporch()
        .args([
            "--quiet",
            "project",
            "init",
            "--title",
            "Polygon V2 round trip",
            "--directory",
            project.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");
    for arguments in [
        vec![
            "validator",
            "set",
            "--source",
            "solutions/accepted.py",
            "--language",
            "python3",
        ],
        vec![
            "validator",
            "unit-add",
            "--name",
            "valid",
            "--input",
            "tests/1.in",
            "--expected",
            "valid",
        ],
        vec![
            "validator",
            "unit-add",
            "--name",
            "invalid",
            "--input",
            "tests/1.in",
            "--expected",
            "invalid",
        ],
    ] {
        let output = reporch()
            .args(["--cwd", project.to_str().unwrap(), "--quiet"])
            .args(arguments)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let manifest = project.join("reporch.problem.json");
    let previous: studio_core::ReleaseManifestV2 =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    let spec = reporch_cli::local_project_v2::read_authoring_spec(&project).unwrap();
    let current =
        reporch_cli::local_project_v2::compile_authoring_spec(&project, &spec, previous.commit_id)
            .unwrap();
    fs::write(&manifest, serde_json::to_vec_pretty(&current).unwrap()).unwrap();
    for (name, profile) in [
        ("polygon", "polygon-compatible"),
        ("icpc", "icpc202509"),
        ("legacy", "icpc-legacy"),
        ("domjudge", "domjudge-zip"),
    ] {
        let archive = temporary.path().join(format!("{name}.zip"));
        let imported = temporary.path().join(format!("imported-{name}"));
        let export = reporch()
            .args([
                "--format",
                "json",
                "package",
                "export",
                manifest.to_str().unwrap(),
                archive.to_str().unwrap(),
                "--profile",
                profile,
                "--source-root",
                project.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(export.status.success(), "{profile}: {export:?}");
        let import = reporch()
            .args([
                "--format",
                "json",
                "package",
                "import",
                archive.to_str().unwrap(),
                imported.to_str().unwrap(),
                "--profile",
                profile,
            ])
            .output()
            .unwrap();
        assert!(import.status.success(), "{profile}: {import:?}");
        assert_eq!(
            serde_json::from_slice::<Value>(
                &fs::read(imported.join("reporch.problem.json")).unwrap()
            )
            .unwrap(),
            serde_json::from_slice::<Value>(&fs::read(&manifest).unwrap()).unwrap(),
            "{profile}"
        );
        assert_eq!(
            fs::read(imported.join("solutions/accepted.py")).unwrap(),
            fs::read(project.join("solutions/accepted.py")).unwrap(),
            "{profile}"
        );
    }
}
