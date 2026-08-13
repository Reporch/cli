use std::fs;
use std::process::Command;

use serde_json::Value;

fn reporch() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reporch"))
}

fn assert_help_commands(arguments: &[&str], expected: &[&str]) {
    let output = reporch().args(arguments).arg("--help").output().unwrap();
    assert!(output.status.success(), "{arguments:?}: {output:?}");
    assert!(output.stderr.is_empty(), "{arguments:?}: {output:?}");
    let help = String::from_utf8(output.stdout).unwrap();
    for command in expected {
        assert!(
            help.lines().any(|line| {
                let line = line.trim_start();
                line == *command
                    || line
                        .strip_prefix(command)
                        .and_then(|suffix| suffix.chars().next())
                        .is_some_and(char::is_whitespace)
            }),
            "missing stable command {arguments:?} {command}:\n{help}"
        );
    }
}

#[test]
fn json_mode_emits_one_stable_success_envelope() {
    let temporary = tempfile::tempdir().unwrap();
    let output = reporch()
        .args([
            "--format",
            "json",
            "project",
            "init",
            "--title",
            "CLI contract",
            "--directory",
            temporary.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema"], "reporch.cli-result.v1");
    assert_eq!(value["command"], "project init");
    assert_eq!(value["data"]["dirty"], false);
    assert!(temporary.path().join("reporch.yaml").is_file());
    assert!(temporary.path().join("reporch.problem.json").is_file());
}

#[test]
fn json_parse_errors_use_exit_two_and_the_error_envelope() {
    let output = reporch()
        .args(["--format", "json", "project", "unknown"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["schema"], "reporch.cli-error.v1");
    assert_eq!(value["command"], "parse");
    assert_eq!(value["error_code"], "input.invalid");
    assert_eq!(value["retryable"], false);
}

#[test]
fn help_is_successful_and_never_emits_an_error_envelope() {
    for arguments in [vec!["--help"], vec!["--format", "json", "--help"]] {
        let output = reporch().args(arguments).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Usage: reporch"),
            "{output:?}"
        );
    }
}

#[test]
fn the_documented_1_x_command_surface_cannot_be_removed_accidentally() {
    assert_help_commands(
        &[],
        &[
            "migrate",
            "check",
            "statement",
            "test",
            "generator",
            "validator",
            "checker",
            "solution",
            "interactor",
            "grader",
            "output",
            "verify",
            "submit",
            "auth",
            "project",
            "member",
            "doctor",
            "completion",
            "quota",
            "release",
            "publication",
            "validation",
            "events",
            "waiver",
            "revision",
            "review",
            "manifest",
            "package",
            "sandbox",
            "toolchain",
            "desktop",
            "artifact",
        ],
    );
    for (arguments, expected) in [
        (&["auth"][..], &["login", "status", "logout"][..]),
        (
            &["project"][..],
            &[
                "init", "link", "list", "show", "open", "status", "diff", "create", "pull", "push",
                "validate", "package",
            ][..],
        ),
        (
            &["member"][..],
            &["search", "list", "add", "update", "remove"][..],
        ),
        (
            &["review"][..],
            &[
                "submit",
                "list",
                "request",
                "inbox",
                "status",
                "claim",
                "cancel",
                "approve",
                "request-changes",
            ][..],
        ),
        (&["waiver"][..], &["list", "create", "revoke"][..]),
        (&["validation"][..], &["list", "show", "watch"][..]),
        (&["events"][..], &["watch"][..]),
        (&["release"][..], &["build", "list", "show", "download"][..]),
        (&["publication"][..], &["publish", "status"][..]),
        (&["revision"][..], &["list", "show", "diff", "restore"][..]),
        (
            &["manifest"][..],
            &["validate", "digest", "compatibility"][..],
        ),
        (&["package"][..], &["export", "import"][..]),
        (&["sandbox"][..], &["plan", "run"][..]),
        (&["toolchain"][..], &["list", "inspect", "install"][..]),
        (&["quota"][..], &["show"][..]),
    ] {
        assert_help_commands(arguments, expected);
    }
}

#[test]
fn named_profiles_reexec_safely_and_unknown_profiles_use_the_error_contract() {
    let config_home = tempfile::tempdir().unwrap();
    fs::write(
        config_home.path().join("config.toml"),
        r#"version = 1
[profiles.production]
studio_api_url = "https://studio.reporch.com"
oidc_issuer = "https://reporch.com/oauth"
cli_client_id = "reporch-studio-cli"
allow_insecure_http = false
"#,
    )
    .unwrap();

    let help = reporch()
        .args(["--profile", "production", "--help"])
        .env("REPORCH_CONFIG_HOME", config_home.path())
        .output()
        .unwrap();
    assert!(help.status.success(), "{help:?}");
    assert!(help.stderr.is_empty(), "{help:?}");
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: reporch"));

    let missing = reporch()
        .args(["--format", "json", "--profile", "missing", "--help"])
        .env("REPORCH_CONFIG_HOME", config_home.path())
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(2), "{missing:?}");
    assert!(missing.stdout.is_empty(), "{missing:?}");
    let error: Value = serde_json::from_slice(&missing.stderr).unwrap();
    assert_eq!(error["schema"], "reporch.cli-error.v1");
    assert_eq!(error["command"], "configuration");
    assert_eq!(error["error_code"], "configuration.invalid");
    assert_eq!(error["retryable"], false);
}

#[test]
fn review_pool_commands_are_explicit_and_parse_without_membership_fields() {
    let review_id = "019f8fc9-cff3-7421-8cf8-0661a7a484dd";
    let missing_pool = reporch()
        .args([
            "--format",
            "json",
            "review",
            "request",
            "--review-id",
            review_id,
        ])
        .output()
        .unwrap();
    assert_eq!(missing_pool.status.code(), Some(2), "{missing_pool:?}");
    let error: Value = serde_json::from_slice(&missing_pool.stderr).unwrap();
    assert_eq!(error["error_code"], "input.invalid");

    for arguments in [
        vec!["review", "request", "--help"],
        vec!["review", "inbox", "--help"],
        vec!["review", "claim", "--help"],
        vec!["review", "approve", "--help"],
    ] {
        let output = reporch().args(arguments).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(help.contains("review"), "{output:?}");
    }
}

#[test]
fn immutable_release_commands_are_first_class_and_non_destructive_by_default() {
    for arguments in [
        vec!["release", "build", "--help"],
        vec!["release", "list", "--help"],
        vec!["release", "show", "--help"],
        vec!["release", "download", "--help"],
    ] {
        let output = reporch().args(arguments).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
    }

    let help = reporch()
        .args(["release", "download", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("--output"), "{help}");
    assert!(!help.contains("--force"), "{help}");
}

#[test]
fn events_watch_documents_bounded_json_and_cursor_resume() {
    let output = reporch()
        .args(["events", "watch", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let help = String::from_utf8_lossy(&output.stdout);
    for option in ["--cursor", "--project-id", "--max-events"] {
        assert!(help.contains(option), "missing {option}: {help}");
    }
}

#[test]
fn check_is_networkless_and_finds_the_project_from_a_child_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let init = reporch()
        .args([
            "--quiet",
            "project",
            "init",
            "--title",
            "Nested check",
            "--directory",
            temporary.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");
    let child = temporary.path().join("solutions");
    let output = reporch()
        .args([
            "--cwd",
            child.to_str().unwrap(),
            "--format",
            "json",
            "check",
        ])
        .env("REPORCH_STUDIO_API_URL", "http://127.0.0.1:1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["command"], "check");
    assert_eq!(value["data"]["valid"], true);
}

#[test]
fn migrate_requires_yes_in_ci_and_is_idempotent() {
    let temporary = tempfile::tempdir().unwrap();
    reporch()
        .args([
            "--quiet",
            "project",
            "init",
            "--title",
            "Legacy",
            "--directory",
            temporary.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    fs::remove_file(temporary.path().join("reporch.yaml")).unwrap();

    let refused = reporch()
        .args([
            "--format",
            "json",
            "migrate",
            "--directory",
            temporary.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");

    for expected_migrated in [true, false] {
        let output = reporch()
            .args([
                "--yes",
                "--format",
                "json",
                "migrate",
                "--directory",
                temporary.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["data"]["migrated"], expected_migrated);
    }
    assert!(
        temporary
            .path()
            .join("reporch.problem.pre-1.0.json")
            .is_file()
    );
}

#[test]
fn authoring_commands_update_yaml_atomically_and_keep_stable_ids() {
    let temporary = tempfile::tempdir().unwrap();
    let init = reporch()
        .args([
            "--quiet",
            "project",
            "init",
            "--title",
            "Authoring",
            "--directory",
            temporary.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");

    let group = reporch()
        .args([
            "--cwd",
            temporary.path().to_str().unwrap(),
            "--format",
            "json",
            "test",
            "group",
            "add",
            "edge",
            "--points",
            "25",
        ])
        .output()
        .unwrap();
    assert!(group.status.success(), "{group:?}");

    fs::write(temporary.path().join("tests/2.in"), b"0 0\n").unwrap();
    fs::write(temporary.path().join("tests/2.ans"), b"0\n").unwrap();
    let case = reporch()
        .args([
            "--cwd",
            temporary.path().to_str().unwrap(),
            "--format",
            "json",
            "test",
            "case",
            "add",
            "--name",
            "zero",
            "--input",
            "tests/2.in",
            "--answer",
            "tests/2.ans",
            "--group",
            "edge",
        ])
        .output()
        .unwrap();
    assert!(case.status.success(), "{case:?}");
    let value: Value = serde_json::from_slice(&case.stdout).unwrap();
    let id = value["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|test| test["name"] == "zero")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let list = reporch()
        .args([
            "--cwd",
            temporary.path().join("tests").to_str().unwrap(),
            "--format",
            "json",
            "test",
            "case",
            "list",
        ])
        .output()
        .unwrap();
    assert!(list.status.success(), "{list:?}");
    let value: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert!(
        value["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|test| test["id"] == id)
    );

    let before = fs::read(temporary.path().join("reporch.yaml")).unwrap();
    let rejected = reporch()
        .args([
            "--cwd",
            temporary.path().to_str().unwrap(),
            "--format",
            "json",
            "test",
            "case",
            "add",
            "--name",
            "bad-group",
            "--input",
            "tests/2.in",
            "--group",
            "missing",
        ])
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(2), "{rejected:?}");
    assert_eq!(
        fs::read(temporary.path().join("reporch.yaml")).unwrap(),
        before
    );
}

#[test]
fn publication_is_fail_closed_without_interactive_confirmation() {
    let project_id = uuid::Uuid::now_v7().to_string();
    let release_id = uuid::Uuid::now_v7().to_string();
    let output = reporch()
        .args([
            "--format",
            "json",
            "--no-input",
            "publication",
            "publish",
            "--project-id",
            &project_id,
            "--release-id",
            &release_id,
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["error_code"], "input.invalid");
    assert!(value["message"].as_str().unwrap().contains("--yes"));
}

#[test]
fn validation_history_is_a_first_class_linked_project_command() {
    let output = reporch()
        .args(["validation", "list", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--project-id"), "{stdout}");
    assert!(!stdout.contains("--validation-run-id"), "{stdout}");
}

#[test]
fn revision_restore_requires_a_non_overwriting_checkout_directory() {
    let output = reporch()
        .args(["revision", "restore", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--directory"), "{stdout}");
    assert!(stdout.contains("<COMMIT_ID>"), "{stdout}");
    assert!(!stdout.contains("--force"), "{stdout}");
}

#[test]
fn completion_scripts_cover_every_supported_shell_and_reject_json_wrapping() {
    for shell in ["bash", "zsh", "fish", "power-shell", "elvish"] {
        let output = reporch().args(["completion", shell]).output().unwrap();
        assert!(output.status.success(), "{shell}: {output:?}");
        assert!(!output.stdout.is_empty(), "{shell}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("reporch"),
            "{shell}"
        );
    }

    let output = reporch()
        .args(["--format", "json", "completion", "zsh"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error_code"], "input.invalid");
}

#[test]
fn toolchain_catalog_is_signed_and_install_never_accepts_an_arbitrary_image() {
    let output = reporch()
        .args(["--format", "json", "toolchain", "list"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["command"], "toolchain list");
    assert_eq!(value["data"]["schema"], "reporch.toolchain-list.v1");
    assert!(value["data"]["entries"].as_array().unwrap().len() >= 10);
    assert_eq!(
        value["data"]["signing_key_sha256"].as_str().unwrap().len(),
        64
    );

    let help = reporch()
        .args(["toolchain", "install", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success(), "{help:?}");
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("<ID>"), "{help}");
    assert!(!help.contains("--image"), "{help}");
}
