// Regression: invalid group graphs, points, and floating tolerances must not be persisted.

use std::fs;
use std::process::{Command, Output};

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

fn init(directory: &std::path::Path) {
    let output = reporch()
        .args([
            "--quiet",
            "project",
            "init",
            "--title",
            "Mutation validation",
            "--directory",
            directory.to_str().unwrap(),
            "--problem-type",
            "scored",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

fn run(directory: &std::path::Path, arguments: &[&str]) -> Output {
    reporch()
        .args(["--cwd", directory.to_str().unwrap(), "--format", "json"])
        .args(arguments)
        .output()
        .unwrap()
}

fn exercise_invalid_mutations(directory: &std::path::Path) {
    for arguments in [
        &["test", "group", "add", "negative", "--points", "-1"][..],
        &[
            "checker",
            "set",
            "--kind",
            "floating",
            "--absolute-error",
            "0",
            "--relative-error",
            "0",
        ][..],
    ] {
        let before = fs::read(directory.join("reporch.yaml")).unwrap();
        let output = run(directory, arguments);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert_eq!(fs::read(directory.join("reporch.yaml")).unwrap(), before);
    }

    assert!(
        run(directory, &["test", "group", "add", "a", "--points", "50"])
            .status
            .success()
    );
    assert!(
        run(
            directory,
            &[
                "test",
                "group",
                "add",
                "b",
                "--points",
                "50",
                "--depends-on",
                "a",
            ],
        )
        .status
        .success()
    );
    let before = fs::read(directory.join("reporch.yaml")).unwrap();
    let cycle = run(
        directory,
        &["test", "group", "update", "a", "--depends-on", "b"],
    );
    assert_eq!(cycle.status.code(), Some(2), "{cycle:?}");
    assert_eq!(fs::read(directory.join("reporch.yaml")).unwrap(), before);
}

#[test]
fn v1_rejects_invalid_authoring_mutations_atomically() {
    let directory = tempfile::tempdir().unwrap();
    init(directory.path());
    exercise_invalid_mutations(directory.path());
}

#[test]
fn v2_rejects_invalid_authoring_mutations_atomically() {
    let directory = tempfile::tempdir().unwrap();
    init(directory.path());
    reporch_cli::local_project_v2::migrate_v1_authoring_file(directory.path()).unwrap();
    exercise_invalid_mutations(directory.path());
}
