use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use studio_core::{
    ExpectedVerdict, IssueSeverity, PackageProfile, ProblemType, ReleaseManifestV1,
    compatibility_report, validate_manifest,
};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::icpc_submit_answer::{
    SIDECAR_PATH, SIDECAR_SCHEMA_V1, SubmitAnswerOutputV1, SubmitAnswerSidecarV1,
    SubmitAnswerSubmissionV1, SubmitAnswerTestV1,
};

enum EntrySource {
    Bytes(Vec<u8>),
    ManifestFile {
        source: PathBuf,
        expected_digest: String,
        expected_size: u64,
    },
}

struct ExportEntry {
    path: String,
    executable: bool,
    source: EntrySource,
}

pub fn export_icpc_2025_09(
    manifest: &ReleaseManifestV1,
    source_root: &Path,
    output: &Path,
) -> Result<()> {
    export_icpc_based(manifest, source_root, output, PackageProfile::Icpc202509)
}

pub fn export_domjudge_zip(
    manifest: &ReleaseManifestV1,
    source_root: &Path,
    output: &Path,
) -> Result<()> {
    export_icpc_based(manifest, source_root, output, PackageProfile::DomjudgeZip)
}

fn export_icpc_based(
    manifest: &ReleaseManifestV1,
    source_root: &Path,
    output: &Path,
    target_profile: PackageProfile,
) -> Result<()> {
    let blocking_issues = validate_manifest(manifest)
        .into_iter()
        .filter(|issue| issue.severity == IssueSeverity::Error)
        .collect::<Vec<_>>();
    if !blocking_issues.is_empty() {
        bail!(
            "native manifest validation failed: {}",
            serde_json::to_string(&blocking_issues)?
        );
    }
    let report = compatibility_report(manifest, target_profile);
    if !report.exportable {
        bail!(
            "package export is blocked: {}",
            serde_json::to_string(&report)?
        );
    }
    ensure!(
        matches!(
            manifest.problem_type,
            ProblemType::Standard
                | ProblemType::Scored
                | ProblemType::Interactive
                | ProblemType::OutputOnly
        ),
        "the ICPC-based writer supports standard, scored, interactive, and submit-answer problems"
    );
    let output_stem = output
        .file_stem()
        .and_then(|value| value.to_str())
        .context("output filename must provide an ASCII package name")?;
    let short_name = sanitize_name(output_stem);
    ensure!(
        output_stem == short_name
            && !short_name.is_empty()
            && short_name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
        "ICPC package name must contain only lowercase ASCII letters and digits"
    );
    ensure!(
        output.extension().and_then(|value| value.to_str()) == Some("zip"),
        "ICPC package output must use the .zip extension"
    );

    let mut entries = vec![ExportEntry {
        path: format!("{short_name}/problem.yaml"),
        executable: false,
        source: EntrySource::Bytes(problem_yaml(manifest).into_bytes()),
    }];
    entries.push(ExportEntry {
        path: format!("{short_name}/-reporch-compatibility.json"),
        executable: false,
        source: EntrySource::Bytes(serde_json::to_vec_pretty(&report)?),
    });
    if target_profile == PackageProfile::DomjudgeZip {
        entries.push(ExportEntry {
            path: format!("{short_name}/domjudge-problem.ini"),
            executable: false,
            source: EntrySource::Bytes(domjudge_problem_ini(manifest, &short_name).into_bytes()),
        });
    }
    let mut occupied = entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();

    if manifest.problem_type == ProblemType::Scored {
        let total_score = manifest
            .judging
            .groups
            .iter()
            .map(|group| group.points as u64)
            .sum::<u64>();
        push_bytes_entry(
            format!("{short_name}/data/secret/test_group.yaml"),
            format!("max_score: {total_score}\nscore_aggregation: sum\n").into_bytes(),
            &mut occupied,
            &mut entries,
        )?;
        for group in &manifest.judging.groups {
            let group_name = sanitize_component(&group.id);
            ensure!(!group_name.is_empty(), "ICPC test group name is empty");
            let mut config = format!(
                "max_score: {}\nscore_aggregation: pass-fail\n",
                group.points as u64
            );
            if !group.depends_on.is_empty() {
                let dependencies = group
                    .depends_on
                    .iter()
                    .map(|dependency| format!("secret/{}", sanitize_component(dependency)))
                    .collect::<Vec<_>>();
                config.push_str(&format!(
                    "require_pass: {}\n",
                    serde_json::to_string(&dependencies)?
                ));
            }
            push_bytes_entry(
                format!("{short_name}/data/secret/{group_name}/test_group.yaml"),
                config.into_bytes(),
                &mut occupied,
                &mut entries,
            )?;
        }
    }

    for (locale, path) in &manifest.statements {
        let target = format!(
            "{short_name}/statement/problem.{}.md",
            sanitize_locale(locale)?
        );
        push_manifest_entry(
            manifest,
            source_root,
            path,
            target,
            false,
            &mut occupied,
            &mut entries,
        )?;
    }

    let sample_paths = manifest
        .publication
        .as_ref()
        .map(|publication| {
            publication
                .samples
                .iter()
                .flat_map(|sample| [&sample.input_file, &sample.output_file])
                .cloned()
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let mut submit_answer_tests = Vec::new();
    for (index, test) in manifest.judging.tests.iter().enumerate() {
        let name = format!("{:04}", index + 1);
        let secret_directories =
            if manifest.problem_type == ProblemType::Scored && !test.groups.is_empty() {
                test.groups
                    .iter()
                    .map(|group| format!("{}/", sanitize_component(group)))
                    .collect::<Vec<_>>()
            } else {
                vec![String::new()]
            };
        for directory in secret_directories {
            push_manifest_entry(
                manifest,
                source_root,
                &test.input_file,
                format!("{short_name}/data/secret/{directory}{name}.in"),
                false,
                &mut occupied,
                &mut entries,
            )?;
            if let Some(answer) = test.answer_file.as_deref() {
                push_manifest_entry(
                    manifest,
                    source_root,
                    answer,
                    format!("{short_name}/data/secret/{directory}{name}.ans"),
                    false,
                    &mut occupied,
                    &mut entries,
                )?;
            }
        }
        if manifest.problem_type == ProblemType::OutputOnly {
            submit_answer_tests.push(SubmitAnswerTestV1 {
                test_id: test.id,
                test_name: test.name.clone(),
                test_index: index + 1,
                input_path: format!("data/secret/{name}.in"),
            });
        }
        if sample_paths.contains(&test.input_file) {
            push_manifest_entry(
                manifest,
                source_root,
                &test.input_file,
                format!("{short_name}/data/sample/{name}.in"),
                false,
                &mut occupied,
                &mut entries,
            )?;
            if let Some(answer) = test.answer_file.as_deref() {
                push_manifest_entry(
                    manifest,
                    source_root,
                    answer,
                    format!("{short_name}/data/sample/{name}.ans"),
                    false,
                    &mut occupied,
                    &mut entries,
                )?;
            }
        }
    }

    for (index, test) in manifest
        .judging
        .validator_tests
        .iter()
        .filter(|test| !test.expected_valid)
        .enumerate()
    {
        push_manifest_entry(
            manifest,
            source_root,
            &test.input_file,
            format!("{short_name}/data/invalid_input/{:04}.in", index + 1),
            false,
            &mut occupied,
            &mut entries,
        )?;
    }

    for (index, test) in manifest.judging.checker_tests.iter().enumerate() {
        let directory = if test.expected_accepted {
            "valid_output"
        } else {
            "invalid_output"
        };
        let name = format!("{:04}", index + 1);
        for (source, extension) in [
            (&test.input_file, "in"),
            (&test.answer_file, "ans"),
            (&test.output_file, "out"),
        ] {
            push_manifest_entry(
                manifest,
                source_root,
                source,
                format!("{short_name}/data/{directory}/{name}.{extension}"),
                false,
                &mut occupied,
                &mut entries,
            )?;
        }
    }

    let mut submissions_yaml = String::new();
    for (index, solution) in manifest.solutions.iter().enumerate() {
        let directory = match solution.expected_verdict {
            ExpectedVerdict::Accepted => "accepted",
            ExpectedVerdict::WrongAnswer => "wrong_answer",
            ExpectedVerdict::TimeLimit => "time_limit_exceeded",
            ExpectedVerdict::RuntimeError => "run_time_error",
            // ICPC 2025-09 has no default directory with MLE or partial-score
            // semantics. The generic rejected directory is intentionally used;
            // scoring constraints below preserve the exact partial expectation.
            ExpectedVerdict::MemoryLimit | ExpectedVerdict::Partial => "rejected",
        };
        let extension = Path::new(&solution.source_path)
            .extension()
            .and_then(|value| value.to_str())
            .map(sanitize_component)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "txt".into());
        let relative_submission_path = format!(
            "{directory}/{:02}_{}.{}",
            index + 1,
            sanitize_component(&solution.name),
            extension
        );
        push_manifest_entry(
            manifest,
            source_root,
            &solution.source_path,
            format!("{short_name}/submissions/{relative_submission_path}"),
            false,
            &mut occupied,
            &mut entries,
        )?;
        if let Some(score) = solution.expected_score.as_ref() {
            submissions_yaml.push_str(&format!("{}:\n", json_string(&relative_submission_path)));
            if score.minimum == score.maximum {
                submissions_yaml.push_str(&format!("  score: {}\n", score.minimum));
            } else {
                submissions_yaml.push_str(&format!(
                    "  score: [{}, {}]\n",
                    score.minimum, score.maximum
                ));
            }
        }
    }
    let mut submit_answer_submissions = Vec::new();
    if manifest.problem_type == ProblemType::OutputOnly {
        for (submission_index, submission) in manifest.output_submissions.iter().enumerate() {
            let directory = submission_directory(submission.expected_verdict);
            let component = sanitize_component(&submission.name);
            ensure!(
                !component.is_empty(),
                "submit-answer submission name cannot be represented in the ICPC profile"
            );
            let package_path = format!("{directory}/{:02}_{component}", submission_index + 1);
            let mut outputs = Vec::with_capacity(manifest.judging.tests.len());
            for (test_index, test) in manifest.judging.tests.iter().enumerate() {
                let source_path = submission.outputs.get(&test.id).with_context(|| {
                    format!(
                        "submit-answer submission {} has no output for test {}",
                        submission.name, test.name
                    )
                })?;
                let file = manifest
                    .files
                    .iter()
                    .find(|file| file.path == *source_path)
                    .with_context(|| format!("manifest file is missing: {source_path}"))?;
                let relative_path = format!("{package_path}/{:04}.out", test_index + 1);
                push_manifest_entry(
                    manifest,
                    source_root,
                    source_path,
                    format!("{short_name}/submissions/{relative_path}"),
                    false,
                    &mut occupied,
                    &mut entries,
                )?;
                outputs.push(SubmitAnswerOutputV1 {
                    test_id: test.id,
                    test_index: test_index + 1,
                    path: format!("submissions/{relative_path}"),
                    source_path: source_path.clone(),
                    sha256: file.sha256.as_str().into(),
                });
            }
            if let Some(score) = submission.expected_score.as_ref() {
                append_submission_score(&mut submissions_yaml, &package_path, score);
            }
            submit_answer_submissions.push(SubmitAnswerSubmissionV1 {
                name: submission.name.clone(),
                package_path,
                expected_verdict: submission.expected_verdict,
                expected_score: submission.expected_score.clone(),
                outputs,
            });
        }
        let sidecar = SubmitAnswerSidecarV1 {
            schema: SIDECAR_SCHEMA_V1.into(),
            tests: submit_answer_tests,
            submissions: submit_answer_submissions,
        };
        push_bytes_entry(
            format!("{short_name}/{SIDECAR_PATH}"),
            serde_json::to_vec_pretty(&sidecar)?,
            &mut occupied,
            &mut entries,
        )?;
    }
    if !submissions_yaml.is_empty() {
        push_bytes_entry(
            format!("{short_name}/submissions/submissions.yaml"),
            submissions_yaml.into_bytes(),
            &mut occupied,
            &mut entries,
        )?;
    }

    if let Some(validator) = manifest.judging.validator_path.as_deref() {
        let filename = source_filename(validator)?;
        push_manifest_entry(
            manifest,
            source_root,
            validator,
            format!("{short_name}/input_validators/{filename}"),
            true,
            &mut occupied,
            &mut entries,
        )?;
    }
    for (index, validator) in manifest.judging.extra_validator_paths.iter().enumerate() {
        let filename = source_filename(validator)?;
        push_manifest_entry(
            manifest,
            source_root,
            validator,
            format!(
                "{short_name}/input_validators/path_{:02}_{filename}",
                index + 1
            ),
            true,
            &mut occupied,
            &mut entries,
        )?;
    }
    for (index, validator) in manifest.judging.extra_validators.iter().enumerate() {
        let filename = source_filename(&validator.source_path)?;
        push_manifest_entry(
            manifest,
            source_root,
            &validator.source_path,
            format!("{short_name}/input_validators/{:02}_{filename}", index + 1),
            true,
            &mut occupied,
            &mut entries,
        )?;
    }
    for (index, generator) in manifest.judging.generators.iter().enumerate() {
        let filename = source_filename(&generator.source_path)?;
        push_manifest_entry(
            manifest,
            source_root,
            &generator.source_path,
            format!("{short_name}/generators/{:02}_{filename}", index + 1),
            true,
            &mut occupied,
            &mut entries,
        )?;
    }
    let output_validator = if manifest.problem_type == ProblemType::Interactive {
        manifest.judging.interactor_path.as_deref()
    } else if let studio_core::CheckerSpec::Custom { source_path, .. } = &manifest.judging.checker {
        Some(source_path.as_str())
    } else {
        None
    };
    if let Some(source_path) = output_validator {
        push_manifest_entry(
            manifest,
            source_root,
            source_path,
            format!(
                "{short_name}/output_validator/{}",
                source_filename(source_path)?
            ),
            true,
            &mut occupied,
            &mut entries,
        )?;
    }

    entries.sort_by(|left, right| left.path.cmp(&right.path));
    for entry in &entries {
        verify_entry(entry)?;
    }
    let output_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .with_context(|| format!("create new export archive {}", output.display()))?;
    let mut archive = ZipWriter::new(output_file);
    for entry in entries {
        archive.start_file(&entry.path, options(entry.executable))?;
        match entry.source {
            EntrySource::Bytes(bytes) => archive.write_all(&bytes)?,
            EntrySource::ManifestFile { source, .. } => {
                let mut source = File::open(&source)?;
                std::io::copy(&mut source, &mut archive)?;
            }
        }
    }
    archive.finish()?.sync_all()?;
    Ok(())
}

fn problem_yaml(manifest: &ReleaseManifestV1) -> String {
    let mut yaml = String::from("problem_format_version: 2025-09\nname:\n");
    for (locale, title) in &manifest.title {
        yaml.push_str(&format!(
            "  {}: {}\n",
            sanitize_component(locale),
            json_string(title)
        ));
    }
    yaml.push_str(&format!("uuid: {}\n", manifest.project_id));
    match manifest.problem_type {
        ProblemType::Scored => yaml.push_str("type: scoring\n"),
        ProblemType::Interactive => yaml.push_str("type: interactive\n"),
        ProblemType::OutputOnly => yaml.push_str("type: [pass-fail, submit-answer]\n"),
        _ => yaml.push_str("type: pass-fail\n"),
    }
    if let Some(source) = manifest.sources.first() {
        yaml.push_str(&format!(
            "source: {}\nlicense: {}\n",
            json_string(&format!("{} {}", source.provider, source.external_id)),
            json_string(&source.license_name)
        ));
        if !source.attribution.is_empty() {
            yaml.push_str(&format!(
                "rights_owner: {}\n",
                json_string(&source.attribution)
            ));
        }
    } else {
        yaml.push_str("license: unknown\n");
    }
    yaml.push_str("limits:\n");
    yaml.push_str(&format!(
        "  time_limit: {:.3}\n  memory: {}\n  output: {}\n",
        manifest.judging.limits.time_ms as f64 / 1_000.0,
        manifest.judging.limits.memory_mib,
        manifest.judging.limits.output_kib.div_ceil(1_024).max(1)
    ));
    if let Some(publication) = manifest.publication.as_ref() {
        if !publication.tags.is_empty() {
            yaml.push_str(&format!(
                "keywords: {}\n",
                serde_json::to_string(&publication.tags).expect("strings serialize")
            ));
        }
        if !publication.allowed_languages.is_empty() {
            yaml.push_str(&format!(
                "languages: {}\n",
                serde_json::to_string(&publication.allowed_languages).expect("strings serialize")
            ));
        }
    }
    yaml
}

fn domjudge_problem_ini(manifest: &ReleaseManifestV1, short_name: &str) -> String {
    let title = manifest
        .title
        .get(&manifest.default_locale)
        .expect("validated default title");
    format!(
        "name = {}\nallow_submit = 1\nallow_judge = 1\ntimelimit = {:.3}\nexternalid = {}\nshort-name = {}\n",
        json_string(title),
        manifest.judging.limits.time_ms as f64 / 1_000.0,
        json_string(short_name),
        json_string(short_name),
    )
}

fn submission_directory(verdict: ExpectedVerdict) -> &'static str {
    match verdict {
        ExpectedVerdict::Accepted => "accepted",
        ExpectedVerdict::WrongAnswer => "wrong_answer",
        ExpectedVerdict::TimeLimit => "time_limit_exceeded",
        ExpectedVerdict::RuntimeError => "run_time_error",
        ExpectedVerdict::MemoryLimit | ExpectedVerdict::Partial => "rejected",
    }
}

fn append_submission_score(
    yaml: &mut String,
    relative_path: &str,
    score: &studio_core::ExpectedScoreRange,
) {
    yaml.push_str(&format!("{}:\n", json_string(relative_path)));
    if score.minimum == score.maximum {
        yaml.push_str(&format!("  score: {}\n", score.minimum));
    } else {
        yaml.push_str(&format!(
            "  score: [{}, {}]\n",
            score.minimum, score.maximum
        ));
    }
}

fn push_bytes_entry(
    target_path: String,
    bytes: Vec<u8>,
    occupied: &mut BTreeSet<String>,
    entries: &mut Vec<ExportEntry>,
) -> Result<()> {
    ensure!(
        occupied.insert(target_path.clone()),
        "duplicate export path"
    );
    entries.push(ExportEntry {
        path: target_path,
        executable: false,
        source: EntrySource::Bytes(bytes),
    });
    Ok(())
}

fn push_manifest_entry(
    manifest: &ReleaseManifestV1,
    source_root: &Path,
    source_path: &str,
    target_path: String,
    force_executable: bool,
    occupied: &mut BTreeSet<String>,
    entries: &mut Vec<ExportEntry>,
) -> Result<()> {
    ensure!(
        occupied.insert(target_path.clone()),
        "duplicate export path"
    );
    let file = manifest
        .files
        .iter()
        .find(|file| file.path == source_path)
        .with_context(|| format!("manifest file is missing: {source_path}"))?;
    entries.push(ExportEntry {
        path: target_path,
        executable: force_executable || file.executable,
        source: EntrySource::ManifestFile {
            source: source_root.join(source_path),
            expected_digest: file.sha256.as_str().into(),
            expected_size: file.size_bytes,
        },
    });
    Ok(())
}

fn verify_entry(entry: &ExportEntry) -> Result<()> {
    let EntrySource::ManifestFile {
        source,
        expected_digest,
        expected_size,
    } = &entry.source
    else {
        return Ok(());
    };
    let mut file = File::open(source).with_context(|| format!("read {}", source.display()))?;
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("source file size overflow")?;
        digest.update(&buffer[..read]);
    }
    ensure!(
        size == *expected_size,
        "source file size changed: {}",
        source.display()
    );
    ensure!(
        hex::encode(digest.finalize()) == *expected_digest,
        "source file digest changed: {}",
        source.display()
    );
    Ok(())
}

fn options(executable: bool) -> SimpleFileOptions {
    SimpleFileOptions::DEFAULT
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(if executable { 0o755 } else { 0o644 })
}

fn sanitize_locale(locale: &str) -> Result<String> {
    let locale = locale
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    ensure!(!locale.is_empty(), "statement locale is empty");
    Ok(locale)
}

fn sanitize_name(value: &str) -> String {
    value
        .to_ascii_lowercase()
        .chars()
        .filter(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        .collect()
}

fn sanitize_component(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    value.trim_matches(['.', '-']).to_owned()
}

fn source_filename(path: &str) -> Result<String> {
    Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .map(sanitize_component)
        .filter(|value| !value.is_empty())
        .context("source filename cannot be represented in the ICPC profile")
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("string serialization cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn add_validator(manifest: &mut ReleaseManifestV1, root: &Path, path: &str, validator: &[u8]) {
        fs::create_dir_all(root.join("validators")).unwrap();
        fs::write(root.join(path), validator).unwrap();
        let invalid_path = "validators/invalid.in";
        let invalid = b"invalid\n";
        fs::write(root.join(invalid_path), invalid).unwrap();
        manifest.files.push(studio_core::ManifestFile {
            path: path.into(),
            sha256: studio_core::Sha256Digest::from_bytes(validator),
            size_bytes: validator.len() as u64,
            media_type: "text/x-python".into(),
            executable: true,
        });
        manifest.files.push(studio_core::ManifestFile {
            path: invalid_path.into(),
            sha256: studio_core::Sha256Digest::from_bytes(invalid),
            size_bytes: invalid.len() as u64,
            media_type: "text/plain".into(),
            executable: false,
        });
        manifest.judging.validator_path = Some(path.into());
        manifest.judging.validator_language = Some("python3".into());
        manifest.judging.validator_tests = vec![
            studio_core::ValidatorTestSpec {
                name: "valid".into(),
                input_file: "tests/1.in".into(),
                expected_valid: true,
            },
            studio_core::ValidatorTestSpec {
                name: "invalid".into(),
                input_file: invalid_path.into(),
                expected_valid: false,
            },
        ];
    }

    #[test]
    fn exports_a_deterministic_digest_checked_icpc_tree() {
        let temporary = tempfile::tempdir().unwrap();
        super::super::init_project(temporary.path(), "ICPC Fixture").unwrap();
        let manifest_path = temporary.path().join("reporch.problem.json");
        let mut manifest = super::super::read_manifest(&manifest_path).unwrap();
        let validator = b"import sys\nraise SystemExit(0 if sys.stdin.read() else 1)\n";
        add_validator(
            &mut manifest,
            temporary.path(),
            "validators/main.py",
            validator,
        );
        let output = temporary.path().join("fixture.zip");
        let replay_directory = temporary.path().join("replay");
        fs::create_dir(&replay_directory).unwrap();
        let replay_output = replay_directory.join("fixture.zip");

        export_icpc_2025_09(&manifest, temporary.path(), &output).unwrap();
        export_icpc_2025_09(&manifest, temporary.path(), &replay_output).unwrap();

        assert_eq!(fs::read(&output).unwrap(), fs::read(replay_output).unwrap());

        let mut archive = zip::ZipArchive::new(File::open(output).unwrap()).unwrap();
        let paths = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_owned())
            .collect::<BTreeSet<_>>();
        assert!(paths.contains("fixture/problem.yaml"));
        assert!(paths.contains("fixture/-reporch-compatibility.json"));
        assert!(paths.contains("fixture/statement/problem.ko.md"));
        assert!(paths.contains("fixture/data/sample/0001.in"));
        assert!(paths.contains("fixture/data/secret/0001.in"));
        assert!(paths.contains("fixture/input_validators/main.py"));
        assert!(paths.contains("fixture/submissions/accepted/01_accepted.py"));
    }

    #[test]
    fn refuses_to_overwrite_an_existing_archive() {
        let temporary = tempfile::tempdir().unwrap();
        super::super::init_project(temporary.path(), "ICPC Fixture").unwrap();
        let mut manifest =
            super::super::read_manifest(&temporary.path().join("reporch.problem.json")).unwrap();
        add_validator(
            &mut manifest,
            temporary.path(),
            "validators/main.py",
            b"raise SystemExit(0)\n",
        );
        let output = temporary.path().join("fixture.zip");
        fs::write(&output, b"keep").unwrap();

        let error = export_icpc_2025_09(&manifest, temporary.path(), &output).unwrap_err();
        assert!(error.to_string().contains("create new export archive"));
        assert_eq!(fs::read(output).unwrap(), b"keep");
    }

    #[test]
    fn exports_scored_groups_dependencies_and_partial_score_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        super::super::init_project(temporary.path(), "Scored Fixture").unwrap();
        let mut manifest =
            super::super::read_manifest(&temporary.path().join("reporch.problem.json")).unwrap();
        add_validator(
            &mut manifest,
            temporary.path(),
            "validators/main.py",
            b"raise SystemExit(0)\n",
        );
        let second_input = b"other\n";
        let second_answer = b"other\n";
        fs::write(temporary.path().join("tests/2.in"), second_input).unwrap();
        fs::write(temporary.path().join("tests/2.ans"), second_answer).unwrap();
        manifest.files.extend([
            studio_core::ManifestFile {
                path: "tests/2.in".into(),
                sha256: studio_core::Sha256Digest::from_bytes(second_input),
                size_bytes: second_input.len() as u64,
                media_type: "text/plain".into(),
                executable: false,
            },
            studio_core::ManifestFile {
                path: "tests/2.ans".into(),
                sha256: studio_core::Sha256Digest::from_bytes(second_answer),
                size_bytes: second_answer.len() as u64,
                media_type: "text/plain".into(),
                executable: false,
            },
        ]);
        manifest.problem_type = ProblemType::Scored;
        manifest.judging.groups = vec![
            studio_core::TestGroupSpec {
                id: "subtask1".into(),
                points: 50.0,
                depends_on: vec![],
                feedback_policy: studio_core::GroupFeedbackPolicyV1::Complete,
            },
            studio_core::TestGroupSpec {
                id: "subtask2".into(),
                points: 50.0,
                depends_on: vec!["subtask1".into()],
                feedback_policy: studio_core::GroupFeedbackPolicyV1::Complete,
            },
        ];
        manifest.judging.tests[0].groups = vec!["subtask1".into()];
        manifest.judging.tests.push(studio_core::TestCaseSpec {
            id: uuid::Uuid::now_v7(),
            name: "second".into(),
            input_file: "tests/2.in".into(),
            answer_file: Some("tests/2.ans".into()),
            groups: vec!["subtask2".into()],
            generated_by: None,
            generator_arguments: vec![],
            seed: None,
        });
        let partial = manifest.solutions.last_mut().unwrap();
        partial.expected_verdict = ExpectedVerdict::Partial;
        partial.expected_score = Some(studio_core::ExpectedScoreRange {
            minimum: 50.0,
            maximum: 50.0,
        });
        let output = temporary.path().join("scored.zip");

        export_icpc_2025_09(&manifest, temporary.path(), &output).unwrap();

        let mut archive = zip::ZipArchive::new(File::open(output).unwrap()).unwrap();
        let mut problem_yaml = String::new();
        archive
            .by_name("scored/problem.yaml")
            .unwrap()
            .read_to_string(&mut problem_yaml)
            .unwrap();
        assert!(problem_yaml.contains("type: scoring"));
        let paths = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_owned())
            .collect::<BTreeSet<_>>();
        assert!(paths.contains("scored/data/secret/subtask1/test_group.yaml"));
        assert!(paths.contains("scored/data/secret/subtask2/0002.in"));
        assert!(paths.contains("scored/submissions/submissions.yaml"));
        assert!(paths.contains("scored/submissions/rejected/03_known-wrong.py"));

        let mut group_yaml = String::new();
        archive
            .by_name("scored/data/secret/subtask2/test_group.yaml")
            .unwrap()
            .read_to_string(&mut group_yaml)
            .unwrap();
        assert!(group_yaml.contains("max_score: 50"));
        assert!(group_yaml.contains("require_pass: [\"secret/subtask1\"]"));

        let mut submissions_yaml = String::new();
        archive
            .by_name("scored/submissions/submissions.yaml")
            .unwrap()
            .read_to_string(&mut submissions_yaml)
            .unwrap();
        assert!(submissions_yaml.contains("\"rejected/03_known-wrong.py\":"));
        assert!(submissions_yaml.contains("score: 50"));
    }

    #[test]
    fn refuses_local_files_that_no_longer_match_the_manifest() {
        let temporary = tempfile::tempdir().unwrap();
        super::super::init_project(temporary.path(), "ICPC Fixture").unwrap();
        let mut manifest =
            super::super::read_manifest(&temporary.path().join("reporch.problem.json")).unwrap();
        add_validator(
            &mut manifest,
            temporary.path(),
            "validators/main.py",
            b"raise SystemExit(0)\n",
        );
        fs::write(
            temporary.path().join("solutions/accepted.py"),
            b"print('tampered')\n",
        )
        .unwrap();
        let output = temporary.path().join("fixture.zip");

        let error = export_icpc_2025_09(&manifest, temporary.path(), &output).unwrap_err();
        assert!(error.to_string().contains("source file size changed"));
        assert!(!output.exists());
    }

    #[test]
    fn round_trips_submit_answer_outputs_with_exact_test_identity_and_hashes() {
        let temporary = tempfile::tempdir().unwrap();
        super::super::init_project(temporary.path(), "Output Only Fixture").unwrap();
        let mut manifest =
            super::super::read_manifest(&temporary.path().join("reporch.problem.json")).unwrap();
        add_validator(
            &mut manifest,
            temporary.path(),
            "validators/main.py",
            b"raise SystemExit(0)\n",
        );
        fs::create_dir_all(temporary.path().join("outputs")).unwrap();
        let accepted = b"sample\n";
        let wrong = b"wrong\n";
        fs::write(temporary.path().join("outputs/accepted.out"), accepted).unwrap();
        fs::write(temporary.path().join("outputs/wrong.out"), wrong).unwrap();
        manifest.files.extend([
            studio_core::ManifestFile {
                path: "outputs/accepted.out".into(),
                sha256: studio_core::Sha256Digest::from_bytes(accepted),
                size_bytes: accepted.len() as u64,
                media_type: "text/plain".into(),
                executable: false,
            },
            studio_core::ManifestFile {
                path: "outputs/wrong.out".into(),
                sha256: studio_core::Sha256Digest::from_bytes(wrong),
                size_bytes: wrong.len() as u64,
                media_type: "text/plain".into(),
                executable: false,
            },
        ]);
        manifest.problem_type = ProblemType::OutputOnly;
        manifest.solutions.clear();
        let test_id = manifest.judging.tests[0].id;
        manifest.output_submissions = vec![
            studio_core::OutputSubmissionSpec {
                name: "official".into(),
                outputs: std::collections::BTreeMap::from([(
                    test_id,
                    "outputs/accepted.out".into(),
                )]),
                expected_verdict: ExpectedVerdict::Accepted,
                expected_score: None,
            },
            studio_core::OutputSubmissionSpec {
                name: "known wrong".into(),
                outputs: std::collections::BTreeMap::from([(test_id, "outputs/wrong.out".into())]),
                expected_verdict: ExpectedVerdict::WrongAnswer,
                expected_score: None,
            },
        ];
        assert!(validate_manifest(&manifest).is_empty());
        let output = temporary.path().join("submitanswer.zip");

        export_icpc_2025_09(&manifest, temporary.path(), &output).unwrap();

        let mut archive = zip::ZipArchive::new(File::open(&output).unwrap()).unwrap();
        let mut yaml = String::new();
        archive
            .by_name("submitanswer/problem.yaml")
            .unwrap()
            .read_to_string(&mut yaml)
            .unwrap();
        assert!(yaml.contains("type: [pass-fail, submit-answer]"));
        assert!(
            archive
                .by_name("submitanswer/-reporch-submit-answer.json")
                .is_ok()
        );
        assert!(
            archive
                .by_name("submitanswer/submissions/accepted/01_official/0001.out")
                .is_ok()
        );
        assert!(
            archive
                .by_name("submitanswer/submissions/wrong_answer/02_known_wrong/0001.out")
                .is_ok()
        );
        drop(archive);

        let imported = super::super::icpc_import::import_icpc_2025_09(
            &output,
            &temporary.path().join("submitanswer-imported"),
        )
        .unwrap();
        assert_eq!(imported.problem_type, ProblemType::OutputOnly);
        assert_eq!(imported.judging.tests[0].id, test_id);
        assert_eq!(imported.output_submissions.len(), 2);
        assert!(validate_manifest(&imported).is_empty());
        for submission in &imported.output_submissions {
            let path = submission.outputs.get(&test_id).unwrap();
            let file = imported
                .files
                .iter()
                .find(|file| file.path == *path)
                .unwrap();
            let expected = if submission.expected_verdict == ExpectedVerdict::Accepted {
                studio_core::Sha256Digest::from_bytes(accepted)
            } else {
                studio_core::Sha256Digest::from_bytes(wrong)
            };
            assert_eq!(file.sha256, expected);
        }
    }

    #[test]
    fn exports_domjudge_metadata_on_the_icpc_base_format() {
        let temporary = tempfile::tempdir().unwrap();
        super::super::init_project(temporary.path(), "DOMjudge Fixture").unwrap();
        let mut manifest =
            super::super::read_manifest(&temporary.path().join("reporch.problem.json")).unwrap();
        add_validator(
            &mut manifest,
            temporary.path(),
            "validators/main.py",
            b"raise SystemExit(0)\n",
        );
        let output = temporary.path().join("fixture.zip");

        export_domjudge_zip(&manifest, temporary.path(), &output).unwrap();

        let mut archive = zip::ZipArchive::new(File::open(&output).unwrap()).unwrap();
        let mut ini = String::new();
        archive
            .by_name("fixture/domjudge-problem.ini")
            .unwrap()
            .read_to_string(&mut ini)
            .unwrap();
        assert!(ini.contains("name = \"DOMjudge Fixture\""));
        assert!(ini.contains("timelimit = 1.000"));
        assert!(ini.contains("allow_submit = 1"));
        assert!(ini.contains("short-name = \"fixture\""));

        let mut report = String::new();
        archive
            .by_name("fixture/-reporch-compatibility.json")
            .unwrap()
            .read_to_string(&mut report)
            .unwrap();
        assert!(report.contains("\"target_profile\": \"domjudge_zip\""));

        let imported = super::super::icpc_import::import_domjudge_zip(
            &output,
            &temporary.path().join("imported"),
        )
        .unwrap();
        assert_eq!(imported.package_profile, PackageProfile::DomjudgeZip);
        assert!(validate_manifest(&imported).is_empty());
    }

    #[test]
    fn round_trips_an_interactive_package_with_typed_harness() {
        let temporary = tempfile::tempdir().unwrap();
        super::super::init_project(temporary.path(), "Interactive Fixture").unwrap();
        let mut manifest =
            super::super::read_manifest(&temporary.path().join("reporch.problem.json")).unwrap();
        add_validator(
            &mut manifest,
            temporary.path(),
            "validators/main.py",
            b"raise SystemExit(0)\n",
        );
        fs::create_dir_all(temporary.path().join("interactive")).unwrap();
        let solver_path = "solutions/accepted.cpp";
        let solver = b"#include <iostream>\nint main(){return 0;}\n";
        let interactor_path = "interactive/interactor.cpp";
        let interactor = b"#include <iostream>\nint main(){return 0;}\n";
        fs::write(temporary.path().join(solver_path), solver).unwrap();
        fs::write(temporary.path().join(interactor_path), interactor).unwrap();
        manifest.files.extend([
            studio_core::ManifestFile {
                path: solver_path.into(),
                sha256: studio_core::Sha256Digest::from_bytes(solver),
                size_bytes: solver.len() as u64,
                media_type: "text/x-c++src".into(),
                executable: false,
            },
            studio_core::ManifestFile {
                path: interactor_path.into(),
                sha256: studio_core::Sha256Digest::from_bytes(interactor),
                size_bytes: interactor.len() as u64,
                media_type: "text/x-c++src".into(),
                executable: false,
            },
        ]);
        manifest.problem_type = ProblemType::Interactive;
        manifest.solutions[0].source_path = solver_path.into();
        manifest.solutions[0].language = "cpp20".into();
        manifest.judging.interactor_path = Some(interactor_path.into());
        manifest.judging.interactor_language = Some("cpp20".into());
        manifest.judging.harness = Some(studio_core::ExecutionHarnessV1::InteractiveStdio {
            profiles: std::collections::BTreeMap::from([(
                "cpp20".into(),
                studio_core::InteractiveStdioProfileV1 {
                    source_path: solver_path.into(),
                    interactor_source_path: interactor_path.into(),
                    asset_paths: vec![solver_path.into(), interactor_path.into()],
                    include_dirs: vec![],
                    idle_timeout_ms: 2_000,
                    transcript_limit_kib: 1_024,
                    solver_compile_command: None,
                    solver_run_command: None,
                    interactor_compile_command: None,
                    interactor_run_command: None,
                },
            )]),
            score_type: studio_core::ScoreAggregation::AllOrNothing,
            score_scale: 100,
        });
        let output = temporary.path().join("interactive.zip");

        export_icpc_2025_09(&manifest, temporary.path(), &output).unwrap();

        let mut archive = zip::ZipArchive::new(File::open(&output).unwrap()).unwrap();
        let mut problem_yaml = String::new();
        archive
            .by_name("interactive/problem.yaml")
            .unwrap()
            .read_to_string(&mut problem_yaml)
            .unwrap();
        assert!(problem_yaml.contains("type: interactive"));
        assert!(
            archive
                .by_name("interactive/output_validator/interactor.cpp")
                .is_ok()
        );

        let imported = super::super::icpc_import::import_icpc_2025_09(
            &output,
            &temporary.path().join("interactive-imported"),
        )
        .unwrap();
        assert_eq!(imported.problem_type, ProblemType::Interactive);
        assert!(validate_manifest(&imported).is_empty());
    }
}
