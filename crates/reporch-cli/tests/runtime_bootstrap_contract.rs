use std::fs;
use std::process::Command;

#[test]
fn minisign_verification_does_not_depend_on_an_existing_runtime_channel() {
    let temporary = tempfile::tempdir().unwrap();
    let artifact = temporary.path().join("runtime-manifest.json");
    let runtime_home = temporary.path().join("runtime-home");
    fs::write(&artifact, b"signed runtime candidate").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_reporch"))
        .args([
            "--cwd",
            temporary.path().to_str().unwrap(),
            "artifact",
            "verify-minisign",
            "--artifact",
            artifact.to_str().unwrap(),
            "--signature",
            "aW52YWxpZA==",
            "--public-key",
            "aW52YWxpZA==",
        ])
        .env("REPORCH_RUNTIME_HOME", &runtime_home)
        .env(
            "REPORCH_RUNTIME_CHANNEL_URL",
            "https://runtime-channel.invalid",
        )
        .env_remove("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("decode Minisign public key"), "{stderr}");
    assert!(!stderr.contains("Runtime bootstrap"), "{stderr}");
    assert!(!runtime_home.exists());
}
