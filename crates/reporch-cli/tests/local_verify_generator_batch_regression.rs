// Regression: ISSUE-001 — generator determinism checks still booted separate VMs.
// Found by /qa on 2026-09-04 while verifying the ISSUE-001 fix.
// Report: .gstack/qa-reports/qa-report-reporch-cli-deep-2026-09-04.md

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use studio_core::{
    GeneratorMatrixStrategyV2, GeneratorRecipeSpecV2, GeneratorSpecV2, ProgramSpecV2,
};
use uuid::Uuid;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

fn fake_batch_runtime(directory: &std::path::Path) {
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
    printf 'G\t0\t0\tmatched\t0\texited\tMSAyCg==\t\n'
    printf 'V\t0\t0\tvalid\t0\texited\t\t\n'
    printf 'V\t0\t1\tinvalid\t1\texited\t\t\n'
    printf 'S\t0\t0\taccepted\t0\texited\tMwo=\t\n'
    printf 'S\t1\t0\taccepted\t0\texited\tMwo=\t\n'
    printf 'S\t2\t0\twrong_answer\t0\texited\tMAo=\t\n'
    printf 'D\n'
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
fn generator_and_solution_preflights_share_the_same_vm_job() {
    let runtime = tempfile::tempdir().unwrap();
    fake_batch_runtime(runtime.path());
    let log = runtime.path().join("runs.log");
    let project = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Generator batch",
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
        project.path().join("generators/random.py"),
        b"print('1 2')\n",
    )
    .unwrap();
    reporch_cli::local_project_v2::update_authoring_spec(project.path(), |root, spec| {
        reporch_cli::local_project_v2::declare_project_file(
            root,
            spec,
            "generators/random.py",
            "text/x-python",
            false,
        )?;
        spec.testing.generators.push(GeneratorSpecV2 {
            program: ProgramSpecV2 {
                id: Uuid::now_v7(),
                name: "random".into(),
                source_path: "generators/random.py".into(),
                language: "python3".into(),
                arguments: Vec::new(),
            },
            recipes: vec![GeneratorRecipeSpecV2 {
                id: Uuid::now_v7(),
                name: "smoke".into(),
                argument_template: Vec::new(),
                parameters: Default::default(),
                matrix: GeneratorMatrixStrategyV2::Cartesian,
                seed_start: 1,
                seed_step: 1,
                count: 1,
                group_ids: Vec::new(),
            }],
        });
        Ok(())
    })
    .unwrap();

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
        .env("REPORCH_FAKE_LOG", &log)
        .env("REPORCH_DEBUG_ENABLE_LOCAL_VERIFY_BATCH", "1")
        .output()
        .unwrap();

    assert!(verified.status.success(), "{verified:?}");
    assert_eq!(fs::read_to_string(log).unwrap().lines().count(), 1);
    let result: serde_json::Value = serde_json::from_slice(&verified.stdout).unwrap();
    assert_eq!(
        result["data"]["generator_checks"].as_array().unwrap().len(),
        1
    );
    assert_eq!(result["data"]["generator_checks"][0]["passed"], true);
}
