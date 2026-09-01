use std::fs;
use std::process::Command;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

#[test]
fn statement_add_safely_creates_a_missing_markdown_file() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("problem");
    let initialized = reporch()
        .current_dir(root.path())
        .args(["new", "--title", "Starter", "--directory", "problem"])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");

    let added = reporch()
        .current_dir(&project)
        .args([
            "statement",
            "add",
            "--locale",
            "ko-KR",
            "--path",
            "statements/ko-KR.md",
            "--title",
            "두 수의 합",
            "--create",
        ])
        .output()
        .unwrap();
    assert!(added.status.success(), "{added:?}");

    let statement = fs::read_to_string(project.join("statements/ko-KR.md")).unwrap();
    assert!(statement.starts_with("# 두 수의 합\n"), "{statement}");
    let checked = reporch()
        .current_dir(&project)
        .arg("check")
        .output()
        .unwrap();
    assert!(checked.status.success(), "{checked:?}");
}

#[cfg(unix)]
#[test]
fn statement_add_refuses_to_follow_a_symlink() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("problem");
    assert!(
        reporch()
            .current_dir(root.path())
            .args(["new", "--title", "Starter", "--directory", "problem",])
            .status()
            .unwrap()
            .success()
    );
    let outside = root.path().join("outside.md");
    fs::write(&outside, "secret").unwrap();
    symlink(&outside, project.join("statements/linked.md")).unwrap();

    let added = reporch()
        .current_dir(&project)
        .args([
            "statement",
            "add",
            "--locale",
            "ko-KR",
            "--path",
            "statements/linked.md",
            "--create",
        ])
        .output()
        .unwrap();
    assert!(!added.status.success(), "{added:?}");
    assert_eq!(fs::read_to_string(outside).unwrap(), "secret");
}
