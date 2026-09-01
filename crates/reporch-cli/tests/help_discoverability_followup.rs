// Regression: QA-L03 — valid aliases and remote workflows were hard to discover.
// Found by /qa on 2026-09-01.
// Report: .gstack/qa-reports/qa-report-reporch-cli-rc9-luna10-2026-09-01.md

use std::process::Command;

fn reporch() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reporch"));
    command.env("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP", "1");
    command
}

fn help(arguments: &[&str]) -> String {
    let output = reporch().args(arguments).arg("--help").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn short_package_profile_aliases_are_accepted_by_real_commands() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("problem");
    let initialized = reporch()
        .args([
            "new",
            "--title",
            "Profile aliases",
            "--directory",
            project.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");

    for profile in ["reporch", "icpc-2025-09", "polygon", "domjudge"] {
        let output = reporch()
            .current_dir(&project)
            .args(["--profile", profile, "manifest", "compatibility"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{profile}: {output:?}");
    }
}

#[test]
fn checker_help_lists_the_test_alias_next_to_run() {
    let rendered = help(&["checker"]);
    assert!(rendered.contains("run"), "{rendered}");
    assert!(rendered.contains("test"), "{rendered}");
    assert!(rendered.contains("alias"), "{rendered}");
}

#[test]
fn package_help_lists_canonical_profiles_and_short_aliases() {
    let rendered = help(&["package"]);
    for expected in [
        "reporch-native",
        "reporch",
        "icpc202509",
        "icpc-2025-09",
        "icpc-legacy",
        "polygon-compatible",
        "polygon",
        "domjudge-zip",
        "domjudge",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected}:\n{rendered}"
        );
    }
}

#[test]
fn member_search_help_names_the_positional_query_and_gives_an_example() {
    let rendered = help(&["member", "search"]);
    assert!(rendered.contains("<QUERY>"), "{rendered}");
    assert!(
        rendered.contains("Name, handle, or email fragment"),
        "{rendered}"
    );
    assert!(
        rendered.contains("reporch member search jinwoo"),
        "{rendered}"
    );
}

#[test]
fn remote_workflow_subcommands_have_user_facing_descriptions() {
    for (arguments, expected) in [
        (&["quota"][..], "CPU, concurrency, and storage quota"),
        (&["publication"][..], "Publish a verified immutable release"),
        (&["validation"][..], "deterministic evidence summary"),
        (&["waiver"][..], "evidence-bound waivers"),
        (&["revision"][..], "Compare two immutable project revisions"),
    ] {
        let rendered = help(arguments);
        assert!(
            rendered.contains(expected),
            "missing {expected}:\n{rendered}"
        );
    }
}

#[test]
fn toolchain_prefetch_help_sets_first_install_latency_expectations() {
    let rendered = help(&["toolchain"]);
    assert!(rendered.contains("about a minute"), "{rendered}");
    assert!(rendered.contains("cached runs are faster"), "{rendered}");
}
