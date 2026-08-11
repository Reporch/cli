use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use quick_xml::Reader;
use quick_xml::events::Event;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use studio_core::{
    CheckerSpec, CheckerTestSpec, ExecutionHarnessV1, ExpectedVerdict, InteractiveStdioProfileV1,
    JudgingSpec, ManifestFile, PackageProfile, ProblemType, ProgramSpec, PublicationSampleV1,
    PublicationSpecV1, RELEASE_MANIFEST_SCHEMA_V1, ReleaseManifestV1, ResourceLimits,
    ScoreAggregation, Sha256Digest, SolutionSpec, SourceAttribution, StatementSectionsV1,
    TestCaseSpec, TestGroupSpec, ValidatorTestSpec, normalize_relative_path, validate_manifest,
};
use uuid::Uuid;
use zip::ZipArchive;

use crate::polygon_export::{
    POLYGON_REPORT_PATH, POLYGON_SIDECAR_PATH, POLYGON_SIDECAR_SCHEMA_V1, PolygonSidecarV1,
};

const MAX_ARCHIVE_FILES: usize = 50_000;
const MAX_ARCHIVE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_CONTROL_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug)]
struct ScannedEntry {
    archive_name: String,
    path: String,
    size: u64,
    executable: bool,
    directory: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalProblemXml {
    #[serde(rename = "@revision")]
    revision: Option<u64>,
    #[serde(rename = "@short-name")]
    short_name: String,
    #[serde(rename = "@name")]
    name: Option<String>,
    names: ExternalNamesXml,
    judging: ExternalJudgingXml,
    files: ExternalFilesXml,
    solutions: ExternalSolutionsXml,
    statements: ExternalStatementsXml,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalNamesXml {
    #[serde(rename = "name")]
    values: Vec<ExternalNameXml>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalNameXml {
    #[serde(rename = "@language")]
    language: String,
    #[serde(rename = "@value")]
    value: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalJudgingXml {
    #[serde(rename = "testset")]
    testsets: Vec<ExternalTestsetXml>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalTestsetXml {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "time-limit")]
    time_limit: u64,
    #[serde(rename = "memory-limit")]
    memory_limit: u64,
    #[serde(rename = "test-count")]
    test_count: usize,
    #[serde(rename = "input-path-pattern")]
    input_path_pattern: String,
    #[serde(rename = "answer-path-pattern")]
    answer_path_pattern: String,
    tests: ExternalTestsXml,
    groups: ExternalGroupsXml,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalTestsXml {
    #[serde(rename = "test")]
    values: Vec<ExternalTestXml>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalTestXml {
    #[serde(rename = "@method")]
    method: String,
    #[serde(rename = "@sample")]
    sample: bool,
    #[serde(rename = "@description")]
    description: String,
    #[serde(rename = "@group")]
    group: String,
    #[serde(rename = "@points")]
    points: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalGroupsXml {
    #[serde(rename = "group")]
    values: Vec<ExternalGroupXml>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalGroupXml {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@points")]
    points: f64,
    #[serde(rename = "dependency")]
    dependencies: Vec<ExternalDependencyXml>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalDependencyXml {
    #[serde(rename = "@group")]
    group: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
struct ExternalSourceXml {
    #[serde(rename = "@path")]
    path: String,
    #[serde(rename = "@type")]
    source_type: String,
    #[serde(rename = "@language")]
    language: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
struct ExternalProgramXml {
    #[serde(rename = "@path")]
    path: String,
    #[serde(rename = "@type")]
    source_type: String,
    #[serde(rename = "@language")]
    language: String,
    #[serde(rename = "@name")]
    name: String,
    source: Option<ExternalSourceXml>,
    testset: Option<ExternalProgramTestsetXml>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
struct ExternalProgramTestsetXml {
    #[serde(rename = "test-count")]
    test_count: usize,
    #[serde(rename = "input-path-pattern")]
    input_path_pattern: String,
    #[serde(rename = "output-path-pattern")]
    output_path_pattern: String,
    #[serde(rename = "answer-path-pattern")]
    answer_path_pattern: String,
    tests: ExternalProgramTestsXml,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
struct ExternalProgramTestsXml {
    #[serde(rename = "test")]
    values: Vec<ExternalProgramTestXml>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
struct ExternalProgramTestXml {
    #[serde(rename = "@verdict")]
    verdict: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalFilesXml {
    validator: Option<ExternalProgramXml>,
    checker: Option<ExternalProgramXml>,
    interactor: Option<ExternalProgramXml>,
    #[serde(rename = "extra-validator")]
    extra_validators: Vec<ExternalProgramXml>,
    #[serde(rename = "generator")]
    generators: Vec<ExternalProgramXml>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalSolutionsXml {
    #[serde(rename = "solution")]
    values: Vec<ExternalSolutionXml>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalSolutionXml {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@tag")]
    tag: String,
    #[serde(rename = "@path")]
    path: String,
    #[serde(rename = "@language")]
    language: String,
    source: Option<ExternalSourceXml>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalStatementsXml {
    #[serde(rename = "statement")]
    values: Vec<ExternalStatementXml>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExternalStatementXml {
    #[serde(rename = "@language")]
    language: String,
    #[serde(rename = "@path")]
    path: String,
}

struct CreatedDirectory {
    path: PathBuf,
    armed: bool,
}

impl CreatedDirectory {
    fn create(path: &Path) -> Result<Self> {
        ensure!(!path.exists(), "import destination already exists");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create import parent {}", parent.display()))?;
        }
        fs::create_dir(path)
            .with_context(|| format!("create import destination {}", path.display()))?;
        Ok(Self {
            path: path.to_owned(),
            armed: true,
        })
    }

    fn finish(mut self) {
        self.armed = false;
    }
}

impl Drop for CreatedDirectory {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub fn import_polygon_package(input: &Path, directory: &Path) -> Result<ReleaseManifestV1> {
    let source = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut archive = ZipArchive::new(source).context("read Polygon ZIP central directory")?;
    let entries = scan_archive(&mut archive)?;
    let destination = CreatedDirectory::create(directory)?;
    extract_archive(&mut archive, directory, &entries)?;

    if !directory.join(POLYGON_SIDECAR_PATH).is_file() {
        let manifest = import_sidecar_free_package(input, directory, &entries)?;
        destination.finish();
        return Ok(manifest);
    }
    ensure!(
        directory.join(POLYGON_REPORT_PATH).is_file(),
        "Polygon package with a Reporch sidecar is missing its compatibility report"
    );

    let sidecar: PolygonSidecarV1 =
        serde_json::from_slice(&fs::read(directory.join(POLYGON_SIDECAR_PATH))?)
            .context("parse the Polygon package sidecar")?;
    ensure!(
        sidecar.schema == POLYGON_SIDECAR_SCHEMA_V1,
        "unsupported Polygon sidecar schema"
    );
    ensure!(
        sidecar.manifest.digest()? == sidecar.manifest_digest,
        "Polygon sidecar manifest digest mismatch"
    );

    let descriptor = fs::read(directory.join("problem.xml"))?;
    ensure!(
        Sha256Digest::from_bytes(&descriptor) == sidecar.descriptor_sha256,
        "problem.xml does not match the checksummed sidecar"
    );
    validate_problem_xml(&descriptor)?;

    let validation = validate_manifest(&sidecar.manifest);
    ensure!(
        validation.is_empty(),
        "sidecar contains an invalid native manifest: {}",
        serde_json::to_string(&validation)?
    );
    for file in &sidecar.manifest.files {
        verify_file(&directory.join(&file.path), file.size_bytes, &file.sha256)
            .with_context(|| format!("verify imported native file {}", file.path))?;
    }

    write_new(
        &directory.join("reporch.problem.json"),
        &serde_json::to_vec_pretty(&sidecar.manifest)?,
    )?;
    write_new(
        &directory.join("reporch.import-report.json"),
        &serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "reporch.import-report.v1",
            "source_profile": "polygon_compatible",
            "target_profile": "reporch_native",
            "source_archive": input.file_name().and_then(|name| name.to_str()),
            "manifest_digest": sidecar.manifest_digest,
            "descriptor_sha256": sidecar.descriptor_sha256,
            "native_validation_passed": true,
            "notes": [
                "This archive contains the Reporch Polygon sidecar, so native paths, UUIDs, solution expectations, groups, and hashes were restored exactly",
                "problem.xml and Polygon-shaped test aliases remain as preserved import evidence",
                "Credentials and Polygon API signatures are never stored in the package"
            ]
        }))?,
    )?;
    destination.finish();
    Ok(sidecar.manifest)
}

fn import_sidecar_free_package(
    input: &Path,
    directory: &Path,
    entries: &[ScannedEntry],
) -> Result<ReleaseManifestV1> {
    let descriptor_bytes = fs::read(directory.join("problem.xml"))?;
    validate_problem_xml(&descriptor_bytes)?;
    let descriptor: ExternalProblemXml =
        quick_xml::de::from_reader(descriptor_bytes.as_slice()).context("decode problem.xml")?;
    ensure!(
        !descriptor.short_name.trim().is_empty(),
        "problem.xml has an empty short-name"
    );
    let file_entries = entries
        .iter()
        .filter(|entry| !entry.directory && directory.join(&entry.path).is_file())
        .collect::<Vec<_>>();
    let paths = file_entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();
    let files = file_entries
        .iter()
        .map(|entry| manifest_file(directory, &entry.path, entry.executable))
        .collect::<Result<Vec<_>>>()?;
    let mut losses = vec![
        "The package has no Reporch sidecar; native project/commit/test UUIDs were regenerated"
            .to_owned(),
        "Polygon build scripts, compiled binaries, resource advanced properties, and toolchain revisions are preserved as files but are not activated automatically"
            .to_owned(),
    ];

    let testset = descriptor
        .judging
        .testsets
        .iter()
        .find(|testset| testset.name == "tests")
        .or_else(|| {
            (descriptor.judging.testsets.len() == 1).then(|| &descriptor.judging.testsets[0])
        })
        .context("problem.xml has no primary tests testset")?;
    let test_count = if testset.test_count == 0 {
        testset.tests.values.len()
    } else {
        testset.test_count
    };
    ensure!(test_count > 0, "Polygon package contains no tests");
    ensure!(
        testset.tests.values.len() <= test_count,
        "problem.xml declares more test records than test-count"
    );

    let mut groups = testset
        .groups
        .values
        .iter()
        .map(|group| {
            ensure!(!group.name.is_empty(), "Polygon test group name is empty");
            ensure!(
                group.points.is_finite() && group.points >= 0.0,
                "Polygon test group has invalid points"
            );
            Ok(TestGroupSpec {
                id: group.name.clone(),
                points: group.points,
                depends_on: group
                    .dependencies
                    .iter()
                    .map(|dependency| dependency.group.clone())
                    .collect(),
                feedback_policy: studio_core::GroupFeedbackPolicyV1::Complete,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut tests = Vec::with_capacity(test_count);
    let mut samples = Vec::new();
    for index in 1..=test_count {
        let metadata = testset.tests.values.get(index - 1);
        let input_file = expand_test_pattern(&testset.input_path_pattern, index)?;
        let answer_file = if testset.answer_path_pattern.is_empty() {
            None
        } else {
            Some(expand_test_pattern(&testset.answer_path_pattern, index)?)
        };
        ensure!(
            paths.contains(input_file.as_str()),
            "Polygon package is missing test input {input_file}; download a linux or windows package containing generated tests"
        );
        if let Some(answer) = answer_file.as_deref() {
            ensure!(
                paths.contains(answer),
                "Polygon package is missing test answer {answer}; download a generated-test package"
            );
        }
        let mut test_groups = metadata
            .filter(|metadata| !metadata.group.is_empty())
            .map(|metadata| vec![metadata.group.clone()])
            .unwrap_or_default();
        if groups.is_empty()
            && let Some(points) = metadata.and_then(|metadata| metadata.points)
        {
            ensure!(
                points.is_finite() && points >= 0.0,
                "Polygon test has invalid points"
            );
            let group_id = format!("test-{index}");
            groups.push(TestGroupSpec {
                id: group_id.clone(),
                points,
                depends_on: vec![],
                feedback_policy: studio_core::GroupFeedbackPolicyV1::Complete,
            });
            test_groups.push(group_id);
        }
        let name = metadata
            .filter(|metadata| !metadata.description.is_empty())
            .map_or_else(
                || format!("test-{index}"),
                |metadata| metadata.description.clone(),
            );
        let test = TestCaseSpec {
            id: Uuid::now_v7(),
            name: name.clone(),
            input_file: input_file.clone(),
            answer_file: answer_file.clone(),
            groups: test_groups,
            generated_by: None,
            generator_arguments: vec![],
            seed: None,
        };
        if metadata.is_some_and(|metadata| metadata.sample)
            && let Some(output_file) = answer_file
        {
            samples.push(PublicationSampleV1 {
                name,
                input_file,
                output_file,
            });
        }
        if metadata.is_some_and(|metadata| metadata.method != "manual") {
            losses.push(format!(
                "Test {index} generator command was flattened to its packaged input bytes"
            ));
        }
        tests.push(test);
    }

    let validator = descriptor
        .files
        .validator
        .as_ref()
        .and_then(|program| program_spec(program, "validator"));
    let extra_validators = descriptor
        .files
        .extra_validators
        .iter()
        .enumerate()
        .filter_map(|(index, program)| {
            program_spec(program, &format!("extra-validator-{}", index + 1))
        })
        .collect::<Vec<_>>();
    let generators = descriptor
        .files
        .generators
        .iter()
        .enumerate()
        .filter_map(|(index, program)| program_spec(program, &format!("generator-{}", index + 1)))
        .collect::<Vec<_>>();
    for program in validator
        .iter()
        .chain(extra_validators.iter())
        .chain(generators.iter())
    {
        ensure!(
            paths.contains(program.source_path.as_str()),
            "Polygon program source is missing: {}",
            program.source_path
        );
    }
    let validator_tests = descriptor
        .files
        .validator
        .as_ref()
        .and_then(|program| program.testset.as_ref())
        .map(|testset| import_validator_tests(testset, &paths))
        .transpose()?
        .unwrap_or_default();

    let interactor = descriptor
        .files
        .interactor
        .as_ref()
        .and_then(|program| program_spec(program, "interactor"));
    if let Some(program) = interactor.as_ref() {
        ensure!(
            paths.contains(program.source_path.as_str()),
            "Polygon interactor source is missing: {}",
            program.source_path
        );
    }
    let mut checker_losses = Vec::new();
    let checker_program = descriptor
        .files
        .checker
        .as_ref()
        .and_then(|program| program_spec(program, "checker"));
    if let Some(program) = checker_program.as_ref() {
        ensure!(
            paths.contains(program.source_path.as_str()),
            "Polygon checker source is missing: {}",
            program.source_path
        );
    }
    let checker = checker_program.as_ref().map_or_else(
        || standard_checker(descriptor.files.checker.as_ref(), &mut checker_losses),
        |program| CheckerSpec::Custom {
            source_path: program.source_path.clone(),
            language: program.language.clone(),
        },
    );
    losses.extend(checker_losses);
    let checker_tests = descriptor
        .files
        .checker
        .as_ref()
        .and_then(|program| program.testset.as_ref())
        .map(|testset| import_checker_tests(testset, &paths))
        .transpose()?
        .unwrap_or_default();

    let mut solution_losses = Vec::new();
    let solutions = descriptor
        .solutions
        .values
        .iter()
        .enumerate()
        .map(|(index, solution)| {
            let (path, language) = solution_source(solution)
                .with_context(|| format!("Polygon solution {} has no source", index + 1))?;
            ensure!(
                paths.contains(path.as_str()),
                "Polygon solution source is missing: {path}"
            );
            let name = if solution.name.is_empty() {
                Path::new(&path)
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("solution")
                    .to_owned()
            } else {
                solution.name.clone()
            };
            Ok(SolutionSpec {
                name,
                source_path: path,
                language,
                expected_verdict: solution_verdict(&solution.tag, &mut solution_losses),
                expected_score: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    losses.extend(solution_losses);

    let problem_type = if interactor.is_some() {
        ProblemType::Interactive
    } else if !groups.is_empty() {
        ProblemType::Scored
    } else {
        ProblemType::Standard
    };
    let harness = if problem_type == ProblemType::Interactive {
        interactive_harness(&solutions, interactor.as_ref().expect("checked"))
    } else {
        None
    };
    if problem_type == ProblemType::Interactive && harness.is_none() {
        losses.push(
            "Interactive harness could not be inferred because no accepted C++ solution was present"
                .into(),
        );
    }

    let mut titles = BTreeMap::new();
    for name in &descriptor.names.values {
        let locale = normalize_locale(&name.language);
        ensure!(!locale.is_empty(), "Polygon localized name has no language");
        ensure!(
            titles.insert(locale.clone(), name.value.clone()).is_none(),
            "Polygon package contains duplicate localized name {locale}"
        );
    }
    if titles.is_empty() {
        titles.insert(
            "en".into(),
            descriptor
                .name
                .clone()
                .unwrap_or_else(|| descriptor.short_name.clone()),
        );
    }
    let mut statements = BTreeMap::new();
    for statement in &descriptor.statements.values {
        let locale = normalize_locale(&statement.language);
        let path = normalize_relative_path(&statement.path)?;
        ensure!(
            paths.contains(path.as_str()),
            "Polygon statement source is missing: {path}"
        );
        ensure!(
            statements.insert(locale.clone(), path.clone()).is_none(),
            "Polygon package contains duplicate statement locale {locale}"
        );
        if Path::new(&path)
            .extension()
            .and_then(|value| value.to_str())
            != Some("md")
        {
            losses.push(format!(
                "Statement {locale} is preserved in its external source format and requires Markdown review"
            ));
        }
    }
    ensure!(
        !statements.is_empty(),
        "Polygon package contains no statements"
    );
    for locale in statements.keys() {
        titles.entry(locale.clone()).or_insert_with(|| {
            descriptor
                .name
                .clone()
                .unwrap_or_else(|| descriptor.short_name.clone())
        });
    }
    let default_locale = if statements.contains_key("en") {
        "en".to_owned()
    } else {
        statements.keys().next().expect("checked non-empty").clone()
    };
    let allowed_languages = solutions
        .iter()
        .map(|solution| solution.language.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let statement_sections = statements
        .keys()
        .map(|locale| {
            (
                locale.clone(),
                StatementSectionsV1 {
                    input_format: String::new(),
                    output_format: String::new(),
                    note: String::new(),
                },
            )
        })
        .collect();
    let revision = descriptor.revision.unwrap_or_default();
    let memory_mib = testset.memory_limit.saturating_add(1024 * 1024 - 1) / (1024 * 1024);
    let manifest = ReleaseManifestV1 {
        schema: RELEASE_MANIFEST_SCHEMA_V1.into(),
        project_id: Uuid::now_v7(),
        commit_id: Uuid::now_v7(),
        problem_type,
        package_profile: PackageProfile::PolygonCompatible,
        default_locale,
        title: titles,
        statements,
        files,
        toolchains: BTreeMap::new(),
        judging: JudgingSpec {
            limits: ResourceLimits {
                time_ms: testset.time_limit.max(1),
                memory_mib: memory_mib.max(1),
                output_kib: 64 * 1024,
            },
            checker,
            tests,
            groups,
            generators,
            validator_path: validator
                .as_ref()
                .map(|program| program.source_path.clone()),
            validator_language: validator.as_ref().map(|program| program.language.clone()),
            extra_validator_paths: vec![],
            extra_validators,
            validator_tests,
            checker_tests,
            interactor_path: interactor
                .as_ref()
                .map(|program| program.source_path.clone()),
            interactor_language: interactor.as_ref().map(|program| program.language.clone()),
            grader_path: None,
            grader_language: None,
            harness,
        },
        sources: vec![SourceAttribution {
            provider: "Codeforces Polygon package".into(),
            external_id: format!("{}@{revision}", descriptor.short_name),
            canonical_url: String::new(),
            license_name: "unknown".into(),
            attribution: String::new(),
        }],
        solutions,
        output_submissions: vec![],
        publication: Some(PublicationSpecV1 {
            category: "Algorithm".into(),
            difficulty: "Imported".into(),
            grading_category: "algorithmic".into(),
            tags: vec![],
            allowed_languages,
            statement_sections,
            samples,
        }),
        policy_version: "studio-policy-v1".into(),
    };
    let validation_issues = validate_manifest(&manifest);
    write_new(
        &directory.join("reporch.problem.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    write_new(
        &directory.join("reporch.import-report.json"),
        &serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "reporch.import-report.v1",
            "source_profile": "polygon_compatible",
            "target_profile": "reporch_native",
            "source_archive": input.file_name().and_then(|name| name.to_str()),
            "sidecar_present": false,
            "descriptor_sha256": Sha256Digest::from_bytes(&descriptor_bytes),
            "native_validation_passed": validation_issues.is_empty(),
            "native_validation_issues": validation_issues,
            "losses": losses,
            "notes": [
                "All external archive entries were preserved byte-for-byte",
                "Concrete generated tests require a linux or windows Polygon package; standard packages may be incomplete",
                "No Polygon credentials, API keys, or signatures are persisted in the import"
            ]
        }))?,
    )?;
    Ok(manifest)
}

fn manifest_file(directory: &Path, path: &str, executable: bool) -> Result<ManifestFile> {
    let bytes = fs::read(directory.join(path))?;
    Ok(ManifestFile {
        path: path.into(),
        sha256: Sha256Digest::from_bytes(&bytes),
        size_bytes: bytes.len() as u64,
        media_type: media_type(path).into(),
        executable,
    })
}

fn media_type(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("md") => "text/markdown",
        Some("tex") => "application/x-tex",
        Some("xml") => "application/xml",
        Some("json") => "application/json",
        Some("py") => "text/x-python",
        Some("rs") => "text/x-rust",
        Some("c" | "h" | "cc" | "cpp" | "cxx" | "hpp") => "text/x-c",
        Some("java" | "kt" | "go" | "txt" | "in" | "ans") => "text/plain",
        _ => "application/octet-stream",
    }
}

fn normalize_locale(language: &str) -> String {
    match language.trim().to_ascii_lowercase().as_str() {
        "english" => "en".into(),
        "russian" => "ru".into(),
        "korean" => "ko".into(),
        value => value.replace('_', "-"),
    }
}

fn expand_test_pattern(pattern: &str, index: usize) -> Result<String> {
    let percent = pattern
        .find('%')
        .context("Polygon test path pattern has no %d placeholder")?;
    ensure!(
        !pattern[percent + 1..].contains('%'),
        "Polygon test path pattern has multiple placeholders"
    );
    let suffix = &pattern[percent + 1..];
    let digits = suffix
        .strip_prefix('0')
        .unwrap_or(suffix)
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    let consumed = usize::from(suffix.starts_with('0')) + digits.len();
    ensure!(
        suffix.as_bytes().get(consumed) == Some(&b'd'),
        "Polygon test path pattern must use %d or %0Nd"
    );
    let width = if digits.is_empty() {
        0
    } else {
        digits.parse::<usize>()?
    };
    ensure!(width <= 12, "Polygon test path width is excessive");
    let formatted = if width == 0 {
        index.to_string()
    } else {
        format!("{index:0width$}")
    };
    let end = percent + 1 + consumed + 1;
    normalize_relative_path(&format!(
        "{}{}{}",
        &pattern[..percent],
        formatted,
        &pattern[end..]
    ))
    .context("unsafe Polygon test path pattern")
}

fn program_spec(program: &ExternalProgramXml, fallback_id: &str) -> Option<ProgramSpec> {
    let path = if !program.path.is_empty() {
        program.path.clone()
    } else {
        program.source.as_ref()?.path.clone()
    };
    if path.is_empty() {
        return None;
    }
    let declared_language = if !program.language.is_empty() {
        &program.language
    } else if !program.source_type.is_empty() {
        &program.source_type
    } else {
        program
            .source
            .as_ref()
            .map(|source| {
                if source.language.is_empty() {
                    source.source_type.as_str()
                } else {
                    source.language.as_str()
                }
            })
            .unwrap_or("")
    };
    Some(ProgramSpec {
        id: if program.name.is_empty() {
            fallback_id.into()
        } else {
            program.name.clone()
        },
        source_path: path.clone(),
        language: language_from_declared(declared_language, &path),
        arguments: vec![],
    })
}

fn solution_source(solution: &ExternalSolutionXml) -> Option<(String, String)> {
    let path = if !solution.path.is_empty() {
        solution.path.clone()
    } else {
        solution.source.as_ref()?.path.clone()
    };
    let declared = if !solution.language.is_empty() {
        solution.language.as_str()
    } else {
        solution
            .source
            .as_ref()
            .map(|source| {
                if source.language.is_empty() {
                    source.source_type.as_str()
                } else {
                    source.language.as_str()
                }
            })
            .unwrap_or("")
    };
    Some((path.clone(), language_from_declared(declared, &path)))
}

fn language_from_declared(declared: &str, path: &str) -> String {
    let value = declared.to_ascii_lowercase();
    if value.contains("cpp") || value.contains("g++") {
        "cpp20".into()
    } else if value.contains("python") || value.starts_with("py") {
        "python3".into()
    } else if value.contains("java") {
        "java17".into()
    } else if value.contains("kotlin") {
        "kotlin".into()
    } else if value.contains("rust") {
        "rust".into()
    } else if value == "c" || value.starts_with("c.") {
        "c17".into()
    } else {
        match Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
        {
            Some("cpp" | "cc" | "cxx") => "cpp20",
            Some("c") => "c17",
            Some("py") => "python3",
            Some("java") => "java17",
            Some("kt") => "kotlin",
            Some("rs") => "rust",
            Some("go") => "go",
            _ => "unknown",
        }
        .into()
    }
}

fn standard_checker(checker: Option<&ExternalProgramXml>, losses: &mut Vec<String>) -> CheckerSpec {
    let name = checker.map_or("", |checker| checker.name.as_str());
    let normalized = name.to_ascii_lowercase();
    if normalized.contains("case-insensitive") || normalized.contains("yesno") {
        CheckerSpec::CaseInsensitive
    } else if normalized.contains("rcmp") || normalized.contains("fcmp") {
        losses.push(format!(
            "Polygon floating checker {name:?} tolerance could not be recovered; token checking is selected until author review"
        ));
        CheckerSpec::Token
    } else if normalized.contains("exact") {
        CheckerSpec::Exact
    } else {
        if !normalized.is_empty()
            && !normalized.contains("wcmp")
            && !normalized.contains("token")
            && !normalized.contains("ncmp")
        {
            losses.push(format!(
                "Polygon standard checker {name:?} was conservatively mapped to token checking"
            ));
        }
        CheckerSpec::Token
    }
}

fn solution_verdict(tag: &str, losses: &mut Vec<String>) -> ExpectedVerdict {
    match tag.to_ascii_uppercase().as_str() {
        "MA" | "OK" => ExpectedVerdict::Accepted,
        "WA" | "PE" | "RJ" => ExpectedVerdict::WrongAnswer,
        "TL" => ExpectedVerdict::TimeLimit,
        "ML" => ExpectedVerdict::MemoryLimit,
        "RE" | "NR" => ExpectedVerdict::RuntimeError,
        "TO" | "TM" => {
            losses.push(format!(
                "Ambiguous Polygon solution tag {tag} was mapped to time_limit and requires author review"
            ));
            ExpectedVerdict::TimeLimit
        }
        value => {
            losses.push(format!(
                "Unknown Polygon solution tag {value:?} was mapped to wrong_answer"
            ));
            ExpectedVerdict::WrongAnswer
        }
    }
}

fn import_validator_tests(
    testset: &ExternalProgramTestsetXml,
    paths: &BTreeSet<&str>,
) -> Result<Vec<ValidatorTestSpec>> {
    let count = testset.test_count.max(testset.tests.values.len());
    (1..=count)
        .map(|index| {
            let input_file = expand_test_pattern(&testset.input_path_pattern, index)?;
            ensure!(
                paths.contains(input_file.as_str()),
                "Polygon validator test input is missing: {input_file}"
            );
            let verdict = testset
                .tests
                .values
                .get(index - 1)
                .map(|test| test.verdict.to_ascii_lowercase())
                .unwrap_or_default();
            ensure!(
                matches!(verdict.as_str(), "valid" | "invalid"),
                "Polygon validator test has unsupported verdict {verdict:?}"
            );
            Ok(ValidatorTestSpec {
                name: format!("polygon-validator-{index}"),
                input_file,
                expected_valid: verdict == "valid",
            })
        })
        .collect()
}

fn import_checker_tests(
    testset: &ExternalProgramTestsetXml,
    paths: &BTreeSet<&str>,
) -> Result<Vec<CheckerTestSpec>> {
    let count = testset.test_count.max(testset.tests.values.len());
    (1..=count)
        .map(|index| {
            let input_file = expand_test_pattern(&testset.input_path_pattern, index)?;
            let output_file = expand_test_pattern(&testset.output_path_pattern, index)?;
            let answer_file = expand_test_pattern(&testset.answer_path_pattern, index)?;
            for path in [&input_file, &output_file, &answer_file] {
                ensure!(
                    paths.contains(path.as_str()),
                    "Polygon checker test file is missing: {path}"
                );
            }
            let verdict = testset
                .tests
                .values
                .get(index - 1)
                .map(|test| test.verdict.to_ascii_lowercase())
                .unwrap_or_default();
            ensure!(
                matches!(
                    verdict.as_str(),
                    "ok" | "wrong-answer" | "presentation-error"
                ),
                "Polygon checker test has unsupported verdict {verdict:?}"
            );
            Ok(CheckerTestSpec {
                name: format!("polygon-checker-{index}"),
                input_file,
                answer_file,
                output_file,
                expected_accepted: verdict == "ok",
            })
        })
        .collect()
}

fn interactive_harness(
    solutions: &[SolutionSpec],
    interactor: &ProgramSpec,
) -> Option<ExecutionHarnessV1> {
    let solution = solutions.iter().find(|solution| {
        solution.expected_verdict == ExpectedVerdict::Accepted && solution.language == "cpp20"
    })?;
    Some(ExecutionHarnessV1::InteractiveStdio {
        profiles: BTreeMap::from([(
            solution.language.clone(),
            InteractiveStdioProfileV1 {
                source_path: solution.source_path.clone(),
                interactor_source_path: interactor.source_path.clone(),
                asset_paths: vec![solution.source_path.clone(), interactor.source_path.clone()],
                include_dirs: vec![],
                idle_timeout_ms: 2_000,
                transcript_limit_kib: 1_024,
                solver_compile_command: None,
                solver_run_command: None,
                interactor_compile_command: None,
                interactor_run_command: None,
            },
        )]),
        score_type: ScoreAggregation::AllOrNothing,
        score_scale: 1,
    })
}

fn scan_archive(archive: &mut ZipArchive<File>) -> Result<Vec<ScannedEntry>> {
    ensure!(
        archive.len() <= MAX_ARCHIVE_FILES,
        "archive exceeds the {MAX_ARCHIVE_FILES} entry limit"
    );
    let mut normalized_paths = BTreeSet::new();
    let mut total_size = 0_u64;
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let file = archive.by_index(index)?;
        ensure!(!file.encrypted(), "encrypted ZIP entries are not supported");
        let raw_name =
            std::str::from_utf8(file.name_raw()).context("ZIP entry name is not valid UTF-8")?;
        ensure!(!raw_name.contains('\\'), "ZIP entry uses a backslash path");
        let trimmed = raw_name.trim_end_matches('/');
        ensure!(!trimmed.is_empty(), "ZIP entry path is empty");
        let path = normalize_relative_path(trimmed)
            .with_context(|| format!("unsafe ZIP entry {raw_name:?}"))?;
        let collision_key = path.clone();
        ensure!(
            normalized_paths.insert(collision_key),
            "duplicate or Unicode-colliding ZIP entry {path}"
        );
        let directory = file.is_dir();
        let file_type = file.unix_mode().unwrap_or_default() & 0o170000;
        ensure!(
            file_type == 0 || file_type == 0o100000 || (directory && file_type == 0o040000),
            "ZIP entry is a symlink or special file: {path}"
        );
        ensure!(
            file.size() <= MAX_ENTRY_BYTES,
            "ZIP entry exceeds the per-file size limit: {path}"
        );
        if matches!(
            path.as_str(),
            "problem.xml" | POLYGON_SIDECAR_PATH | POLYGON_REPORT_PATH
        ) {
            ensure!(
                file.size() <= MAX_CONTROL_BYTES,
                "Polygon control file exceeds the 16 MiB limit: {path}"
            );
        }
        total_size = total_size
            .checked_add(file.size())
            .context("ZIP uncompressed size overflow")?;
        ensure!(
            total_size <= MAX_ARCHIVE_BYTES,
            "archive exceeds the 5 GiB uncompressed project limit"
        );
        entries.push(ScannedEntry {
            archive_name: raw_name.into(),
            path,
            size: file.size(),
            executable: file.unix_mode().is_some_and(|mode| mode & 0o111 != 0),
            directory,
        });
    }
    ensure!(
        entries
            .iter()
            .any(|entry| !entry.directory && entry.path == "problem.xml"),
        "Polygon package is missing problem.xml"
    );
    Ok(entries)
}

fn extract_archive(
    archive: &mut ZipArchive<File>,
    directory: &Path,
    entries: &[ScannedEntry],
) -> Result<()> {
    for entry in entries {
        let output = directory.join(&entry.path);
        if entry.directory {
            fs::create_dir_all(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut source = archive.by_name(&entry.archive_name)?;
        let mut target = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)
            .with_context(|| format!("create imported file {}", output.display()))?;
        let copied = std::io::copy(&mut source, &mut target)?;
        ensure!(
            copied == entry.size,
            "ZIP entry size changed while extracting"
        );
        target.sync_all()?;
        set_executable(&output, entry.executable)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
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

fn validate_problem_xml(bytes: &[u8]) -> Result<()> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut root_seen = false;
    let mut depth = 0_u64;
    loop {
        match reader.read_event().context("parse problem.xml")? {
            Event::Start(element) => {
                if !root_seen {
                    ensure!(
                        element.name().as_ref() == b"problem",
                        "problem.xml root must be <problem>"
                    );
                    let mut revision = false;
                    let mut short_name = false;
                    for attribute in element.attributes() {
                        let attribute = attribute.context("parse problem.xml root attribute")?;
                        revision |= attribute.key.as_ref() == b"revision";
                        short_name |= attribute.key.as_ref() == b"short-name";
                    }
                    ensure!(
                        revision && short_name,
                        "problem.xml root is missing revision or short-name"
                    );
                    root_seen = true;
                }
                depth = depth.checked_add(1).context("problem.xml depth overflow")?;
                ensure!(depth <= 128, "problem.xml exceeds the nesting limit");
            }
            Event::End(_) => {
                depth = depth
                    .checked_sub(1)
                    .context("problem.xml has an unmatched end tag")?
            }
            Event::DocType(_) => bail!("problem.xml document types are forbidden"),
            Event::Eof => break,
            _ => {}
        }
    }
    ensure!(root_seen && depth == 0, "problem.xml is incomplete");
    Ok(())
}

fn verify_file(path: &Path, expected_size: u64, expected_digest: &Sha256Digest) -> Result<()> {
    let mut file = File::open(path).with_context(|| format!("read {}", path.display()))?;
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
            .context("file size overflow")?;
        digest.update(&buffer[..read]);
    }
    ensure!(size == expected_size, "file size mismatch");
    ensure!(
        hex::encode(digest.finalize()) == expected_digest.as_str(),
        "file digest mismatch"
    );
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut output = OpenOptions::new().write(true).create_new(true).open(path)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    fn write_corpus_archive(corpus_name: &str, output: &Path) {
        fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) {
            let mut entries = fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    collect_files(&path, files);
                } else {
                    files.push(path);
                }
            }
        }

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../evals/corpus")
            .join(corpus_name);
        let mut files = Vec::new();
        collect_files(&root, &mut files);
        let mut archive = ZipWriter::new(File::create(output).unwrap());
        for path in files {
            let relative = path
                .strip_prefix(&root)
                .unwrap()
                .components()
                .map(|component| component.as_os_str().to_str().unwrap())
                .collect::<Vec<_>>()
                .join("/");
            archive
                .start_file(
                    relative,
                    SimpleFileOptions::DEFAULT
                        .compression_method(CompressionMethod::Deflated)
                        .unix_permissions(0o644),
                )
                .unwrap();
            archive.write_all(&fs::read(path).unwrap()).unwrap();
        }
        archive.finish().unwrap();
    }

    #[test]
    fn round_trips_the_exact_native_manifest() {
        let (temporary, manifest) = super::super::polygon_export::tests::fixture();
        let archive = temporary.path().join("polygon.zip");
        super::super::polygon_export::export_polygon_package(&manifest, temporary.path(), &archive)
            .unwrap();

        let imported =
            import_polygon_package(&archive, &temporary.path().join("imported")).unwrap();

        assert_eq!(imported.digest().unwrap(), manifest.digest().unwrap());
        assert!(validate_manifest(&imported).is_empty());
    }

    #[test]
    fn imports_a_sidecar_free_polygon_package_semantically() {
        let (temporary, manifest) = super::super::polygon_export::tests::fixture();
        let full = temporary.path().join("polygon-full.zip");
        super::super::polygon_export::export_polygon_package(&manifest, temporary.path(), &full)
            .unwrap();
        let external = temporary.path().join("polygon-external.zip");
        let mut source = ZipArchive::new(File::open(full).unwrap()).unwrap();
        let mut target = ZipWriter::new(File::create(&external).unwrap());
        for index in 0..source.len() {
            let mut entry = source.by_index(index).unwrap();
            if entry.name().starts_with("-reporch-polygon/")
                && !entry.name().starts_with("-reporch-polygon/tests/")
            {
                continue;
            }
            let name = entry.name().to_owned();
            target
                .start_file(
                    name,
                    SimpleFileOptions::DEFAULT
                        .compression_method(CompressionMethod::Deflated)
                        .unix_permissions(entry.unix_mode().unwrap_or(0o644)),
                )
                .unwrap();
            std::io::copy(&mut entry, &mut target).unwrap();
        }
        target.finish().unwrap();
        let destination = temporary.path().join("sidecar-free-import");

        let imported = import_polygon_package(&external, &destination).unwrap();

        assert_eq!(imported.package_profile, PackageProfile::PolygonCompatible);
        assert_eq!(
            imported.title,
            BTreeMap::from([("ko".into(), "Polygon Fixture".into())])
        );
        assert_eq!(imported.judging.tests.len(), 1);
        assert_eq!(imported.solutions.len(), 3);
        assert!(
            imported
                .solutions
                .iter()
                .any(|solution| solution.expected_verdict == ExpectedVerdict::Accepted)
        );
        let report: serde_json::Value = serde_json::from_slice(
            &fs::read(destination.join("reporch.import-report.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(report["sidecar_present"], false);
        assert!(
            report["losses"]
                .as_array()
                .is_some_and(|losses| !losses.is_empty())
        );
    }

    #[test]
    fn advanced_external_corpus_preserves_scoring_tools_tags_and_loss_report() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("advanced.zip");
        write_corpus_archive("polygon-external-advanced", &archive);
        let destination = temporary.path().join("imported");

        let imported = import_polygon_package(&archive, &destination).unwrap();

        assert_eq!(imported.problem_type, ProblemType::Scored);
        assert_eq!(imported.default_locale, "en");
        assert_eq!(
            imported.title.get("ko").map(String::as_str),
            Some("고급 폴리곤 코퍼스")
        );
        assert_eq!(imported.judging.limits.time_ms, 1_500);
        assert_eq!(imported.judging.limits.memory_mib, 256);
        assert_eq!(imported.judging.tests.len(), 3);
        assert_eq!(imported.judging.groups.len(), 2);
        assert_eq!(imported.judging.groups[1].depends_on, ["pretests"]);
        assert_eq!(imported.judging.generators.len(), 1);
        assert_eq!(imported.judging.extra_validators.len(), 1);
        assert_eq!(imported.judging.validator_tests.len(), 2);
        assert_eq!(imported.judging.checker_tests.len(), 2);
        assert!(matches!(
            imported.judging.checker,
            CheckerSpec::Custom { .. }
        ));
        for verdict in [
            ExpectedVerdict::Accepted,
            ExpectedVerdict::TimeLimit,
            ExpectedVerdict::MemoryLimit,
            ExpectedVerdict::RuntimeError,
        ] {
            assert!(
                imported
                    .solutions
                    .iter()
                    .any(|solution| solution.expected_verdict == verdict),
                "missing imported verdict {verdict:?}"
            );
        }
        let report: serde_json::Value = serde_json::from_slice(
            &fs::read(destination.join("reporch.import-report.json")).unwrap(),
        )
        .unwrap();
        let losses = report["losses"].as_array().unwrap();
        assert!(losses.iter().any(|loss| {
            loss.as_str()
                .is_some_and(|loss| loss.contains("generator command was flattened"))
        }));
        assert!(losses.iter().any(|loss| {
            loss.as_str()
                .is_some_and(|loss| loss.contains("requires Markdown review"))
        }));
    }

    #[test]
    fn interactive_external_corpus_infers_a_bounded_stdio_harness() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("interactive.zip");
        write_corpus_archive("polygon-external-interactive", &archive);

        let imported =
            import_polygon_package(&archive, &temporary.path().join("imported")).unwrap();

        assert_eq!(imported.problem_type, ProblemType::Interactive);
        assert_eq!(
            imported.judging.interactor_path.as_deref(),
            Some("interactors/interactor.cpp")
        );
        assert_eq!(imported.judging.validator_tests.len(), 2);
        assert_eq!(
            imported
                .solutions
                .iter()
                .filter(|solution| solution.expected_verdict == ExpectedVerdict::Accepted)
                .count(),
            2
        );
        let Some(ExecutionHarnessV1::InteractiveStdio { profiles, .. }) = imported.judging.harness
        else {
            panic!("interactive harness was not inferred");
        };
        let profile = profiles.get("cpp20").unwrap();
        assert_eq!(profile.idle_timeout_ms, 2_000);
        assert_eq!(profile.transcript_limit_kib, 1_024);
        assert_eq!(profile.interactor_source_path, "interactors/interactor.cpp");
    }

    #[test]
    fn rejects_traversal_before_creating_a_destination() {
        let temporary = tempfile::tempdir().unwrap();
        let archive_path = temporary.path().join("bad.zip");
        let mut archive = ZipWriter::new(File::create(&archive_path).unwrap());
        archive
            .start_file(
                "../../escape",
                SimpleFileOptions::DEFAULT.compression_method(CompressionMethod::Stored),
            )
            .unwrap();
        archive.write_all(b"bad").unwrap();
        archive.finish().unwrap();
        let destination = temporary.path().join("imported");

        let error = import_polygon_package(&archive_path, &destination).unwrap_err();

        assert!(error.to_string().contains("unsafe ZIP entry"));
        assert!(!destination.exists());
        assert!(!temporary.path().join("escape").exists());
    }

    #[test]
    fn rejects_xml_document_types() {
        let xml = br#"<?xml version="1.0"?><!DOCTYPE problem SYSTEM "file:///etc/passwd"><problem revision="1" short-name="x"></problem>"#;
        let error = validate_problem_xml(xml).unwrap_err();
        assert!(error.to_string().contains("document types are forbidden"));
    }

    #[test]
    fn rejects_native_file_tampering_and_removes_partial_import() {
        let (temporary, manifest) = super::super::polygon_export::tests::fixture();
        let original = temporary.path().join("polygon.zip");
        super::super::polygon_export::export_polygon_package(
            &manifest,
            temporary.path(),
            &original,
        )
        .unwrap();
        let tampered = temporary.path().join("tampered.zip");
        let mut source = ZipArchive::new(File::open(original).unwrap()).unwrap();
        let mut target = ZipWriter::new(File::create(&tampered).unwrap());
        for index in 0..source.len() {
            let mut entry = source.by_index(index).unwrap();
            let name = entry.name().to_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if name == "solutions/accepted.py" {
                bytes[0] ^= 1;
            }
            target
                .start_file(
                    name,
                    SimpleFileOptions::DEFAULT
                        .compression_method(CompressionMethod::Stored)
                        .unix_permissions(entry.unix_mode().unwrap_or(0o644)),
                )
                .unwrap();
            target.write_all(&bytes).unwrap();
        }
        target.finish().unwrap();
        let destination = temporary.path().join("tampered-import");

        let error = import_polygon_package(&tampered, &destination).unwrap_err();

        assert!(error.to_string().contains("verify imported native file"));
        assert!(!destination.exists());
    }
}
