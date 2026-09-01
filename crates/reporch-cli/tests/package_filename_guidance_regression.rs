// Regression: QA-L04 — portable package errors hid the offending output filename.
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
fn portable_exports_name_the_invalid_stem_and_show_a_valid_example() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("problem");
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "A + B",
            "--directory",
            project.to_str().unwrap(),
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");

    for (profile, filename) in [
        ("icpc-2025-09", "invalid-icpc.zip"),
        ("icpc-legacy", "invalid-legacy.zip"),
        ("domjudge-zip", "invalid-domjudge.zip"),
    ] {
        let output = reporch()
            .args([
                "--cwd",
                project.to_str().unwrap(),
                "--profile",
                profile,
                "--format",
                "json",
                "package",
                "export",
                "reporch.yaml",
                directory.path().join(filename).to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{profile}: {output:?}");
        assert!(output.stdout.is_empty(), "{profile}: {output:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["schema"], "reporch.cli-error.v1");
        assert_eq!(error["error_code"], "input.invalid");
        let message = error["message"].as_str().unwrap();
        let stem = filename.trim_end_matches(".zip");
        assert!(message.contains(stem), "{profile}: {message}");
        assert!(message.contains("aplusb.zip"), "{profile}: {message}");
    }
}
