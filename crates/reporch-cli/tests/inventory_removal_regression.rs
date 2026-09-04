// Regression: structural removal must not leave stale inventory declarations.

use std::fs;
use std::process::{Command, Output};

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

fn run(directory: &std::path::Path, arguments: &[&str]) -> Output {
    reporch()
        .args(["--cwd", directory.to_str().unwrap(), "--format", "json"])
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn v2_removals_prune_only_unreferenced_inventory_and_preserve_files() {
    let directory = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "--quiet",
            "project",
            "init",
            "--title",
            "Inventory removal",
            "--directory",
            directory.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");
    reporch_cli::local_project_v2::migrate_v1_authoring_file(directory.path()).unwrap();

    fs::create_dir_all(directory.path().join("qa")).unwrap();
    fs::write(directory.path().join("qa/shared.py"), "print(1)\n").unwrap();
    fs::write(directory.path().join("qa/case.in"), "1\n").unwrap();
    fs::write(directory.path().join("qa/case.ans"), "1\n").unwrap();

    let generator = run(
        directory.path(),
        &[
            "generator",
            "add",
            "--id",
            "shared",
            "--source",
            "qa/shared.py",
            "--language",
            "python3",
        ],
    );
    assert!(generator.status.success(), "{generator:?}");
    let solution = run(
        directory.path(),
        &[
            "solution",
            "add",
            "--name",
            "shared-wrong",
            "--source",
            "qa/shared.py",
            "--language",
            "python3",
            "--expected",
            "wrong-answer",
            "--role",
            "known-wrong",
        ],
    );
    assert!(solution.status.success(), "{solution:?}");
    let test = run(
        directory.path(),
        &[
            "test",
            "case",
            "add",
            "--name",
            "qa-case",
            "--input",
            "qa/case.in",
            "--answer",
            "qa/case.ans",
        ],
    );
    assert!(test.status.success(), "{test:?}");

    let spec = reporch_cli::local_project_v2::read_authoring_spec(directory.path()).unwrap();
    let test_id = spec
        .testing
        .tests
        .iter()
        .find(|test| test.name == "qa-case")
        .unwrap()
        .id
        .to_string();
    let removed_test = run(directory.path(), &["test", "case", "remove", &test_id]);
    assert!(removed_test.status.success(), "{removed_test:?}");
    let data: serde_json::Value = serde_json::from_slice(&removed_test.stdout).unwrap();
    assert_eq!(
        data["data"]["inventory_removed"],
        serde_json::json!(["qa/case.ans", "qa/case.in"])
    );

    let removed_generator = run(directory.path(), &["generator", "remove", "shared"]);
    assert!(removed_generator.status.success(), "{removed_generator:?}");
    let spec = reporch_cli::local_project_v2::read_authoring_spec(directory.path()).unwrap();
    assert!(spec.files.iter().any(|file| file.path == "qa/shared.py"));

    let removed_solution = run(directory.path(), &["solution", "remove", "shared-wrong"]);
    assert!(removed_solution.status.success(), "{removed_solution:?}");
    let data: serde_json::Value = serde_json::from_slice(&removed_solution.stdout).unwrap();
    assert_eq!(
        data["data"]["inventory_removed"],
        serde_json::json!(["qa/shared.py"])
    );

    let spec = reporch_cli::local_project_v2::read_authoring_spec(directory.path()).unwrap();
    for path in ["qa/case.in", "qa/case.ans", "qa/shared.py"] {
        assert!(!spec.files.iter().any(|file| file.path == path));
        assert!(directory.path().join(path).is_file());
    }
}
