use std::process::Command;

#[test]
fn toolchain_prefetch_reports_progress_on_stderr_and_keeps_stdout_machine_readable() {
    let output = Command::new(env!("CARGO_BIN_EXE_reporch"))
        .env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1")
        .args([
            "--format",
            "jsonl",
            "toolchain",
            "prefetch",
            "missing-regression-toolchain",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let lines = String::from_utf8(output.stderr).unwrap();
    let mut values = lines.lines().map(|line| {
        serde_json::from_str::<serde_json::Value>(line).expect("one JSON envelope per stderr line")
    });
    let progress = values.next().expect("progress envelope");
    assert_eq!(progress["schema"], "reporch.cli-progress.v1");
    assert_eq!(progress["command"], "toolchain prefetch");
    assert!(
        progress["message"]
            .as_str()
            .unwrap()
            .contains("missing-regression-toolchain")
    );
    let error = values.next().expect("error envelope");
    assert_eq!(error["schema"], "reporch.cli-error.v1");
    assert!(values.next().is_none(), "{lines}");
}

#[test]
fn quiet_suppresses_progress_but_not_machine_readable_errors() {
    let output = Command::new(env!("CARGO_BIN_EXE_reporch"))
        .env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1")
        .args([
            "--format",
            "jsonl",
            "--quiet",
            "toolchain",
            "prefetch",
            "missing-regression-toolchain",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let lines = String::from_utf8(output.stderr).unwrap();
    let values = lines
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(values.len(), 1, "{lines}");
    assert_eq!(values[0]["schema"], "reporch.cli-error.v1");
}
