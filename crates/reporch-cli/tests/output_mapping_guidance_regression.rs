// Regression: QA-L06 — output mapping failures did not tell users how to find test UUIDs.
// Found by /qa on 2026-09-01.
// Report: .gstack/qa-reports/qa-report-reporch-cli-rc9-fixed-luna10-2026-09-01.md

use std::process::Command;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

fn project() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Output guidance",
            "--directory",
            directory.path().to_str().unwrap(),
            "--problem-type",
            "output-only",
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");
    directory
}

#[test]
fn malformed_and_unknown_output_mapping_ids_show_the_discovery_command() {
    let project = project();
    for mapping in [
        "not-a-uuid=outputs/new.txt",
        "019f8fc9-cff3-7421-8cf8-0661a7a484dd=outputs/new.txt",
    ] {
        let output = reporch()
            .args([
                "--cwd",
                project.path().to_str().unwrap(),
                "--format",
                "json",
                "output",
                "add",
                "--name",
                "candidate",
                "--expected",
                "accepted",
                "--map",
                mapping,
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{mapping}: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("reporch test case list --format json"),
            "{mapping}: {stderr}"
        );
    }
}
