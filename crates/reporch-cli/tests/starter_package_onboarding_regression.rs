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
