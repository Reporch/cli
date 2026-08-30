use std::fs;
use std::process::{Command, Output};

use serde_json::Value;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
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

fn run_with_path(
    directory: &std::path::Path,
    arguments: &[&str],
    path: &std::path::Path,
) -> Output {
    reporch()
        .args(["--cwd", directory.to_str().unwrap()])
        .args(arguments)
        .env("PATH", path)
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
fn init_can_safely_use_an_existing_directory_with_explicit_opt_in() {
    let existing = tempfile::tempdir().unwrap();
    fs::write(existing.path().join("notes.txt"), "preserve me").unwrap();
    let initialized = run(
        existing.path(),
        &[
            "project",
            "init",
            "--title",
            "Existing directory",
            "--allow-non-empty",
        ],
    );
    assert!(initialized.status.success(), "{initialized:?}");
    assert_eq!(
        fs::read_to_string(existing.path().join("notes.txt")).unwrap(),
        "preserve me"
    );
    assert!(existing.path().join("reporch.yaml").is_file());

    let collision = tempfile::tempdir().unwrap();
    fs::create_dir_all(collision.path().join("statements")).unwrap();
    fs::write(collision.path().join("statements/ko.md"), "do not replace").unwrap();
    let refused = run(
        collision.path(),
        &[
            "project",
            "init",
            "--title",
            "Collision",
            "--allow-non-empty",
        ],
    );
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert!(stderr.contains("generated path already exists"), "{stderr}");
    assert!(stderr.contains("no files were written"), "{stderr}");
    assert!(!collision.path().join("reporch.yaml").exists());
    assert!(!collision.path().join("tests").exists());
    assert!(!collision.path().join(".reporch").exists());
    assert_eq!(
        fs::read_to_string(collision.path().join("statements/ko.md")).unwrap(),
        "do not replace"
    );
}

#[test]
fn init_rejects_stale_reporch_state_before_writing() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join(".reporch")).unwrap();
    fs::write(project.path().join(".reporch/state.json"), "{}").unwrap();

    let refused = run(
        project.path(),
        &[
            "project",
            "init",
            "--title",
            "Stale state",
            "--allow-non-empty",
        ],
    );
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert!(stderr.contains("existing .reporch/state.json"), "{stderr}");
    assert!(stderr.contains("no files were written"), "{stderr}");
    assert!(!project.path().join("reporch.yaml").exists());
    assert!(!project.path().join("statements").exists());
}

#[cfg(unix)]
#[test]
fn init_rolls_back_only_paths_created_by_a_failed_transaction() {
    use std::os::unix::fs::PermissionsExt as _;

    let project = tempfile::tempdir().unwrap();
    let statements = project.path().join("statements");
    let solutions = project.path().join("solutions");
    fs::create_dir_all(&statements).unwrap();
    fs::create_dir_all(&solutions).unwrap();
    fs::set_permissions(&solutions, fs::Permissions::from_mode(0o555)).unwrap();

    let refused = run(
        project.path(),
        &[
            "project",
            "init",
            "--title",
            "Rollback",
            "--allow-non-empty",
        ],
    );
    fs::set_permissions(&solutions, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(refused.status.code(), Some(5), "{refused:?}");
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert!(
        stderr.contains("every path created by this attempt was rolled back"),
        "{stderr}"
    );
    assert!(
        statements.is_dir(),
        "pre-existing empty directory was removed"
    );
    assert!(
        solutions.is_dir(),
        "pre-existing protected directory was removed"
    );
    assert!(!statements.join("ko.md").exists());
    assert!(!project.path().join("tests").exists());
    assert!(!project.path().join("reporch.yaml").exists());
}

#[cfg(unix)]
#[test]
fn init_rejects_symlinked_template_parents_even_with_non_empty_opt_in() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), project.path().join("statements")).unwrap();
    let refused = run(
        project.path(),
        &[
            "project",
            "init",
            "--title",
            "Symlink collision",
            "--allow-non-empty",
        ],
    );
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert!(stderr.contains("unsafe parent component"), "{stderr}");
    assert!(!outside.path().join("ko.md").exists());
    assert!(!project.path().join("reporch.yaml").exists());

    let parent = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    symlink(target.path(), parent.path().join("linked-project")).unwrap();
    let refused_root = reporch()
        .args([
            "--cwd",
            parent.path().to_str().unwrap(),
            "project",
            "init",
            "--title",
            "Symlink root",
            "--directory",
            "linked-project",
            "--allow-non-empty",
        ])
        .output()
        .unwrap();
    assert_eq!(refused_root.status.code(), Some(2), "{refused_root:?}");
    assert!(
        String::from_utf8(refused_root.stderr)
            .unwrap()
            .contains("project directory must be a real directory")
    );
    assert!(!target.path().join("reporch.yaml").exists());
}

#[test]
fn manifest_commands_default_to_the_current_authoring_file() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "standard").status.success());
    let nested = project.path().join("nested/worktree");
    fs::create_dir_all(&nested).unwrap();

    for command in ["validate", "digest"] {
        let output = run(&nested, &["manifest", command]);
        assert!(output.status.success(), "{command}: {output:?}");
    }
    let compatibility = run(&nested, &["manifest", "compatibility"]);
    assert!(compatibility.status.success(), "{compatibility:?}");

    for profile in [
        "reporch_native",
        "icpc_202509",
        "icpc_legacy",
        "polygon_compatible",
        "domjudge_zip",
    ] {
        let underscore_alias = run(
            &nested,
            &["--profile", profile, "manifest", "compatibility"],
        );
        assert!(
            underscore_alias.status.success(),
            "{profile}: {underscore_alias:?}"
        );
    }
}

#[test]
fn literal_test_case_input_is_unambiguous_and_written_safely() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "standard").status.success());

    let help = run(project.path(), &["test", "case", "add", "--help"]);
    assert!(help.status.success(), "{help:?}");
    let help = String::from_utf8(help.stdout).unwrap();
    for expected in [
        "--input <INPUT_FILE>",
        "--input-text <TEXT>",
        "--answer-text <TEXT>",
    ] {
        assert!(help.contains(expected), "missing {expected:?}:\n{help}");
    }

    let added = run_json(
        project.path(),
        &[
            "test",
            "case",
            "add",
            "--name",
            "literal",
            "--input-text",
            "9 4",
            "--answer-text",
            "13",
        ],
    );
    assert!(added.status.success(), "{added:?}");
    let envelope: Value = serde_json::from_slice(&added.stdout).unwrap();
    let added_case = envelope["data"].as_array().unwrap().last().unwrap();
    let input = added_case["input_file"].as_str().unwrap();
    let answer = added_case["answer_file"].as_str().unwrap();
    assert_eq!(
        fs::read_to_string(project.path().join(input)).unwrap(),
        "9 4"
    );
    assert_eq!(
        fs::read_to_string(project.path().join(answer)).unwrap(),
        "13"
    );
    assert!(run(project.path(), &["check"]).status.success());

    let ambiguous = run(
        project.path(),
        &[
            "test",
            "case",
            "add",
            "--name",
            "ambiguous",
            "--input",
            "tests/1.in",
            "--input-text",
            "1",
        ],
    );
    assert_eq!(ambiguous.status.code(), Some(2), "{ambiguous:?}");

    let before = fs::read_dir(project.path().join("tests/manual"))
        .unwrap()
        .count();
    let duplicate = run(
        project.path(),
        &[
            "test",
            "case",
            "add",
            "--name",
            "literal",
            "--input-text",
            "must roll back",
        ],
    );
    assert_eq!(duplicate.status.code(), Some(2), "{duplicate:?}");
    assert_eq!(
        fs::read_dir(project.path().join("tests/manual"))
            .unwrap()
            .count(),
        before,
        "a failed manifest update left a generated input behind"
    );
}

#[test]
fn validator_units_accept_literal_text_without_treating_it_as_a_path() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "standard").status.success());
    let added = run_json(
        project.path(),
        &[
            "validator",
            "unit-add",
            "--name",
            "literal-valid",
            "--input-text",
            "1 2",
            "--expected",
            "valid",
        ],
    );
    assert!(added.status.success(), "{added:?}");
    let envelope: Value = serde_json::from_slice(&added.stdout).unwrap();
    let unit = envelope["data"].as_array().unwrap().last().unwrap();
    let path = unit["input_file"].as_str().unwrap();
    assert!(path.starts_with("validator-tests/"), "{path}");
    assert_eq!(
        fs::read_to_string(project.path().join(path)).unwrap(),
        "1 2"
    );

    let help = run(project.path(), &["validator", "unit-add", "--help"]);
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--input <INPUT_FILE>"), "{help}");
    assert!(help.contains("--input-text <TEXT>"), "{help}");
}

#[test]
fn package_export_uses_project_defaults_and_reports_structured_recovery() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "standard").status.success());

    let exported = run_json(
        project.path(),
        &["package", "export", "reporch.yaml", "native.rpk"],
    );
    assert!(exported.status.success(), "{exported:?}");
    let envelope: Value = serde_json::from_slice(&exported.stdout).unwrap();
    assert_eq!(envelope["data"]["profile"], "reporch_native");
    assert!(project.path().join("native.rpk").is_file());

    let stale = run_json(
        project.path(),
        &["package", "export", "reporch.yaml", "native.rpk"],
    );
    assert_eq!(stale.status.code(), Some(2), "{stale:?}");
    let error: Value = serde_json::from_slice(&stale.stderr).unwrap();
    assert!(error["message"].as_str().unwrap().contains("may be stale"));
    assert_eq!(
        error["details"]["schema"],
        "reporch.package-destination-conflict.v1"
    );
    assert!(error["details"]["current_manifest_digest"].is_string());

    let imported = run_json(
        project.path(),
        &[
            "--profile",
            "reporch-native",
            "package",
            "import",
            "native.rpk",
            "imported",
        ],
    );
    assert!(imported.status.success(), "{imported:?}");
    let import_conflict = run_json(
        project.path(),
        &[
            "--profile",
            "reporch-native",
            "package",
            "import",
            "native.rpk",
            "imported",
        ],
    );
    assert_eq!(
        import_conflict.status.code(),
        Some(2),
        "{import_conflict:?}"
    );
    let error: Value = serde_json::from_slice(&import_conflict.stderr).unwrap();
    assert_eq!(
        error["details"]["schema"],
        "reporch.package-import-destination-conflict.v1"
    );
    assert_eq!(error["details"]["recovery"], "choose_new_empty_directory");

    let blocked = run_json(
        project.path(),
        &[
            "--profile",
            "polygon-compatible",
            "manifest",
            "compatibility",
            "--require-exportable",
        ],
    );
    assert_eq!(blocked.status.code(), Some(1), "{blocked:?}");
    let error: Value = serde_json::from_slice(&blocked.stderr).unwrap();
    assert_eq!(error["error_code"], "operation.failed");
    assert_eq!(
        error["details"]["schema"],
        "reporch.compatibility-report.v1"
    );
    assert_eq!(error["details"]["target_profile"], "polygon_compatible");
    assert_eq!(error["details"]["exportable"], false);
}

#[test]
fn scored_solution_ranges_update_without_repeating_the_verdict() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "scored").status.success());
    let updated = run_json(
        project.path(),
        &[
            "solution",
            "update",
            "partial-50",
            "--minimum-score",
            "40",
            "--maximum-score",
            "70",
        ],
    );
    assert!(updated.status.success(), "{updated:?}");
    let listed = run_json(project.path(), &["solution", "list"]);
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let partial = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|solution| solution["program"]["name"] == "partial-50")
        .unwrap();
    assert_eq!(partial["expected_score"]["minimum"], 40.0);
    assert_eq!(partial["expected_score"]["maximum"], 70.0);

    let invalid = run(
        project.path(),
        &[
            "solution",
            "update",
            "accepted",
            "--minimum-score",
            "80",
            "--maximum-score",
            "100",
        ],
    );
    assert_eq!(invalid.status.code(), Some(2), "{invalid:?}");
    assert!(
        String::from_utf8(invalid.stderr)
            .unwrap()
            .contains("score range is only valid for partial solutions")
    );
}

#[test]
fn scored_group_help_calls_the_positional_value_a_name() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "scored").status.success());
    let help = run(project.path(), &["test", "group", "add", "--help"]);
    assert!(help.status.success(), "{help:?}");
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("<NAME>"), "{help}");
    assert!(!help.contains("<ID>"), "{help}");
}

#[test]
fn schema_mismatch_points_to_the_declared_version_instead_of_a_random_field() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "library").status.success());
    let yaml_path = project.path().join("reporch.yaml");
    let yaml = fs::read_to_string(&yaml_path).unwrap().replacen(
        "reporch.authoring-spec.v2",
        "reporch.authoring-spec.v1",
        1,
    );
    fs::write(yaml_path, yaml).unwrap();

    let checked = run(project.path(), &["check"]);
    assert_eq!(checked.status.code(), Some(2), "{checked:?}");
    let stderr = String::from_utf8(checked.stderr).unwrap();
    assert!(
        stderr.contains("does not belong to declared schema"),
        "{stderr}"
    );
    assert!(stderr.contains("reporch.authoring-spec.v2"), "{stderr}");
}

#[test]
fn verify_failure_lists_the_complete_recovery_sequence() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "standard").status.success());
    let verified = run_json(project.path(), &["verify"]);
    assert_eq!(verified.status.code(), Some(2), "{verified:?}");
    let error: Value = serde_json::from_slice(&verified.stderr).unwrap();
    for expected in [
        "reporch auth login",
        "reporch project create",
        "reporch project link",
        "reporch project push",
        "reporch verify",
    ] {
        assert!(
            error["message"].as_str().unwrap().contains(expected),
            "missing {expected:?}: {error}"
        );
    }
}

#[test]
fn output_mismatch_error_includes_actual_verdict_score_and_structured_report() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "output-only").status.success());
    let yaml_path = project.path().join("reporch.yaml");
    let mut yaml = fs::read_to_string(&yaml_path).unwrap();
    let marker = "  name: known-wrong";
    let section = yaml.find(marker).unwrap();
    let verdict = yaml[section..]
        .find("expected_verdict: wrong_answer")
        .unwrap()
        + section;
    yaml.replace_range(
        verdict..verdict + "expected_verdict: wrong_answer".len(),
        "expected_verdict: accepted",
    );
    fs::write(yaml_path, yaml).unwrap();

    let tested = run_json(project.path(), &["output", "test"]);
    assert_eq!(tested.status.code(), Some(1), "{tested:?}");
    let error: Value = serde_json::from_slice(&tested.stderr).unwrap();
    assert_eq!(error["error_code"], "operation.failed");
    let message = error["message"].as_str().unwrap();
    for expected in [
        "known-wrong",
        "expected accepted",
        "actual wrong_answer",
        "score 0",
    ] {
        assert!(
            message.contains(expected),
            "missing {expected:?}: {message}"
        );
    }
    assert_eq!(error["details"]["schema"], "reporch.output-test-report.v1");
    assert_eq!(error["details"]["passed"], false);
    assert_eq!(error["details"]["submissions"][1]["actual"], "wrong_answer");
    assert_eq!(error["details"]["submissions"][1]["score"], 0.0);
}

#[test]
fn library_solution_commands_stay_on_authoring_v2_without_manual_migration() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "library").status.success());
    fs::write(
        project.path().join("solutions/alternative.cpp"),
        "int solve(int a, int b) { return a + b; }\n",
    )
    .unwrap();

    let added = run_json(
        project.path(),
        &[
            "solution",
            "add",
            "--name",
            "alternative-library",
            "--source",
            "solutions/alternative.cpp",
            "--language",
            "cpp",
            "--expected",
            "accepted",
            "--role",
            "alternative",
        ],
    );
    assert!(added.status.success(), "{added:?}");
    let matrix = run_json(project.path(), &["solution", "matrix"]);
    assert!(matrix.status.success(), "{matrix:?}");
    let matrix: Value = serde_json::from_slice(&matrix.stdout).unwrap();
    assert!(matrix["data"].as_array().unwrap().iter().any(|solution| {
        solution["program"]["name"] == "alternative-library" && solution["role"] == "alternative"
    }));
    assert!(
        fs::read_to_string(project.path().join("reporch.yaml"))
            .unwrap()
            .contains("schema: reporch.authoring-spec.v2")
    );
    assert!(run(project.path(), &["check"]).status.success());
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
fn solution_matrix_is_readable_and_explicitly_non_executing() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "scored").status.success());

    let matrix = run(project.path(), &["solution", "matrix"]);
    assert!(matrix.status.success(), "{matrix:?}");
    let stdout = String::from_utf8(matrix.stdout).unwrap();
    for expected in [
        "role reference",
        "role known-wrong",
        "score 50..50",
        "expectations only",
        "reporch verify",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?}:\n{stdout}");
    }

    fs::write(project.path().join("solutions/control.py"), "print(0)\n").unwrap();
    let rejected_name = run(
        project.path(),
        &[
            "solution",
            "add",
            "--name",
            "bad\u{1b}[2J",
            "--source",
            "solutions/control.py",
            "--language",
            "python3",
            "--expected",
            "wrong-answer",
            "--role",
            "known-wrong",
        ],
    );
    assert_eq!(rejected_name.status.code(), Some(2), "{rejected_name:?}");
    assert!(
        String::from_utf8(rejected_name.stderr)
            .unwrap()
            .contains("no control characters")
    );

    let yaml_path = project.path().join("reporch.yaml");
    let yaml = fs::read_to_string(&yaml_path).unwrap().replacen(
        "name: accepted",
        "name: \"\\u001b[2J\"",
        1,
    );
    fs::write(&yaml_path, yaml).unwrap();
    let escaped = run(project.path(), &["solution", "matrix"]);
    assert!(escaped.status.success(), "{escaped:?}");
    let stdout = String::from_utf8(escaped.stdout).unwrap();
    assert!(
        !stdout.contains('\u{1b}'),
        "terminal escape leaked: {stdout:?}"
    );
    assert!(
        stdout.contains("\\u{1b}[2J"),
        "control was not escaped: {stdout:?}"
    );
}

#[test]
fn interactive_and_grader_runs_accept_names_uuids_and_paths_without_parser_errors() {
    for (problem_type, command) in [("interactive", "interactor"), ("grader", "grader")] {
        let project = tempfile::tempdir().unwrap();
        assert!(init(project.path(), problem_type).status.success());
        let spec = reporch_cli::local_project_v2::read_authoring_spec(project.path()).unwrap();
        let solution_id = spec.testing.solutions[0].program.id.to_string();
        let test_id = spec.testing.tests[0].id.to_string();

        let unknown_solution = run(
            project.path(),
            &[
                command,
                "run",
                "--solution",
                "solutions/missing.cpp",
                "--test",
                "tests/1.in",
            ],
        );
        assert_eq!(
            unknown_solution.status.code(),
            Some(2),
            "{unknown_solution:?}"
        );
        let stderr = String::from_utf8(unknown_solution.stderr).unwrap();
        assert!(stderr.contains("name, UUID, or source path"), "{stderr}");

        let unknown_test = run(
            project.path(),
            &[
                command,
                "run",
                "--solution",
                "solutions/accepted.cpp",
                "--test",
                "tests/missing.in",
            ],
        );
        assert_eq!(unknown_test.status.code(), Some(2), "{unknown_test:?}");
        let stderr = String::from_utf8(unknown_test.stderr).unwrap();
        assert!(stderr.contains("name, UUID, or input path"), "{stderr}");
        assert!(!stderr.contains("invalid character"), "{stderr}");

        let no_runtime = tempfile::tempdir().unwrap();
        for (solution, test) in [
            ("accepted", "sample-1"),
            ("solutions/accepted.cpp", "tests/1.in"),
            (solution_id.as_str(), test_id.as_str()),
        ] {
            let valid_selectors = run_with_path(
                project.path(),
                &[
                    command,
                    "run",
                    "--solution",
                    solution,
                    "--test",
                    test,
                    "--runtime",
                    "podman",
                ],
                no_runtime.path(),
            );
            assert_eq!(
                valid_selectors.status.code(),
                Some(2),
                "{valid_selectors:?}"
            );
            let stderr = String::from_utf8(valid_selectors.stderr).unwrap();
            assert!(stderr.contains("reporch verify"), "{stderr}");
            assert!(stderr.contains("podman machine init"), "{stderr}");
            assert!(
                stderr.contains("never runs author code directly on the host"),
                "{stderr}"
            );
            assert!(!stderr.contains("was not found"), "{stderr}");
            assert!(!stderr.contains("invalid character"), "{stderr}");
        }
    }
}

#[test]
fn readable_runtime_selectors_fail_closed_on_name_path_ambiguity() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "interactive").status.success());
    fs::write(
        project.path().join("solutions/collision.cpp"),
        "#include <iostream>\nint main(){ return 0; }\n",
    )
    .unwrap();
    let added = run(
        project.path(),
        &[
            "solution",
            "add",
            "--name",
            "solutions/accepted.cpp",
            "--source",
            "solutions/collision.cpp",
            "--language",
            "cpp",
            "--expected",
            "accepted",
            "--role",
            "alternative",
        ],
    );
    assert!(added.status.success(), "{added:?}");

    let ambiguous_solution = run(
        project.path(),
        &[
            "interactor",
            "run",
            "--solution",
            "solutions/accepted.cpp",
            "--test",
            "sample-1",
        ],
    );
    assert_eq!(
        ambiguous_solution.status.code(),
        Some(2),
        "{ambiguous_solution:?}"
    );
    let stderr = String::from_utf8(ambiguous_solution.stderr).unwrap();
    assert!(stderr.contains("ambiguous solution selector"), "{stderr}");
    assert!(stderr.contains("exact UUID"), "{stderr}");

    let removed = run(
        project.path(),
        &["solution", "remove", "solutions/accepted.cpp"],
    );
    assert!(removed.status.success(), "{removed:?}");
    let listed = run_json(project.path(), &["solution", "list"]);
    let value: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let solutions = value["data"].as_array().unwrap();
    assert!(solutions.iter().any(|solution| {
        solution["program"]["name"] == "accepted"
            && solution["program"]["source_path"] == "solutions/accepted.cpp"
    }));
    assert!(
        !solutions
            .iter()
            .any(|solution| { solution["program"]["source_path"] == "solutions/collision.cpp" })
    );

    fs::write(project.path().join("tests/2.in"), "3\n").unwrap();
    fs::write(project.path().join("tests/2.ans"), "6\n").unwrap();
    let added_test = run(
        project.path(),
        &[
            "test",
            "case",
            "add",
            "--name",
            "tests/1.in",
            "--input",
            "tests/2.in",
            "--answer",
            "tests/2.ans",
        ],
    );
    assert!(added_test.status.success(), "{added_test:?}");
    let ambiguous_test = run(
        project.path(),
        &[
            "interactor",
            "run",
            "--solution",
            "accepted",
            "--test",
            "tests/1.in",
        ],
    );
    assert_eq!(ambiguous_test.status.code(), Some(2), "{ambiguous_test:?}");
    let stderr = String::from_utf8(ambiguous_test.stderr).unwrap();
    assert!(stderr.contains("ambiguous test selector"), "{stderr}");
    assert!(stderr.contains("exact UUID"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn local_runtime_guidance_rejects_a_non_rootless_daemon() {
    use std::os::unix::fs::PermissionsExt as _;

    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "interactive").status.success());
    let runtime = tempfile::tempdir().unwrap();
    let podman = runtime.path().join("podman");
    fs::write(
        &podman,
        "#!/bin/sh\ncase \"$1\" in\n  --version) exit 0 ;;\n  info) printf '%s\\n' '{\"host\":{\"security\":{\"rootless\":false}}}' ;;\n  *) exit 64 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&podman, fs::Permissions::from_mode(0o755)).unwrap();

    let refused = run_with_path(
        project.path(),
        &[
            "interactor",
            "run",
            "--solution",
            "accepted",
            "--test",
            "sample-1",
            "--runtime",
            "podman",
        ],
        runtime.path(),
    );
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert!(
        stderr.contains("requires a rootless Podman or Docker daemon"),
        "{stderr}"
    );
    assert!(
        stderr.contains("never runs author code directly on the host"),
        "{stderr}"
    );
    assert!(stderr.contains("reporch verify"), "{stderr}");

    fs::write(
        &podman,
        "#!/bin/sh\ncase \"$1\" in\n  --version) exit 0 ;;\n  info) exit 1 ;;\n  *) exit 64 ;;\nesac\n",
    )
    .unwrap();
    let inspection_failed = run_with_path(
        project.path(),
        &[
            "interactor",
            "run",
            "--solution",
            "accepted",
            "--test",
            "sample-1",
            "--runtime",
            "podman",
        ],
        runtime.path(),
    );
    assert_eq!(
        inspection_failed.status.code(),
        Some(2),
        "{inspection_failed:?}"
    );
    let stderr = String::from_utf8(inspection_failed.stderr).unwrap();
    assert!(stderr.contains("security inspection failed"), "{stderr}");
    assert!(
        stderr.contains("never runs author code directly on the host"),
        "{stderr}"
    );
    assert!(stderr.contains("reporch verify"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn hanging_oci_security_probe_returns_retryable_json_and_leaves_no_child() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};

    let project = tempfile::tempdir().unwrap();
    for (hang_on, maximum) in [
        ("--version", Duration::from_secs(7)),
        ("info", Duration::from_secs(12)),
    ] {
        let runtime = tempfile::tempdir().unwrap();
        let docker = runtime.path().join("docker");
        let child_pid = runtime.path().join("child.pid");
        let script = if hang_on == "--version" {
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  --version) /bin/sleep 30 & echo $! > '{}' ; wait ;;\n  info) printf '%s\\n' '[\"name=rootless\"]' ;;\n  *) exit 64 ;;\nesac\n",
                child_pid.display()
            )
        } else {
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  --version) exit 0 ;;\n  info) /bin/sleep 30 & echo $! > '{}' ; wait ;;\n  *) exit 64 ;;\nesac\n",
                child_pid.display()
            )
        };
        fs::write(&docker, script).unwrap();
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();

        let started = Instant::now();
        let output = reporch()
            .args([
                "--cwd",
                project.path().to_str().unwrap(),
                "--format",
                "json",
                "--no-input",
                "sandbox",
                "plan",
                "--runtime",
                "docker",
                "--image",
                &format!("registry.test/toolchain@sha256:{}", "a".repeat(64)),
                "--project-directory",
                project.path().to_str().unwrap(),
                "--",
                "/bin/true",
            ])
            .env("PATH", runtime.path())
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(6), "{hang_on}: {output:?}");
        assert!(started.elapsed() < maximum, "{hang_on}: {output:?}");
        let envelope: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(envelope["schema"], "reporch.cli-error.v1");
        assert_eq!(envelope["error_code"], "runtime.service_unavailable");
        assert_eq!(envelope["retryable"], true);
        let message = envelope["message"].as_str().unwrap();
        assert!(
            message.contains("start the rootless Docker daemon"),
            "{message}"
        );
        assert!(message.contains("reporch verify"), "{message}");

        let pid = fs::read_to_string(&child_pid).unwrap();
        let pid = pid.trim();
        for attempt in 0..20 {
            if !std::process::Command::new("/bin/kill")
                .args(["-0", pid])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
            {
                break;
            }
            assert!(
                attempt < 19,
                "bounded {hang_on} probe left child {pid} running"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn legacy_1x_aliases_keep_readable_matrix_selectors_and_safe_output_pruning() {
    let scored = tempfile::tempdir().unwrap();
    reporch_cli::init_legacy_v1_project_template(
        scored.path(),
        "Legacy scored",
        uuid::Uuid::now_v7(),
        studio_core::ProblemType::Scored,
    )
    .unwrap();
    let matrix = run(scored.path(), &["solution", "matrix"]);
    assert!(matrix.status.success(), "{matrix:?}");
    let stdout = String::from_utf8(matrix.stdout).unwrap();
    for expected in [
        "partial-50",
        "score 50..50",
        "solutions/partial.py",
        "expectations only",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?}:\n{stdout}");
    }

    let interactive = tempfile::tempdir().unwrap();
    reporch_cli::init_legacy_v1_project_template(
        interactive.path(),
        "Legacy interactive",
        uuid::Uuid::now_v7(),
        studio_core::ProblemType::Interactive,
    )
    .unwrap();
    let legacy_spec = reporch_cli::local_project::read_authoring_spec(interactive.path()).unwrap();
    let test_id = legacy_spec.judging.tests[0].id.to_string();
    let no_runtime = tempfile::tempdir().unwrap();
    for test in ["tests/1.in", test_id.as_str()] {
        let selected = run_with_path(
            interactive.path(),
            &[
                "interactor",
                "run",
                "--solution",
                "solutions/accepted.cpp",
                "--test",
                test,
                "--runtime",
                "podman",
            ],
            no_runtime.path(),
        );
        assert_eq!(selected.status.code(), Some(2), "{selected:?}");
        let stderr = String::from_utf8(selected.stderr).unwrap();
        assert!(stderr.contains("reporch verify"), "{stderr}");
        assert!(!stderr.contains("was not found"), "{stderr}");
    }

    let output_only = tempfile::tempdir().unwrap();
    reporch_cli::init_legacy_v1_project_template(
        output_only.path(),
        "Legacy output",
        uuid::Uuid::now_v7(),
        studio_core::ProblemType::OutputOnly,
    )
    .unwrap();
    let removed = run(output_only.path(), &["output", "remove", "known-wrong"]);
    assert!(removed.status.success(), "{removed:?}");
    let yaml = fs::read_to_string(output_only.path().join("reporch.yaml")).unwrap();
    assert!(!yaml.contains("outputs/known-wrong.txt"), "{yaml}");
    assert!(output_only.path().join("outputs/known-wrong.txt").is_file());
}

#[test]
fn output_remove_prunes_only_unused_declarations_and_leaves_files_on_disk() {
    let project = tempfile::tempdir().unwrap();
    assert!(init(project.path(), "output-only").status.success());
    let output_path = project.path().join("outputs/known-wrong.txt");
    assert!(output_path.is_file());

    let removed = run(project.path(), &["output", "remove", "known-wrong"]);
    assert!(removed.status.success(), "{removed:?}");
    let stdout = String::from_utf8(removed.stdout).unwrap();
    assert!(
        stdout.contains("Pruned 1 unused file declaration"),
        "{stdout}"
    );
    assert!(stdout.contains("files remain on disk"), "{stdout}");
    assert!(output_path.is_file());

    fs::remove_file(output_path).unwrap();
    let checked = run(project.path(), &["check"]);
    assert!(checked.status.success(), "{checked:?}");
    let yaml = fs::read_to_string(project.path().join("reporch.yaml")).unwrap();
    assert!(!yaml.contains("outputs/known-wrong.txt"), "{yaml}");

    let shared = tempfile::tempdir().unwrap();
    assert!(init(shared.path(), "output-only").status.success());
    let yaml_path = shared.path().join("reporch.yaml");
    let mut yaml = fs::read_to_string(&yaml_path).unwrap();
    let official = "outputs/official.txt";
    let index = yaml.rfind(official).unwrap();
    yaml.replace_range(index..index + official.len(), "outputs/known-wrong.txt");
    fs::write(&yaml_path, yaml).unwrap();
    let removed = run(shared.path(), &["output", "remove", "known-wrong"]);
    assert!(removed.status.success(), "{removed:?}");
    let stdout = String::from_utf8(removed.stdout).unwrap();
    assert!(
        stdout.contains("Pruned 0 unused file declaration"),
        "{stdout}"
    );
    let yaml = fs::read_to_string(yaml_path).unwrap();
    assert!(yaml.contains("outputs/known-wrong.txt"), "{yaml}");

    let referenced = tempfile::tempdir().unwrap();
    assert!(init(referenced.path(), "output-only").status.success());
    let yaml_path = referenced.path().join("reporch.yaml");
    let yaml = fs::read_to_string(&yaml_path).unwrap().replacen(
        "answer_file: tests/1.ans",
        "answer_file: outputs/known-wrong.txt",
        1,
    );
    fs::write(&yaml_path, yaml).unwrap();
    let removed = run(referenced.path(), &["output", "remove", "known-wrong"]);
    assert!(removed.status.success(), "{removed:?}");
    let stdout = String::from_utf8(removed.stdout).unwrap();
    assert!(
        stdout.contains("Pruned 0 unused file declaration"),
        "{stdout}"
    );
    let yaml = fs::read_to_string(yaml_path).unwrap();
    assert!(yaml.contains("outputs/known-wrong.txt"), "{yaml}");
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
