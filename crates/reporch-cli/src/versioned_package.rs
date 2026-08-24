use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use studio_core::{
    CheckerTestSpec, CompatibilityIssueV1, CompatibilityReportV1, CompatibilitySeverity,
    CustomImplExpectedOutputMode, CustomImplInputMode, CustomImplProfileV1, ExecutionHarnessV1,
    ExpectedVerdict, InteractiveStdioProfileV1, JudgingSpec, OutputSubmissionSpec, PackageProfile,
    ProgramSpec, RELEASE_MANIFEST_SCHEMA_V1, ReleaseManifestV1, ReleaseManifestV2,
    ScoreAggregation, Sha256Digest, SolutionSpec, TestCaseSpec, TestGroupSpec, ValidatorTestSpec,
    VersionedReleaseManifest, compatibility_report, normalize_relative_path,
};
use tempfile::Builder;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, System, ZipArchive, ZipWriter};

const PREFIX: &str = "-reporch-v2/";
const SIDECAR_PATH: &str = "-reporch-v2/sidecar-v1.json";
const REPORT_PATH: &str = "-reporch-v2/loss-report-v1.json";
const SIDECAR_SCHEMA: &str = "reporch.external-v2-sidecar.v1";
const MAX_ENTRIES: usize = 50_000;
const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_SIDECAR_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionedSidecarV1 {
    schema: String,
    target_profile: PackageProfile,
    manifest: VersionedReleaseManifest,
    manifest_digest: Sha256Digest,
    files: Vec<SidecarFileV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarFileV1 {
    path: String,
    archive_path: String,
    sha256: Sha256Digest,
    size_bytes: u64,
    executable: bool,
}

pub fn export_v2_with_sidecar<F>(
    manifest: &ReleaseManifestV2,
    source_root: &Path,
    output: &Path,
    profile: PackageProfile,
    export_projection: F,
) -> Result<CompatibilityReportV1>
where
    F: FnOnce(&ReleaseManifestV1, &Path, &Path) -> Result<()>,
{
    manifest.validate_references()?;
    ensure!(
        profile != PackageProfile::ReporchNative,
        "native export has its own V2 format"
    );
    ensure!(!output.exists(), "export destination already exists");
    ensure!(
        manifest.files.len() <= MAX_ENTRIES,
        "manifest exceeds the external sidecar file-count limit"
    );
    ensure!(
        manifest
            .files
            .iter()
            .all(|file| !file.path.starts_with(PREFIX)),
        "manifest path collides with the reserved V2 sidecar prefix"
    );
    let projection = project_v2_to_v1(manifest)?;
    let report = v2_compatibility_report(manifest, &projection, profile);
    ensure!(
        report.exportable,
        "package export is blocked: {}",
        serde_json::to_string(&report)?
    );
    let source_root = fs::canonicalize(source_root)
        .with_context(|| format!("resolve source root {}", source_root.display()))?;
    ensure!(source_root.is_dir(), "source root is not a directory");
    let output_parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure!(
        output_parent.is_dir(),
        "export destination parent does not exist"
    );
    let temporary = Builder::new()
        .prefix(".reporch-external-v2-")
        .tempdir_in(output_parent)?;
    let archive_path = temporary.path().join(
        output
            .file_name()
            .context("export destination has no filename")?,
    );
    export_projection(&projection, &source_root, &archive_path)?;
    append_sidecar(&archive_path, manifest, &source_root, profile, &report)?;
    fs::rename(&archive_path, output)
        .with_context(|| format!("install external V2 package {}", output.display()))?;
    Ok(report)
}

pub fn compatibility_v2(
    manifest: &ReleaseManifestV2,
    profile: PackageProfile,
) -> Result<CompatibilityReportV1> {
    if profile == PackageProfile::ReporchNative {
        return Ok(CompatibilityReportV1 {
            schema: studio_core::COMPATIBILITY_REPORT_SCHEMA_V1.into(),
            source_profile: manifest.package_profile,
            target_profile: profile,
            exportable: true,
            lossless: true,
            issues: vec![],
        });
    }
    Ok(v2_compatibility_report(
        manifest,
        &project_v2_to_v1(manifest)?,
        profile,
    ))
}

pub fn contains_v2_sidecar(input: &Path) -> Result<bool> {
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut archive = ZipArchive::new(file).context("read package ZIP")?;
    crate::archive_safety::validate_zip_resource_budget(
        &mut archive,
        MAX_ENTRIES + 20_000,
        MAX_TOTAL_BYTES + 2 * MAX_SIDECAR_BYTES,
    )?;
    Ok(archive.by_name(SIDECAR_PATH).is_ok())
}

pub fn import_v2_sidecar(
    input: &Path,
    directory: &Path,
    expected_profile: PackageProfile,
) -> Result<VersionedReleaseManifest> {
    ensure!(!directory.exists(), "import destination already exists");
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut archive = ZipArchive::new(file).context("read package ZIP")?;
    ensure!(
        archive.len() <= MAX_ENTRIES + 20_000,
        "package has too many entries"
    );
    let sidecar_bytes = read_named_limited(&mut archive, SIDECAR_PATH, MAX_SIDECAR_BYTES)?;
    let sidecar: VersionedSidecarV1 =
        serde_json::from_slice(&sidecar_bytes).context("parse external V2 sidecar")?;
    ensure!(
        sidecar.schema == SIDECAR_SCHEMA,
        "unsupported external V2 sidecar schema"
    );
    ensure!(
        sidecar.target_profile == expected_profile,
        "external V2 sidecar profile does not match the selected importer"
    );
    ensure!(
        matches!(sidecar.manifest, VersionedReleaseManifest::V2(_)),
        "external V2 sidecar must contain a V2 manifest"
    );
    sidecar.manifest.validate_references()?;
    ensure!(
        sidecar.manifest.digest()? == sidecar.manifest_digest,
        "external V2 sidecar manifest digest mismatch"
    );
    ensure!(
        sidecar.files.len() == sidecar.manifest.files().len(),
        "external V2 sidecar file inventory length mismatch"
    );
    let mut declared_paths = BTreeSet::new();
    let mut archive_paths = BTreeSet::new();
    let manifest_files = sidecar
        .manifest
        .files()
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let mut total = 0_u64;
    for entry in &sidecar.files {
        ensure!(
            declared_paths.insert(entry.path.as_str()),
            "duplicate external V2 sidecar file path"
        );
        ensure!(
            archive_paths.insert(entry.archive_path.as_str())
                && entry.archive_path.starts_with(&format!("{PREFIX}files/")),
            "invalid external V2 sidecar payload path"
        );
        let manifest_file = manifest_files
            .get(entry.path.as_str())
            .context("external V2 sidecar references an undeclared manifest file")?;
        ensure!(
            entry.sha256 == manifest_file.sha256
                && entry.size_bytes == manifest_file.size_bytes
                && entry.executable == manifest_file.executable,
            "external V2 sidecar metadata disagrees with its manifest"
        );
        ensure!(
            entry.size_bytes <= MAX_FILE_BYTES,
            "sidecar file exceeds 1 GiB"
        );
        total = total
            .checked_add(entry.size_bytes)
            .context("sidecar total size overflow")?;
        ensure!(total <= MAX_TOTAL_BYTES, "sidecar files exceed 5 GiB");
    }

    fs::create_dir_all(directory)
        .with_context(|| format!("create import destination {}", directory.display()))?;
    let result = (|| -> Result<()> {
        for entry in &sidecar.files {
            let path = normalize_relative_path(&entry.path)?;
            let destination = directory.join(&path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut source = archive
                .by_name(&entry.archive_path)
                .with_context(|| format!("missing V2 payload {}", entry.archive_path))?;
            ensure!(
                !source.is_dir()
                    && source.size() == entry.size_bytes
                    && source.unix_mode().unwrap_or(0o100644) & 0o170000 != 0o120000,
                "unsafe or mismatched V2 payload entry"
            );
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)?;
            let (size, digest) = copy_digest(&mut source, &mut output, entry.size_bytes)?;
            output.sync_all()?;
            ensure!(
                size == entry.size_bytes && digest == entry.sha256,
                "V2 payload digest mismatch"
            );
            set_executable(&destination, entry.executable)?;
        }
        write_new(
            &directory.join("reporch.problem.json"),
            &serde_json::to_vec_pretty(&sidecar.manifest)?,
        )?;
        let archived_report = read_named_limited(&mut archive, REPORT_PATH, MAX_SIDECAR_BYTES)?;
        let archived_report: CompatibilityReportV1 = serde_json::from_slice(&archived_report)
            .context("parse external V2 compatibility report")?;
        let VersionedReleaseManifest::V2(manifest) = &sidecar.manifest else {
            unreachable!("V2 sidecar version was checked before extraction")
        };
        let canonical_report = compatibility_v2(manifest, expected_profile)?;
        ensure!(
            archived_report == canonical_report,
            "external V2 compatibility report is not bound to the validated manifest"
        );
        write_new(
            &directory.join("reporch.import-report.json"),
            &serde_json::to_vec_pretty(&canonical_report)?,
        )?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(directory);
        return Err(error);
    }
    Ok(sidecar.manifest)
}

fn append_sidecar(
    archive_path: &Path,
    manifest: &ReleaseManifestV2,
    source_root: &Path,
    profile: PackageProfile,
    report: &CompatibilityReportV1,
) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(archive_path)?;
    let mut archive = ZipWriter::new_append(file).context("append V2 sidecar to package")?;
    let files = manifest
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| SidecarFileV1 {
            path: file.path.clone(),
            archive_path: format!("{PREFIX}files/{index:08}.bin"),
            sha256: file.sha256.clone(),
            size_bytes: file.size_bytes,
            executable: file.executable,
        })
        .collect::<Vec<_>>();
    let sidecar = VersionedSidecarV1 {
        schema: SIDECAR_SCHEMA.into(),
        target_profile: profile,
        manifest: manifest.clone().into(),
        manifest_digest: manifest.digest()?,
        files: files.clone(),
    };
    write_zip_bytes(
        &mut archive,
        SIDECAR_PATH,
        &serde_json::to_vec(&sidecar)?,
        false,
    )?;
    write_zip_bytes(
        &mut archive,
        REPORT_PATH,
        &serde_json::to_vec_pretty(report)?,
        false,
    )?;
    for (entry, file) in files.iter().zip(&manifest.files) {
        archive.start_file(
            &entry.archive_path,
            zip_options(file.executable, file.size_bytes),
        )?;
        let source = source_root.join(&file.path);
        let mut source =
            File::open(&source).with_context(|| format!("open V2 sidecar source {}", file.path))?;
        let (size, digest) = copy_digest(&mut source, &mut archive, file.size_bytes)?;
        ensure!(
            size == file.size_bytes && digest == file.sha256,
            "source changed during V2 sidecar export"
        );
    }
    archive.finish()?.sync_all()?;
    Ok(())
}

fn project_v2_to_v1(manifest: &ReleaseManifestV2) -> Result<ReleaseManifestV1> {
    let group_names = manifest
        .testing
        .groups
        .iter()
        .map(|group| (group.id, group.name.clone()))
        .collect::<BTreeMap<_, _>>();
    ensure!(
        group_names.values().collect::<BTreeSet<_>>().len() == group_names.len(),
        "V2 group names must be unique for external projection"
    );
    let generator_names = manifest
        .testing
        .generators
        .iter()
        .map(|generator| (generator.program.id, generator.program.name.clone()))
        .collect::<BTreeMap<_, _>>();
    let recipes = manifest
        .testing
        .generators
        .iter()
        .flat_map(|generator| {
            generator
                .recipes
                .iter()
                .map(move |recipe| ((generator.program.id, recipe.id), recipe))
        })
        .collect::<BTreeMap<_, _>>();
    let tests = manifest
        .testing
        .tests
        .iter()
        .map(|test| {
            let recipe = test.generated.as_ref().and_then(|generated| {
                recipes
                    .get(&(generated.generator_id, generated.recipe_id))
                    .copied()
            });
            Ok(TestCaseSpec {
                id: test.id,
                name: test.name.clone(),
                input_file: test.input_file.clone(),
                answer_file: test.answer_file.clone(),
                groups: test
                    .group_ids
                    .iter()
                    .map(|id| {
                        group_names
                            .get(id)
                            .cloned()
                            .context("V2 test group is missing")
                    })
                    .collect::<Result<Vec<_>>>()?,
                generated_by: test
                    .generated
                    .as_ref()
                    .and_then(|generated| generator_names.get(&generated.generator_id).cloned()),
                generator_arguments: recipe
                    .map_or_else(Vec::new, |recipe| recipe.argument_template.clone()),
                seed: test.generated.as_ref().map(|generated| generated.seed),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let groups = manifest
        .testing
        .groups
        .iter()
        .map(|group| {
            Ok(TestGroupSpec {
                id: group.name.clone(),
                points: group.points,
                depends_on: group
                    .depends_on
                    .iter()
                    .map(|id| {
                        group_names
                            .get(id)
                            .cloned()
                            .context("V2 group dependency is missing")
                    })
                    .collect::<Result<Vec<_>>>()?,
                feedback_policy: group.feedback_policy,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let primary = manifest.testing.validators.primary.as_ref();
    let extra_validators = manifest
        .testing
        .validators
        .extra
        .iter()
        .map(program_v1)
        .collect();
    let interactor = manifest.execution.interactive.as_ref();
    let grader = manifest
        .execution
        .harness
        .as_ref()
        .and_then(|harness| harness.private_files.first().cloned())
        .or_else(|| {
            manifest
                .execution
                .harness
                .as_ref()
                .and_then(|harness| harness.profiles.values().next())
                .map(|profile| profile.source_path.clone())
        });
    let harness = project_harness_v1(manifest)?;
    Ok(ReleaseManifestV1 {
        schema: RELEASE_MANIFEST_SCHEMA_V1.into(),
        project_id: manifest.project_id,
        commit_id: manifest.commit_id,
        problem_type: manifest.problem_type,
        package_profile: manifest.package_profile,
        default_locale: manifest.default_locale.clone(),
        title: manifest.title.clone(),
        statements: manifest.statements.clone(),
        files: manifest.files.clone(),
        toolchains: manifest.toolchains.clone(),
        judging: JudgingSpec {
            limits: manifest.testing.limits.clone(),
            checker: manifest.testing.checker.checker.clone(),
            tests,
            groups,
            generators: manifest
                .testing
                .generators
                .iter()
                .map(|generator| program_v1(&generator.program))
                .collect(),
            validator_path: primary.map(|program| program.source_path.clone()),
            validator_language: primary.map(|program| program.language.clone()),
            extra_validator_paths: vec![],
            extra_validators,
            validator_tests: manifest
                .testing
                .validators
                .unit_tests
                .iter()
                .map(|test| ValidatorTestSpec {
                    name: test.name.clone(),
                    input_file: test.input_file.clone(),
                    expected_valid: test.expected_valid,
                })
                .collect(),
            checker_tests: manifest
                .testing
                .checker
                .unit_tests
                .iter()
                .map(|test| CheckerTestSpec {
                    name: test.name.clone(),
                    input_file: test.input_file.clone(),
                    answer_file: test.answer_file.clone(),
                    output_file: test.output_file.clone(),
                    expected_accepted: test.expected_accepted,
                })
                .collect(),
            interactor_path: interactor.map(|value| value.interactor.source_path.clone()),
            interactor_language: interactor.map(|value| value.interactor.language.clone()),
            grader_path: grader,
            grader_language: manifest
                .execution
                .harness
                .as_ref()
                .and_then(|harness| harness.profiles.values().next())
                .map(|profile| profile.language.clone()),
            harness,
        },
        sources: manifest.sources.clone(),
        solutions: manifest
            .testing
            .solutions
            .iter()
            .map(|solution| SolutionSpec {
                name: solution.program.name.clone(),
                source_path: solution.program.source_path.clone(),
                language: solution.program.language.clone(),
                expected_verdict: solution.expected_verdict,
                expected_score: solution.expected_score.clone(),
            })
            .collect(),
        output_submissions: manifest
            .output_submissions
            .iter()
            .map(|submission| OutputSubmissionSpec {
                name: submission.name.clone(),
                outputs: submission.outputs.clone(),
                expected_verdict: submission.expected_verdict,
                expected_score: submission.expected_score.clone(),
            })
            .collect(),
        publication: manifest.publication.clone(),
        policy_version: manifest.policy_version.clone(),
    })
}

fn program_v1(program: &studio_core::ProgramSpecV2) -> ProgramSpec {
    ProgramSpec {
        id: program.name.clone(),
        source_path: program.source_path.clone(),
        language: program.language.clone(),
        arguments: program.arguments.clone(),
    }
}

fn project_harness_v1(manifest: &ReleaseManifestV2) -> Result<Option<ExecutionHarnessV1>> {
    if let Some(interactive) = &manifest.execution.interactive {
        let solver = manifest
            .testing
            .solutions
            .iter()
            .find(|solution| solution.expected_verdict == ExpectedVerdict::Accepted)
            .context("interactive external projection requires an accepted solution")?;
        return Ok(Some(ExecutionHarnessV1::InteractiveStdio {
            profiles: BTreeMap::from([(
                solver.program.language.clone(),
                InteractiveStdioProfileV1 {
                    source_path: solver.program.source_path.clone(),
                    interactor_source_path: interactive.interactor.source_path.clone(),
                    asset_paths: vec![
                        solver.program.source_path.clone(),
                        interactive.interactor.source_path.clone(),
                    ],
                    include_dirs: vec![],
                    idle_timeout_ms: interactive.idle_timeout_ms,
                    transcript_limit_kib: interactive.transcript_limit_kib,
                    solver_compile_command: None,
                    solver_run_command: None,
                    interactor_compile_command: None,
                    interactor_run_command: None,
                },
            )]),
            score_type: ScoreAggregation::GroupMin,
            score_scale: 100,
        }));
    }
    let Some(harness) = &manifest.execution.harness else {
        return Ok(None);
    };
    Ok(Some(ExecutionHarnessV1::CustomImpl {
        profiles: harness
            .profiles
            .iter()
            .map(|(language, profile)| {
                Ok((
                    language.clone(),
                    CustomImplProfileV1 {
                        source_path: profile
                            .submission_source_path
                            .clone()
                            .context(
                                "V2 harness profile requires a contestant submission template for external projection",
                            )?,
                        asset_paths: profile.asset_paths.clone(),
                        compile_script: profile.compile_script.clone(),
                        run_script: profile.run_script.clone(),
                        compile_command: profile.compile_command.clone(),
                        run_command: profile.run_command.clone(),
                    },
                ))
            })
            .collect::<Result<_>>()?,
        input_mode: CustomImplInputMode::Raw,
        expected_output_mode: CustomImplExpectedOutputMode::Raw,
    }))
}

fn v2_compatibility_report(
    manifest: &ReleaseManifestV2,
    projection: &ReleaseManifestV1,
    profile: PackageProfile,
) -> CompatibilityReportV1 {
    let mut report = compatibility_report(projection, profile);
    let mut add_warning = |code: &str, message: &str, path: &str| {
        report.issues.push(CompatibilityIssueV1 {
            code: code.into(),
            severity: CompatibilitySeverity::Warning,
            message: message.into(),
            path: Some(path.into()),
        });
    };
    if !manifest.tutorials.is_empty() {
        add_warning(
            "compatibility.v2_tutorial_sidecar",
            "localized tutorials are preserved only in the Reporch V2 sidecar",
            "tutorials",
        );
    }
    if !manifest.testing.stress_suites.is_empty() {
        add_warning(
            "compatibility.v2_stress_sidecar",
            "stress suites are preserved only in the Reporch V2 sidecar",
            "testing.stress_suites",
        );
    }
    if manifest
        .testing
        .generators
        .iter()
        .any(|generator| !generator.recipes.is_empty())
    {
        add_warning(
            "compatibility.v2_generator_recipe_sidecar",
            "typed generator recipes are preserved only in the Reporch V2 sidecar",
            "testing.generators",
        );
    }
    report.lossless = report.issues.is_empty();
    report
}

fn read_named_limited(archive: &mut ZipArchive<File>, name: &str, maximum: u64) -> Result<Vec<u8>> {
    let entry = archive
        .by_name(name)
        .with_context(|| format!("missing {name}"))?;
    ensure!(
        !entry.is_dir() && entry.size() <= maximum,
        "unsafe or oversized {name}"
    );
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry
        .take(maximum + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {name}"))?;
    ensure!(bytes.len() as u64 <= maximum, "oversized {name}");
    Ok(bytes)
}

fn copy_digest<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    maximum: u64,
) -> Result<(u64, Sha256Digest)> {
    let mut size = 0_u64;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("file size overflow")?;
        ensure!(size <= maximum, "file exceeds its declared size");
        digest.update(&buffer[..read]);
        writer.write_all(&buffer[..read])?;
    }
    Ok((
        size,
        hex::encode(digest.finalize())
            .parse()
            .context("generated digest is invalid")?,
    ))
}

fn write_zip_bytes(
    archive: &mut ZipWriter<File>,
    path: &str,
    bytes: &[u8],
    executable: bool,
) -> Result<()> {
    archive.start_file(path, zip_options(executable, bytes.len() as u64))?;
    archive.write_all(bytes)?;
    Ok(())
}

fn zip_options(executable: bool, size: u64) -> SimpleFileOptions {
    SimpleFileOptions::DEFAULT
        .compression_method(CompressionMethod::Stored)
        .system(System::Unix)
        .unix_permissions(if executable { 0o755 } else { 0o644 })
        .large_file(size >= u32::MAX as u64)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut output = OpenOptions::new().write(true).create_new(true).open(path)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
    )?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _executable: bool) -> Result<()> {
    Ok(())
}
