use std::process::Command;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

#[test]
fn fresh_standard_project_is_exportable_to_every_supported_package_profile() {
    let root = tempfile::tempdir().unwrap();
    let initialized = reporch()
        .current_dir(root.path())
        .args([
            "new",
            "--title",
            "Portable starter",
            "--directory",
            "problem",
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");
    let project = root.path().join("problem");

    for (profile, archive) in [
        ("reporch-native", "native.rpk"),
        ("icpc202509", "icpc.zip"),
        ("icpc-legacy", "legacy.zip"),
        ("polygon-compatible", "polygon.zip"),
        ("domjudge-zip", "domjudge.zip"),
    ] {
        let exported = reporch()
            .current_dir(&project)
            .args([
                "--profile",
                profile,
                "package",
                "export",
                "reporch.yaml",
                archive,
            ])
            .output()
            .unwrap();
        assert!(exported.status.success(), "{profile}: {exported:?}");
        assert!(project.join(archive).is_file(), "{profile}");
    }
}

#[test]
fn replacing_the_starter_validator_removes_only_its_example_unit_matrix() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("problem");
    assert!(
        reporch()
            .current_dir(root.path())
            .args([
                "new",
                "--title",
                "Replace validator",
                "--directory",
                "problem",
            ])
            .status()
            .unwrap()
            .success()
    );

    let replaced = reporch()
        .current_dir(&project)
        .args([
            "validator",
            "set",
            "--source",
            "solutions/accepted.py",
            "--language",
            "python3",
        ])
        .output()
        .unwrap();
    assert!(replaced.status.success(), "{replaced:?}");
    let spec = reporch_cli::local_project_v2::read_authoring_spec(&project).unwrap();
    assert!(spec.testing.validators.unit_tests.is_empty());
}
