// Regression: every statement command must enforce the same Markdown and asset policy.

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

fn init(directory: &std::path::Path) {
    let output = reporch()
        .args([
            "--quiet",
            "project",
            "init",
            "--title",
            "Statement validation",
            "--directory",
            directory.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

fn exercise_statement_validation(directory: &std::path::Path) {
    let statement = directory.join("statements/ko.md");
    fs::write(&statement, "# Unsafe\n\n<div>raw</div>\n").unwrap();
    for arguments in [
        &["check"][..],
        &["statement", "check"][..],
        &["statement", "render", "--locale", "ko"][..],
    ] {
        let output = run(directory, arguments);
        assert!(!output.status.success(), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("raw HTML"),
            "{arguments:?}: {output:?}"
        );
    }

    fs::write(
        &statement,
        "# Missing asset\n\n![diagram](assets/missing.png)\n",
    )
    .unwrap();
    for arguments in [
        &["check"][..],
        &["statement", "check"][..],
        &["statement", "render", "--locale", "ko"][..],
    ] {
        let output = run(directory, arguments);
        assert!(!output.status.success(), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("assets/missing.png"),
            "{arguments:?}: {output:?}"
        );
    }
}

#[test]
fn v1_check_and_render_reject_unsafe_markdown_and_missing_assets() {
    let directory = tempfile::tempdir().unwrap();
    init(directory.path());
    exercise_statement_validation(directory.path());
}

#[test]
fn v2_check_and_render_reject_unsafe_markdown_and_missing_assets() {
    let directory = tempfile::tempdir().unwrap();
    init(directory.path());
    reporch_cli::local_project_v2::migrate_v1_authoring_file(directory.path()).unwrap();
    exercise_statement_validation(directory.path());
}
