use std::process::Command;
use std::time::{Duration, Instant};

fn reporch() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reporch"))
}

#[test]
fn auth_login_no_input_fails_before_network_or_device_polling() {
    let isolated = tempfile::tempdir().unwrap();
    let runtime = isolated.path().join("runtime");
    let started = Instant::now();
    let output = reporch()
        .env("REPORCH_RUNTIME_HOME", &runtime)
        .env(
            "REPORCH_RUNTIME_CHANNEL_URL",
            "https://127.0.0.1:9/runtime/channel.json",
        )
        .args([
            "--format",
            "jsonl",
            "--no-input",
            "auth",
            "login",
            "--issuer",
            "http://127.0.0.1:9/oauth",
            "--allow-insecure-http",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "auth login --no-input unexpectedly waited: {:?}",
        started.elapsed()
    );
    assert!(output.stdout.is_empty(), "{output:?}");
    let error: serde_json::Value =
        serde_json::from_slice(output.stderr.trim_ascii()).expect("JSONL error envelope");
    assert_eq!(error["schema"], "reporch.cli-error.v1");
    assert_eq!(error["command"], "auth login");
    assert_eq!(error["error_code"], "input.invalid");
    assert_eq!(error["retryable"], false);
    assert!(
        !runtime.exists(),
        "auth unexpectedly bootstrapped the VM runtime"
    );
}
