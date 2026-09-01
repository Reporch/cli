// Regression: QA-L07 — scored group mutations hid the running point total until `check`.
// Found by /qa on 2026-09-01.
// Report: .gstack/qa-reports/qa-report-reporch-cli-rc9-fixed-luna10-2026-09-01.md

use std::process::Command;

use serde_json::Value;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

#[test]
fn scored_group_mutations_show_total_overage_and_keep_json_shape_stable() {
    let directory = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Scored feedback",
            "--directory",
            directory.path().to_str().unwrap(),
            "--problem-type",
            "scored",
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");

    let added = reporch()
        .args([
            "--cwd",
            directory.path().to_str().unwrap(),
            "test",
            "group",
            "add",
            "bonus",
            "--points",
            "40",
        ])
        .output()
        .unwrap();
    assert!(added.status.success(), "{added:?}");
    let human = String::from_utf8(added.stdout).unwrap();
    assert!(human.contains("scored groups total 140/100"), "{human}");
    assert!(human.contains("40 points over"), "{human}");
    assert!(
        human.contains("reporch test group update bonus --points <POINTS>"),
        "{human}"
    );

    let updated = reporch()
        .args([
            "--cwd",
            directory.path().to_str().unwrap(),
            "--format",
            "json",
            "test",
            "group",
            "update",
            "bonus",
            "--points",
            "0",
        ])
        .output()
        .unwrap();
    assert!(updated.status.success(), "{updated:?}");
    let envelope: Value = serde_json::from_slice(&updated.stdout).unwrap();
    assert_eq!(envelope["command"], "test group update");
    assert!(envelope["data"].is_array(), "{envelope}");
}
