use std::fs;
use std::process::{Command, Output};

use serde_json::Value;

fn reporch() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reporch"))
}

fn init(directory: &std::path::Path, problem_type: &str) -> Output {
    reporch()
        .args([
            "project",
            "init",
            "--title",
            "Usability regression",
            "--problem-type",
            problem_type,
            "--directory",
            directory.to_str().unwrap(),
        ])
        .output()
        .unwrap()
}

fn run(directory: &std::path::Path, arguments: &[&str]) -> Output {
    reporch()
        .args(["--cwd", directory.to_str().unwrap()])
        .args(arguments)
        .output()
        .unwrap()
}

fn run_json(directory: &std::path::Path, arguments: &[&str]) -> Output {
    reporch()
        .args(["--cwd", directory.to_str().unwrap(), "--format", "json"])
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn init_explains_the_starter_instead_of_hiding_precreated_solutions() {
    let project = tempfile::tempdir().unwrap();
    let output = init(project.path(), "standard");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "Starter includes",
        "accepted",
        "known-wrong",
        "Edit the existing starter files",
        "reporch check",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?}:\n{stdout}");
    }
}

#[test]
fn init_and_statement_errors_explain_the_safe_recovery() {
    let non_empty = tempfile::tempdir().unwrap();
    fs::write(non_empty.path().join("notes.txt"), "keep me").unwrap();
    let refused = init(non_empty.path(), "standard");
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert!(
        stderr.contains("--directory <new-empty-directory>"),
        "{stderr}"
    );
    assert!(
        stderr.contains("never overwrites existing files"),
        "{stderr}"
    );

    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "standard").status.success());
    let missing = run(
        project.path(),
        &[
            "statement",
            "add",
            "--locale",
            "en",
            "--path",
            "statements/en.md",
        ],
    );
    assert_eq!(missing.status.code(), Some(2), "{missing:?}");
    let stderr = String::from_utf8(missing.stderr).unwrap();
    assert!(stderr.contains("create statements/en.md first"), "{stderr}");
    assert!(stderr.contains("reporch statement add"), "{stderr}");
}

#[test]
fn check_is_explicitly_static_and_reports_what_was_not_executed() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "standard").status.success());

    let human = run(project.path(), &["check"]);
    assert!(human.status.success(), "{human:?}");
    let stdout = String::from_utf8(human.stdout).unwrap();
    assert!(stdout.contains("Static check passed"), "{stdout}");
    assert!(stdout.contains("Not executed"), "{stdout}");
    assert!(stdout.contains("reporch verify"), "{stdout}");
    assert!(!stdout.contains("Valid ·"), "{stdout}");

    let json = run_json(project.path(), &["check"]);
    assert!(json.status.success(), "{json:?}");
    let value: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["data"]["validation_scope"], "static");
    assert_eq!(value["data"]["execution_performed"], false);
    assert_eq!(value["data"]["unexecuted"]["solutions"], 3);
    assert_eq!(value["data"]["next_step"], "reporch verify");
}

#[test]
fn scored_check_rejects_a_starter_plus_an_accidental_second_hundred_points() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "scored").status.success());
    for arguments in [
        vec!["test", "group", "add", "small", "--points", "30"],
        vec!["test", "group", "add", "full", "--points", "70"],
    ] {
        let output = run(project.path(), &arguments);
        assert!(output.status.success(), "{output:?}");
    }

    let checked = run(project.path(), &["check"]);
    assert_eq!(checked.status.code(), Some(2), "{checked:?}");
    let stderr = String::from_utf8(checked.stderr).unwrap();
    assert!(
        stderr.contains("scored problem group points must total 100"),
        "{stderr}"
    );
    assert!(stderr.contains("got 200"), "{stderr}");
    assert!(stderr.contains("reporch test group list"), "{stderr}");
}

#[test]
fn solution_roles_are_explicit_editable_and_protect_the_single_reference() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "standard").status.success());
    fs::write(
        project.path().join("solutions/custom.py"),
        "a, b = map(int, input().split())\nprint(a + b)\n",
    )
    .unwrap();

    let duplicate_reference = run(
        project.path(),
        &[
            "solution",
            "add",
            "--name",
            "custom-reference",
            "--source",
            "solutions/custom.py",
            "--language",
            "python",
            "--expected",
            "accepted",
            "--role",
            "reference",
        ],
    );
    assert_eq!(
        duplicate_reference.status.code(),
        Some(2),
        "{duplicate_reference:?}"
    );
    let stderr = String::from_utf8(duplicate_reference.stderr).unwrap();
    assert!(
        stderr.contains("reference solution already exists: accepted"),
        "{stderr}"
    );
    assert!(
        stderr.contains("solution update accepted --role alternative"),
        "{stderr}"
    );

    let demoted = run(
        project.path(),
        &["solution", "update", "accepted", "--role", "alternative"],
    );
    assert!(demoted.status.success(), "{demoted:?}");
    let added = run(
        project.path(),
        &[
            "solution",
            "add",
            "--name",
            "custom-reference",
            "--source",
            "solutions/custom.py",
            "--language",
            "python",
            "--expected",
            "accepted",
            "--role",
            "reference",
        ],
    );
    assert!(added.status.success(), "{added:?}");

    let listed = run_json(project.path(), &["solution", "list"]);
    assert!(listed.status.success(), "{listed:?}");
    let value: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let solutions = value["data"].as_array().unwrap();
    assert_eq!(
        solutions
            .iter()
            .find(|solution| solution["program"]["name"] == "custom-reference")
            .unwrap()["role"],
        "reference"
    );
    assert_eq!(
        solutions
            .iter()
            .find(|solution| solution["program"]["name"] == "accepted")
            .unwrap()["role"],
        "alternative"
    );
}

#[test]
fn solution_roles_reject_contradictory_verdicts_without_changing_the_project() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "standard").status.success());

    let contradictory = run(
        project.path(),
        &[
            "solution",
            "update",
            "known-wrong",
            "--role",
            "reference",
            "--expected",
            "wrong-answer",
        ],
    );
    assert_eq!(contradictory.status.code(), Some(1), "{contradictory:?}");
    let stderr = String::from_utf8(contradictory.stderr).unwrap();
    assert!(
        stderr.contains("reference and oracle solutions must have expected verdict accepted"),
        "{stderr}"
    );

    let listed = run_json(project.path(), &["solution", "list"]);
    assert!(listed.status.success(), "{listed:?}");
    let value: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let solution = value["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|solution| solution["program"]["name"] == "known-wrong")
        .unwrap();
    assert_eq!(solution["role"], "known_wrong");
    assert_eq!(solution["expected_verdict"], "wrong_answer");
}

#[test]
fn every_problem_type_starter_passes_the_new_static_semantics() {
    for problem_type in [
        "standard",
        "scored",
        "interactive",
        "output-only",
        "library",
        "grader",
    ] {
        let project = tempfile::tempdir().unwrap();
        let initialized = init(project.path(), problem_type);
        assert!(
            initialized.status.success(),
            "{problem_type}: {initialized:?}"
        );
        let checked = run(project.path(), &["check"]);
        assert!(checked.status.success(), "{problem_type}: {checked:?}");
    }
}

#[test]
fn static_semantics_require_a_reference_and_bounded_scored_points() {
    let standard = tempfile::tempdir().unwrap();
    assert!(init(standard.path(), "standard").status.success());
    for name in ["accepted", "accepted-alt", "known-wrong"] {
        let removed = run(standard.path(), &["solution", "remove", name]);
        assert!(removed.status.success(), "{name}: {removed:?}");
    }
    let missing_reference = run(standard.path(), &["check"]);
    assert_eq!(missing_reference.status.code(), Some(2));
    assert!(
        String::from_utf8(missing_reference.stderr)
            .unwrap()
            .contains("one accepted reference solution is required")
    );

    let scored = tempfile::tempdir().unwrap();
    assert!(init(scored.path(), "scored").status.success());
    let yaml_path = scored.path().join("reporch.yaml");
    let yaml = fs::read_to_string(&yaml_path).unwrap();
    let yaml = yaml.replacen("points: 50.0", "points: -10.0", 1);
    let yaml = yaml.replacen("points: 50.0", "points: 110.0", 1);
    fs::write(yaml_path, yaml).unwrap();
    let invalid_points = run(scored.path(), &["check"]);
    assert_eq!(invalid_points.status.code(), Some(2));
    assert!(
        String::from_utf8(invalid_points.stderr)
            .unwrap()
            .contains("points must be a finite value from 0 to 100")
    );
}
