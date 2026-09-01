// Regression: QA-H01 — RC8 help incorrectly said the default runtime required Docker/Podman.
// Found by /qa on 2026-09-01.
// Report: .gstack/qa-reports/qa-report-reporch-cli-2026-09-01.md

use std::process::Command;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

#[test]
fn authoring_help_names_the_reporch_vm_as_the_default_backend() {
    for command in ["checker", "validator", "generator", "interactor", "grader"] {
        let output = reporch().args([command, "run", "--help"]).output().unwrap();
        assert!(output.status.success(), "{command}: {output:?}");
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(
            help.contains("`auto` uses the mandatory Reporch VM"),
            "{command}:\n{help}"
        );
        assert!(
            help.contains("deprecated explicit compatibility modes"),
            "{command}:\n{help}"
        );
        assert!(
            !help.contains("Local author-code execution requires rootless Podman or Docker"),
            "{command}:\n{help}"
        );
    }
}

#[test]
fn sandbox_help_does_not_describe_auto_as_an_oci_only_path() {
    let output = reporch().arg("--help").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(
        help.contains("Plan or run a networkless Reporch VM command"),
        "{help}"
    );
}
