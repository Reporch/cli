// Regression: ISSUE-007 — guided test defaults pointed at files that did not exist.
// Found by /qa on 2026-09-04.
// Report: .gstack/qa-reports/qa-report-reporch-cli-deep-2026-09-04.md

#[cfg(target_os = "macos")]
mod macos {
    use std::process::Command;

    fn reporch() -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
        command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
        command
    }

    #[test]
    fn guided_test_creates_files_from_literal_input_and_output() {
        let project = tempfile::tempdir().unwrap();
        let initialized = reporch()
            .args([
                "new",
                "--title",
                "Guided tests",
                "--directory",
                project.path().to_str().unwrap(),
                "--yes",
                "--quiet",
            ])
            .output()
            .unwrap();
        assert!(initialized.status.success(), "{initialized:?}");

        let expect_script = format!(
            "set timeout 10\n\
             spawn -noecho {{{binary}}} --cwd {{{project}}} test\n\
             expect {{Test name}}\n\
             send -- \"\\r\"\n\
             expect {{Input data}}\n\
             send -- \"2 3\\r\"\n\
             expect {{Expected output}}\n\
             send -- \"5\\r\"\n\
             expect eof\n\
             exit [lindex [wait] 3]",
            binary = env!("CARGO_BIN_EXE_reporch"),
            project = project.path().display(),
        );
        let guided = Command::new("/usr/bin/expect")
            .args(["-c", &expect_script])
            .env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1")
            .output()
            .unwrap();
        assert!(guided.status.success(), "{guided:?}");
        let transcript = String::from_utf8_lossy(&guided.stdout);
        assert!(transcript.contains("Test name [sample-2]"), "{transcript}");
        assert!(
            transcript.contains("Input data (single line)"),
            "{transcript}"
        );

        let listed = reporch()
            .args([
                "--cwd",
                project.path().to_str().unwrap(),
                "--format",
                "json",
                "test",
                "case",
                "list",
            ])
            .output()
            .unwrap();
        assert!(listed.status.success(), "{listed:?}");
        let result: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
        let tests = result["data"].as_array().unwrap();
        let added = tests
            .iter()
            .find(|test| test["name"] == "sample-2")
            .expect("the guide should add sample-2");
        let input = project.path().join(added["input_file"].as_str().unwrap());
        let answer = project.path().join(added["answer_file"].as_str().unwrap());
        assert_eq!(std::fs::read(input).unwrap(), b"2 3\n", "{transcript}");
        assert_eq!(std::fs::read(answer).unwrap(), b"5\n", "{transcript}");
    }
}
