use std::process::Command;

fn help(arguments: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_reporch"))
        .env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1")
        .args(arguments)
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn advanced_authoring_help_contains_copyable_end_to_end_examples() {
    let cases = [
        (vec!["test"], "reporch test group add full-score"),
        (vec!["generator"], "reporch generator recipe random"),
        (vec!["validator"], "reporch validator unit-add"),
        (vec!["checker"], "reporch checker unit-add"),
    ];
    for (arguments, example) in cases {
        let rendered = help(&arguments);
        assert!(rendered.contains("Examples:"), "{rendered}");
        assert!(rendered.contains(example), "missing {example}: {rendered}");
    }
}

#[test]
fn doctor_help_separates_remote_account_and_local_runtime_diagnostics() {
    let rendered = help(&["doctor"]);
    assert!(rendered.contains("authenticated Studio API"), "{rendered}");
    assert!(rendered.contains("runtime doctor"), "{rendered}");
}
