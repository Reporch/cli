// Regression: QA-L09 — invalid output paths and timeouts paid VM setup cost before failing.
// Found by /qa on 2026-09-01.
// Report: .gstack/qa-reports/qa-report-reporch-cli-rc9-ux2-luna10-2026-09-01.md

use std::process::Command;
use std::time::{Duration, Instant};

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

#[test]
fn invalid_runtime_output_and_timeout_fail_before_any_runtime_progress() {
    let directory = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Interactive options",
            "--directory",
            directory.path().to_str().unwrap(),
            "--problem-type",
            "interactive",
            "--yes",
            "--quiet",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");

    let cases = [
        (
            vec![
                "interactor",
                "run",
                "--solution",
                "accepted",
                "--test",
                "sample-1",
                "--output",
                "/tmp/transcript.txt",
            ],
            "safe project-relative path",
        ),
        (
            vec![
                "interactor",
                "run",
                "--solution",
                "accepted",
                "--test",
                "sample-1",
                "--timeout-seconds",
                "0",
            ],
            "1..=600",
        ),
    ];

    for (arguments, expected) in cases {
        let started = Instant::now();
        let output = reporch()
            .args([
                "--cwd",
                directory.path().to_str().unwrap(),
                "--format",
                "json",
            ])
            .args(arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(expected), "{stderr}");
        assert!(
            !stderr.contains("Initializing local verification"),
            "{stderr}"
        );
        assert!(
            !stderr.contains("Preparing the isolated Reporch VM"),
            "{stderr}"
        );
    }
}
