use std::process::Command;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

#[test]
fn checker_test_is_a_discoverable_alias_for_checker_run() {
    let output = reporch()
        .args(["checker", "test", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--timeout-seconds"), "{help}");
    assert!(help.contains("Reporch VM"), "{help}");
}

#[test]
fn output_remove_accepts_consistent_long_name_option_and_legacy_positional_form() {
    let long = reporch()
        .args(["output", "remove", "--name", "candidate"])
        .output()
        .unwrap();
    let positional = reporch()
        .args(["output", "remove", "candidate"])
        .output()
        .unwrap();

    for output in [long, positional] {
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.contains("Usage:"), "{stderr}");
        assert!(stderr.contains("no reporch.yaml found"), "{stderr}");
    }
}
