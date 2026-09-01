use std::process::Command;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

#[test]
fn top_level_status_and_diff_match_the_nested_project_commands() {
    let root = tempfile::tempdir().unwrap();
    assert!(
        reporch()
            .current_dir(root.path())
            .args(["new", "--title", "Discoverable", "--directory", "problem"])
            .status()
            .unwrap()
            .success()
    );
    let project = root.path().join("problem");
    for (short, nested) in [("status", "status"), ("diff", "diff")] {
        let short_output = reporch()
            .current_dir(&project)
            .args(["--format", "json", short])
            .output()
            .unwrap();
        let nested_output = reporch()
            .current_dir(&project)
            .args(["--format", "json", "project", nested])
            .output()
            .unwrap();
        assert!(short_output.status.success(), "{short_output:?}");
        assert!(nested_output.status.success(), "{nested_output:?}");
        let short_json: serde_json::Value = serde_json::from_slice(&short_output.stdout).unwrap();
        let nested_json: serde_json::Value = serde_json::from_slice(&nested_output.stdout).unwrap();
        assert_eq!(short_json["data"], nested_json["data"]);
    }
}

#[test]
fn migration_help_explains_legacy_detection_backup_and_noop() {
    let output = reporch().args(["migrate", "--help"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let help = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "pre-1.0",
        "reporch.problem.pre-1.0.json",
        "migrated:false",
        "reporch migrate --directory",
    ] {
        assert!(help.contains(expected), "missing {expected}: {help}");
    }
}
