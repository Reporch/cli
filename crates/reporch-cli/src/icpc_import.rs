use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use studio_core::{
    CheckerSpec, CheckerTestSpec, ExecutionHarnessV1, ExpectedScoreRange, ExpectedVerdict,
    InteractiveStdioProfileV1, JudgingSpec, ManifestFile, OutputSubmissionSpec, PackageProfile,
    ProblemType, ProgramSpec, PublicationSampleV1, PublicationSpecV1, RELEASE_MANIFEST_SCHEMA_V1,
    ReleaseManifestV1, ResourceLimits, ScoreAggregation, Sha256Digest, SolutionSpec,
    SourceAttribution, StatementSectionsV1, TestCaseSpec, TestGroupSpec, ValidatorTestSpec,
    normalize_relative_path, validate_manifest,
};
use uuid::Uuid;
use zip::ZipArchive;

use crate::icpc_submit_answer::{SIDECAR_PATH, SIDECAR_SCHEMA_V1, SubmitAnswerSidecarV1};

const MAX_ARCHIVE_FILES: usize = 50_000;
const MAX_ARCHIVE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct ScannedEntry {
    pub(crate) archive_name: String,
    pub(crate) relative_path: Option<String>,
    pub(crate) size: u64,
    pub(crate) executable: bool,
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

#[derive(Debug, Deserialize)]
struct ProblemYaml {
    problem_format_version: String,
    #[serde(rename = "type", default)]
    kind: StringOrStrings,
    name: StringOrMap,
    uuid: Uuid,
    #[serde(default)]
    source: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    rights_owner: Option<String>,
    #[serde(default)]
    limits: LimitsYaml,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    languages: StringOrStrings,
}

#[derive(Debug, Default, Deserialize)]
#[serde(untagged)]
enum StringOrStrings {
    One(String),
    Many(Vec<String>),
    #[default]
    Missing,
}

impl StringOrStrings {
    fn values(&self) -> Vec<String> {
        match self {
            // The 2025-09 reference submit-answer fixture uses the legacy
            // whitespace-separated scalar spelling even though the normative
            // form is a YAML sequence. Accept both spellings.
            Self::One(value) => value.split_whitespace().map(str::to_owned).collect(),
            Self::Many(values) => values.clone(),
            Self::Missing => Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StringOrMap {
    One(String),
    Localized(BTreeMap<String, String>),
}

impl StringOrMap {
    fn titles(&self) -> BTreeMap<String, String> {
        match self {
            Self::One(value) => BTreeMap::from([("en".into(), value.clone())]),
            Self::Localized(values) => values.clone(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct LimitsYaml {
    #[serde(default)]
    time_limit: Option<f64>,
    #[serde(default)]
    memory: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct TestGroupYaml {
    #[serde(default)]
    max_score: Option<ScoreMaximum>,
    #[serde(default)]
    require_pass: StringOrStrings,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScoreMaximum {
    Integer(u64),
    Named(String),
}

#[derive(Debug, Default, Deserialize)]
struct SubmissionMetadata {
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    score: Option<ScoreConstraint>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScoreConstraint {
    Exact(f64),
    Range([f64; 2]),
}

impl ScoreConstraint {
    fn range(&self) -> ExpectedScoreRange {
        match self {
            Self::Exact(score) => ExpectedScoreRange {
                minimum: *score,
                maximum: *score,
            },
            Self::Range([minimum, maximum]) => ExpectedScoreRange {
                minimum: *minimum,
                maximum: *maximum,
            },
        }
    }
}

pub fn import_icpc_2025_09(input: &Path, directory: &Path) -> Result<ReleaseManifestV1> {
    import_icpc_based(input, directory, PackageProfile::Icpc202509)
}

pub fn import_domjudge_zip(input: &Path, directory: &Path) -> Result<ReleaseManifestV1> {
    import_icpc_based(input, directory, PackageProfile::DomjudgeZip)
}

fn import_icpc_based(
    input: &Path,
    directory: &Path,
    source_profile: PackageProfile,
) -> Result<ReleaseManifestV1> {
    let input_file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut archive = ZipArchive::new(input_file).context("read ICPC ZIP central directory")?;
    let (root, entries) = scan_archive(&mut archive)?;
    let destination = CreatedDirectory::create(directory)?;
    extract_archive(&mut archive, directory, &root, &entries)?;

    let problem: ProblemYaml = read_yaml(&directory.join("problem.yaml"))?;
    ensure!(
        problem.problem_format_version == "2025-09",
        "expected ICPC problem_format_version 2025-09"
    );
    let manifest = build_manifest(directory, &entries, &problem, source_profile)?;
    let validation_issues = validate_manifest(&manifest);
    write_new(
        &directory.join("reporch.problem.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    write_new(
        &directory.join("reporch.import-report.json"),
        &serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "reporch.import-report.v1",
            "source_profile": source_profile,
            "target_profile": "reporch_native",
            "source_archive": input.file_name().and_then(|name| name.to_str()),
            "package_root": root,
            "native_validation_passed": validation_issues.is_empty(),
            "native_validation_issues": validation_issues,
            "notes": [
                "ICPC configuration files and auxiliary entries were preserved byte-for-byte",
                "external generator arguments and advanced validator directory programs may require author review",
                "submit-answer test/output identity is restored from the Reporch sidecar when present; otherwise directory outputs are mapped in deterministic lexical order"
            ]
        }))?,
    )?;
    destination.finish();
    Ok(manifest)
}

pub(crate) fn scan_archive(archive: &mut ZipArchive<File>) -> Result<(String, Vec<ScannedEntry>)> {
    ensure!(
        archive.len() <= MAX_ARCHIVE_FILES,
        "archive exceeds the {MAX_ARCHIVE_FILES} entry limit"
    );
    let mut total_size = 0_u64;
    let mut roots = BTreeSet::new();
    let mut normalized_paths = BTreeSet::new();
    let mut entries = Vec::with_capacity(archive.len());

    for index in 0..archive.len() {
        let file = archive.by_index(index)?;
        ensure!(!file.encrypted(), "encrypted ZIP entries are not supported");
        let raw_name =
            std::str::from_utf8(file.name_raw()).context("ZIP entry name is not valid UTF-8")?;
        ensure!(!raw_name.contains('\\'), "ZIP entry uses a backslash path");
        let trimmed = raw_name.trim_end_matches('/');
        ensure!(!trimmed.is_empty(), "ZIP entry path is empty");
        let normalized = normalize_relative_path(trimmed)
            .with_context(|| format!("unsafe ZIP entry {raw_name:?}"))?;
        ensure!(
            normalized_paths.insert(normalized.clone()),
            "duplicate or Unicode-colliding ZIP entry {normalized}"
        );
        let mut components = normalized.split('/');
        let root = components.next().expect("validated non-empty path");
        roots.insert(root.to_owned());
        let remainder = components.collect::<Vec<_>>().join("/");
        let is_dir = file.is_dir();
        let file_type = file.unix_mode().unwrap_or_default() & 0o170000;
        ensure!(
            file_type == 0 || file_type == 0o100000 || (is_dir && file_type == 0o040000),
            "ZIP entry is a symlink or special file: {normalized}"
        );
        ensure!(
            is_dir || !remainder.is_empty(),
            "files must be inside one package root directory"
        );
        ensure!(
            file.size() <= MAX_ENTRY_BYTES,
            "ZIP entry exceeds the per-file size limit: {normalized}"
        );
        total_size = total_size
            .checked_add(file.size())
            .context("ZIP uncompressed size overflow")?;
        ensure!(
            total_size <= MAX_ARCHIVE_BYTES,
            "archive exceeds the 5 GiB uncompressed project limit"
        );
        entries.push(ScannedEntry {
            archive_name: raw_name.to_owned(),
            relative_path: (!remainder.is_empty()).then_some(remainder),
            size: file.size(),
            executable: file.unix_mode().is_some_and(|mode| mode & 0o111 != 0),
        });
    }
    ensure!(
        roots.len() == 1,
        "ICPC ZIP must contain exactly one package root"
    );
    let root = roots.into_iter().next().context("ICPC ZIP is empty")?;
    ensure!(
        entries
            .iter()
            .any(|entry| entry.relative_path.as_deref() == Some("problem.yaml")),
        "package root does not contain problem.yaml"
    );
    Ok((root, entries))
}

pub(crate) fn extract_archive(
    archive: &mut ZipArchive<File>,
    directory: &Path,
    _root: &str,
    entries: &[ScannedEntry],
) -> Result<()> {
    for (index, scanned) in entries.iter().enumerate() {
        let Some(relative_path) = scanned.relative_path.as_deref() else {
            continue;
        };
        let mut file = archive.by_index(index)?;
        ensure!(
            file.name_raw() == scanned.archive_name.as_bytes(),
            "ZIP central directory changed during extraction"
        );
        if file.is_dir() {
            fs::create_dir_all(directory.join(relative_path))?;
            continue;
        }
        let target = directory.join(relative_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .with_context(|| format!("create extracted file {}", target.display()))?;
        let copied = std::io::copy(&mut file, &mut output)?;
        ensure!(
            copied == scanned.size,
            "ZIP entry size changed while extracting"
        );
        output.sync_all()?;
    }
    Ok(())
}

fn build_manifest(
    directory: &Path,
    entries: &[ScannedEntry],
    problem: &ProblemYaml,
    package_profile: PackageProfile,
) -> Result<ReleaseManifestV1> {
    let file_entries = entries
        .iter()
        .filter_map(|entry| entry.relative_path.as_deref().map(|path| (entry, path)))
        .filter(|(_, path)| directory.join(path).is_file())
        .collect::<Vec<_>>();
    let paths = file_entries
        .iter()
        .map(|(_, path)| *path)
        .collect::<BTreeSet<_>>();
    let files = file_entries
        .iter()
        .map(|(entry, path)| manifest_file(directory, path, entry.executable))
        .collect::<Result<Vec<_>>>()?;

    let titles = problem.name.titles();
    ensure!(!titles.is_empty(), "problem name map is empty");
    let mut statements = BTreeMap::new();
    for locale in titles.keys() {
        let path = format!("statement/problem.{locale}.md");
        ensure!(
            paths.contains(path.as_str()),
            "the initial importer requires a Markdown statement for locale {locale}"
        );
        statements.insert(locale.clone(), path);
    }
    let default_locale = if titles.contains_key("en") {
        "en".to_owned()
    } else {
        titles.keys().next().expect("checked non-empty").clone()
    };

    let kinds = problem.kind.values();
    let submit_answer = kinds.iter().any(|kind| kind == "submit-answer");
    ensure!(
        !(submit_answer && kinds.iter().any(|kind| kind == "scoring")),
        "scored submit-answer import is not enabled until native score aggregation is explicit"
    );
    let problem_type = if submit_answer {
        ProblemType::OutputOnly
    } else if kinds.iter().any(|kind| kind == "interactive") {
        ProblemType::Interactive
    } else if kinds.iter().any(|kind| kind == "scoring") {
        ProblemType::Scored
    } else {
        ProblemType::Standard
    };
    ensure!(
        matches!(
            problem_type,
            ProblemType::Standard
                | ProblemType::Scored
                | ProblemType::Interactive
                | ProblemType::OutputOnly
        ),
        "the ICPC importer supports pass-fail, scoring, interactive, and submit-answer packages"
    );
    if problem_type == ProblemType::OutputOnly {
        ensure!(
            !paths
                .iter()
                .any(|path| path.starts_with("static_validator/")),
            "ICPC submit-answer packages must not provide a static validator"
        );
    }

    let submit_answer_sidecar =
        if problem_type == ProblemType::OutputOnly && paths.contains(SIDECAR_PATH) {
            let sidecar: SubmitAnswerSidecarV1 = read_json(&directory.join(SIDECAR_PATH))?;
            ensure!(
                sidecar.schema == SIDECAR_SCHEMA_V1,
                "unsupported submit-answer sidecar schema"
            );
            Some(sidecar)
        } else {
            None
        };

    let groups = import_groups(directory, &paths, problem_type)?;
    let tests = import_tests(
        &paths,
        &groups,
        problem_type,
        submit_answer_sidecar.as_ref(),
    )?;
    ensure!(!tests.is_empty(), "ICPC package has no secret input tests");
    let solutions = if problem_type == ProblemType::OutputOnly {
        vec![]
    } else {
        import_solutions(directory, &paths, problem_type)?
    };
    let output_submissions = if problem_type == ProblemType::OutputOnly {
        import_output_submissions(directory, &paths, &tests, submit_answer_sidecar.as_ref())?
    } else {
        vec![]
    };
    let validator_programs = program_files(&paths, "input_validators/");
    let validator = validator_programs.first().cloned();
    let extra_validators = validator_programs.into_iter().skip(1).collect();
    let generators = program_files(&paths, "generators/");
    let checker_program = program_files(&paths, "output_validator/")
        .into_iter()
        .next();
    let checker = if problem_type == ProblemType::Interactive {
        CheckerSpec::Token
    } else {
        checker_program
            .as_ref()
            .map_or(CheckerSpec::Token, |program| CheckerSpec::Custom {
                source_path: program.source_path.clone(),
                language: program.language.clone(),
            })
    };
    let validator_tests = import_validator_tests(&paths, &tests, validator.is_some());
    let checker_tests = import_checker_tests(
        &paths,
        checker_program.is_some() && problem_type != ProblemType::Interactive,
    )?;
    let interactor = if problem_type == ProblemType::Interactive {
        Some(
            checker_program
                .clone()
                .context("interactive package has no output validator/interactor")?,
        )
    } else {
        None
    };
    let harness = interactor
        .as_ref()
        .map(|program| interactive_harness(&solutions, program))
        .transpose()?;
    let publication = publication(problem, &paths, &titles)?;

    let time_seconds = problem.limits.time_limit.unwrap_or(1.0);
    ensure!(
        time_seconds.is_finite() && time_seconds > 0.0,
        "invalid ICPC time limit"
    );
    let time_ms = (time_seconds * 1_000.0).round();
    ensure!(time_ms <= u64::MAX as f64, "ICPC time limit is too large");

    Ok(ReleaseManifestV1 {
        schema: RELEASE_MANIFEST_SCHEMA_V1.into(),
        project_id: problem.uuid,
        commit_id: Uuid::now_v7(),
        problem_type,
        package_profile,
        default_locale,
        title: titles,
        statements,
        files,
        toolchains: BTreeMap::new(),
        judging: JudgingSpec {
            limits: ResourceLimits {
                time_ms: time_ms as u64,
                memory_mib: problem.limits.memory.unwrap_or(1024),
                output_kib: problem.limits.output.unwrap_or(64).saturating_mul(1024),
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
        sources: source_attribution(problem),
        solutions,
        output_submissions,
        publication: Some(publication),
        policy_version: "studio-policy-v1".into(),
    })
}

fn import_validator_tests(
    paths: &BTreeSet<&str>,
    tests: &[TestCaseSpec],
    has_validator: bool,
) -> Vec<ValidatorTestSpec> {
    if !has_validator {
        return vec![];
    }
    let mut imported = tests.first().map_or_else(Vec::new, |test| {
        vec![ValidatorTestSpec {
            name: "imported-valid-secret".into(),
            input_file: test.input_file.clone(),
            expected_valid: true,
        }]
    });
    imported.extend(paths.iter().filter_map(|path| {
        let name = path
            .strip_prefix("data/invalid_input/")?
            .strip_suffix(".in")?;
        Some(ValidatorTestSpec {
            name: format!("imported-invalid-{name}"),
            input_file: (*path).into(),
            expected_valid: false,
        })
    }));
    imported
}

fn import_checker_tests(
    paths: &BTreeSet<&str>,
    has_custom_checker: bool,
) -> Result<Vec<CheckerTestSpec>> {
    if !has_custom_checker {
        return Ok(vec![]);
    }
    let mut imported = Vec::new();
    for (directory, expected_accepted) in [("valid_output", true), ("invalid_output", false)] {
        let prefix = format!("data/{directory}/");
        for path in paths {
            let Some(name) = path
                .strip_prefix(&prefix)
                .and_then(|path| path.strip_suffix(".in"))
            else {
                continue;
            };
            let answer_file = format!("{prefix}{name}.ans");
            let output_file = format!("{prefix}{name}.out");
            ensure!(
                paths.contains(answer_file.as_str()) && paths.contains(output_file.as_str()),
                "checker validation case {directory}/{name} is incomplete"
            );
            imported.push(CheckerTestSpec {
                name: format!("imported-{directory}-{name}"),
                input_file: (*path).into(),
                answer_file,
                output_file,
                expected_accepted,
            });
        }
    }
    Ok(imported)
}

fn import_groups(
    directory: &Path,
    paths: &BTreeSet<&str>,
    problem_type: ProblemType,
) -> Result<Vec<TestGroupSpec>> {
    if problem_type != ProblemType::Scored {
        return Ok(Vec::new());
    }
    let mut groups = Vec::new();
    for path in paths {
        let Some(group) = path
            .strip_prefix("data/secret/")
            .and_then(|path| path.strip_suffix("/test_group.yaml"))
        else {
            continue;
        };
        let config: TestGroupYaml = read_yaml(&directory.join(path))?;
        let points = match config.max_score {
            Some(ScoreMaximum::Integer(value)) => value as f64,
            Some(ScoreMaximum::Named(value)) => {
                bail!("scoring group {group} has unsupported max_score {value:?}")
            }
            None => bail!("scoring group {group} must declare max_score"),
        };
        let depends_on = config
            .require_pass
            .values()
            .into_iter()
            .filter(|dependency| dependency != "sample")
            .map(|dependency| {
                dependency
                    .strip_prefix("secret/")
                    .unwrap_or(&dependency)
                    .to_owned()
            })
            .collect();
        groups.push(TestGroupSpec {
            id: group.into(),
            points,
            depends_on,
            feedback_policy: studio_core::GroupFeedbackPolicyV1::Complete,
        });
    }
    ensure!(
        !groups.is_empty(),
        "scoring import requires explicit test groups"
    );
    let total = groups.iter().map(|group| group.points).sum::<f64>();
    ensure!(
        (total - 100.0).abs() <= f64::EPSILON,
        "Reporch scoring groups must total 100; imported package totals {total}"
    );
    groups.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(groups)
}

fn import_tests(
    paths: &BTreeSet<&str>,
    groups: &[TestGroupSpec],
    problem_type: ProblemType,
    submit_answer_sidecar: Option<&SubmitAnswerSidecarV1>,
) -> Result<Vec<TestCaseSpec>> {
    let sidecar_tests = submit_answer_sidecar
        .map(|sidecar| {
            let mut by_input = BTreeMap::new();
            let mut ids = BTreeSet::new();
            let mut indexes = BTreeSet::new();
            for test in &sidecar.tests {
                ensure!(
                    normalize_relative_path(&test.input_path)? == test.input_path,
                    "submit-answer sidecar contains an unsafe input path"
                );
                ensure!(
                    by_input.insert(test.input_path.as_str(), test).is_none()
                        && ids.insert(test.test_id)
                        && indexes.insert(test.test_index),
                    "submit-answer sidecar contains duplicate test identity"
                );
            }
            Ok::<_, anyhow::Error>(by_input)
        })
        .transpose()?;
    let mut tests = Vec::new();
    for path in paths {
        let Some(name) = path
            .strip_prefix("data/secret/")
            .and_then(|path| path.strip_suffix(".in"))
        else {
            continue;
        };
        let answer_path = format!("data/secret/{name}.ans");
        let answer = paths.contains(answer_path.as_str()).then_some(answer_path);
        ensure!(
            answer.is_some() || problem_type == ProblemType::Interactive,
            "test {name} has no answer file"
        );
        let test_groups = if problem_type == ProblemType::Scored {
            let matches = groups
                .iter()
                .filter(|group| name == group.id || name.starts_with(&format!("{}/", group.id)))
                .map(|group| group.id.clone())
                .collect::<Vec<_>>();
            ensure!(
                matches.len() == 1,
                "test {name} must belong to exactly one group"
            );
            matches
        } else {
            vec![]
        };
        let sidecar_test = sidecar_tests
            .as_ref()
            .and_then(|tests| tests.get(*path).copied());
        if let Some(sidecar_test) = sidecar_test {
            ensure!(
                sidecar_test.test_index == tests.len() + 1,
                "submit-answer sidecar test order does not match the package"
            );
        }
        tests.push(TestCaseSpec {
            id: sidecar_test.map_or_else(Uuid::now_v7, |test| test.test_id),
            name: sidecar_test.map_or_else(|| name.into(), |test| test.test_name.clone()),
            input_file: (*path).into(),
            answer_file: answer,
            groups: test_groups,
            generated_by: None,
            generator_arguments: vec![],
            seed: None,
        });
    }
    if let Some(sidecar) = submit_answer_sidecar {
        ensure!(
            tests.len() == sidecar.tests.len()
                && tests
                    .iter()
                    .all(|test| sidecar.tests.iter().any(|entry| entry.test_id == test.id)),
            "submit-answer sidecar test set does not match the package"
        );
    }
    Ok(tests)
}

fn interactive_harness(
    solutions: &[SolutionSpec],
    interactor: &ProgramSpec,
) -> Result<ExecutionHarnessV1> {
    let solution = solutions
        .iter()
        .find(|solution| {
            solution.expected_verdict == ExpectedVerdict::Accepted
                && matches!(
                    solution.language.to_ascii_lowercase().as_str(),
                    "c++" | "c++17" | "c++20" | "cpp" | "cpp17" | "cpp20"
                )
        })
        .context("interactive import requires an accepted C++ reference solution")?;
    Ok(ExecutionHarnessV1::InteractiveStdio {
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
        score_scale: 100,
    })
}

fn import_output_submissions(
    directory: &Path,
    paths: &BTreeSet<&str>,
    tests: &[TestCaseSpec],
    sidecar: Option<&SubmitAnswerSidecarV1>,
) -> Result<Vec<OutputSubmissionSpec>> {
    let metadata = read_submission_metadata(directory, paths)?;
    if let Some(sidecar) = sidecar {
        return import_sidecar_output_submissions(directory, paths, tests, sidecar, &metadata);
    }
    import_ordered_output_submissions(paths, tests, &metadata)
}

fn import_sidecar_output_submissions(
    directory: &Path,
    paths: &BTreeSet<&str>,
    tests: &[TestCaseSpec],
    sidecar: &SubmitAnswerSidecarV1,
    metadata: &BTreeMap<String, SubmissionMetadata>,
) -> Result<Vec<OutputSubmissionSpec>> {
    let tests_by_id = tests
        .iter()
        .map(|test| (test.id, test))
        .collect::<BTreeMap<_, _>>();
    let mut names = BTreeSet::new();
    let mut package_paths = BTreeSet::new();
    let mut imported = Vec::with_capacity(sidecar.submissions.len());
    for submission in &sidecar.submissions {
        ensure!(
            !submission.name.trim().is_empty() && names.insert(submission.name.as_str()),
            "submit-answer sidecar contains an empty or duplicate submission name"
        );
        ensure!(
            normalize_relative_path(&submission.package_path)? == submission.package_path
                && package_paths.insert(submission.package_path.as_str()),
            "submit-answer sidecar contains an unsafe or duplicate package path"
        );
        ensure!(
            submission.package_path.starts_with(&format!(
                "{}/",
                verdict_submission_directory(submission.expected_verdict)
            )),
            "submit-answer sidecar verdict does not match its submission directory"
        );
        ensure!(
            submission.outputs.len() == tests.len(),
            "submit-answer sidecar submission does not cover every test"
        );
        if let Some(entry) = matching_submission_metadata(metadata, &submission.package_path) {
            ensure!(
                entry.score.as_ref().map(ScoreConstraint::range) == submission.expected_score,
                "submit-answer score metadata disagrees with the Reporch sidecar"
            );
        }

        let prefix = format!("submissions/{}/", submission.package_path);
        let package_files = paths
            .iter()
            .filter(|path| path.starts_with(&prefix))
            .copied()
            .collect::<BTreeSet<_>>();
        let mut output_paths = BTreeSet::new();
        let mut outputs = BTreeMap::new();
        for output in &submission.outputs {
            let test = tests_by_id
                .get(&output.test_id)
                .context("submit-answer output references an unknown test")?;
            ensure!(
                output.test_index > 0
                    && tests.get(output.test_index - 1).map(|test| test.id) == Some(output.test_id),
                "submit-answer output test index does not match its test identity"
            );
            ensure!(
                normalize_relative_path(&output.path)? == output.path
                    && output.path.starts_with(&prefix)
                    && output_paths.insert(output.path.as_str())
                    && paths.contains(output.path.as_str()),
                "submit-answer sidecar contains an unsafe, duplicate, or missing output path"
            );
            ensure!(
                normalize_relative_path(&output.source_path)? == output.source_path,
                "submit-answer sidecar contains an unsafe native source path"
            );
            let bytes = fs::read(directory.join(&output.path))?;
            ensure!(
                Sha256Digest::from_bytes(&bytes).as_str() == output.sha256,
                "submit-answer output digest does not match the sidecar"
            );
            ensure!(
                outputs.insert(test.id, output.path.clone()).is_none(),
                "submit-answer sidecar maps a test more than once"
            );
        }
        ensure!(
            package_files == output_paths,
            "submit-answer package contains output files not declared by the sidecar"
        );
        imported.push(OutputSubmissionSpec {
            name: submission.name.clone(),
            outputs,
            expected_verdict: submission.expected_verdict,
            expected_score: submission.expected_score.clone(),
        });
    }
    Ok(imported)
}

fn import_ordered_output_submissions(
    paths: &BTreeSet<&str>,
    tests: &[TestCaseSpec],
    metadata: &BTreeMap<String, SubmissionMetadata>,
) -> Result<Vec<OutputSubmissionSpec>> {
    let mut grouped = BTreeMap::<String, Vec<String>>::new();
    for path in paths {
        let Some(relative) = path.strip_prefix("submissions/") else {
            continue;
        };
        if relative == "submissions.yaml" {
            continue;
        }
        let components = relative.split('/').collect::<Vec<_>>();
        if components.len() < 2 || submission_verdict(components[0], None).is_none() {
            continue;
        }
        let package_path = if components.len() == 2 {
            ensure!(
                tests.len() == 1,
                "a submit-answer package with multiple tests must use one directory per example submission or include the Reporch sidecar"
            );
            relative.to_owned()
        } else {
            format!("{}/{}", components[0], components[1])
        };
        grouped
            .entry(package_path)
            .or_default()
            .push((*path).into());
    }

    let mut names = BTreeSet::new();
    let mut imported = Vec::with_capacity(grouped.len());
    for (package_path, mut output_paths) in grouped {
        output_paths.sort();
        ensure!(
            output_paths.len() == tests.len(),
            "submit-answer submission {package_path} must contain exactly one output per test"
        );
        let directory = package_path
            .split('/')
            .next()
            .context("submit-answer submission path has no verdict directory")?;
        let entry_metadata = matching_submission_metadata(metadata, &package_path);
        let expected_score = entry_metadata
            .and_then(|entry| entry.score.as_ref())
            .map(ScoreConstraint::range);
        ensure!(
            expected_score.is_none(),
            "scored submit-answer import is not enabled until native score aggregation is explicit"
        );
        let expected_verdict = submission_verdict(directory, expected_score.as_ref())
            .context("unknown submit-answer verdict directory")?;
        let name = package_path
            .rsplit('/')
            .next()
            .context("submit-answer submission name is missing")?
            .to_owned();
        ensure!(
            names.insert(name.clone()),
            "submit-answer submission names must be unique"
        );
        let outputs = tests
            .iter()
            .zip(output_paths)
            .map(|(test, path)| (test.id, path))
            .collect();
        imported.push(OutputSubmissionSpec {
            name,
            outputs,
            expected_verdict,
            expected_score,
        });
    }
    Ok(imported)
}

fn read_submission_metadata(
    directory: &Path,
    paths: &BTreeSet<&str>,
) -> Result<BTreeMap<String, SubmissionMetadata>> {
    if paths.contains("submissions/submissions.yaml") {
        read_yaml(&directory.join("submissions/submissions.yaml"))
    } else {
        Ok(BTreeMap::new())
    }
}

fn submission_verdict(
    directory: &str,
    expected_score: Option<&ExpectedScoreRange>,
) -> Option<ExpectedVerdict> {
    match directory {
        "accepted" => Some(ExpectedVerdict::Accepted),
        "time_limit_exceeded" | "brute_force" => Some(ExpectedVerdict::TimeLimit),
        "run_time_error" => Some(ExpectedVerdict::RuntimeError),
        "wrong_answer" => Some(ExpectedVerdict::WrongAnswer),
        "rejected" if expected_score.is_some() => Some(ExpectedVerdict::Partial),
        "rejected" => Some(ExpectedVerdict::WrongAnswer),
        _ => None,
    }
}

fn verdict_submission_directory(verdict: ExpectedVerdict) -> &'static str {
    match verdict {
        ExpectedVerdict::Accepted => "accepted",
        ExpectedVerdict::WrongAnswer => "wrong_answer",
        ExpectedVerdict::TimeLimit => "time_limit_exceeded",
        ExpectedVerdict::RuntimeError => "run_time_error",
        ExpectedVerdict::MemoryLimit | ExpectedVerdict::Partial => "rejected",
    }
}

fn import_solutions(
    directory: &Path,
    paths: &BTreeSet<&str>,
    problem_type: ProblemType,
) -> Result<Vec<SolutionSpec>> {
    let metadata = read_submission_metadata(directory, paths)?;
    let mut solutions = Vec::new();
    for path in paths {
        let Some(relative) = path.strip_prefix("submissions/") else {
            continue;
        };
        if relative == "submissions.yaml" || !directory.join(path).is_file() {
            continue;
        }
        let Some((directory_name, filename)) = relative.split_once('/') else {
            continue;
        };
        ensure!(
            !filename.contains('/'),
            "directory submissions require manual import"
        );
        let entry_metadata = matching_submission_metadata(&metadata, relative);
        let expected_score = entry_metadata
            .and_then(|entry| entry.score.as_ref())
            .map(|s| s.range());
        let expected_verdict = match directory_name {
            "accepted" => ExpectedVerdict::Accepted,
            "time_limit_exceeded" | "brute_force" => ExpectedVerdict::TimeLimit,
            "run_time_error" => ExpectedVerdict::RuntimeError,
            "wrong_answer" => ExpectedVerdict::WrongAnswer,
            "rejected" if problem_type == ProblemType::Scored && expected_score.is_some() => {
                ExpectedVerdict::Partial
            }
            "rejected" => ExpectedVerdict::WrongAnswer,
            _ => continue,
        };
        let language = entry_metadata
            .and_then(|entry| entry.language.clone())
            .unwrap_or_else(|| language_for_path(path));
        solutions.push(SolutionSpec {
            name: filename.into(),
            source_path: (*path).into(),
            language,
            expected_verdict,
            expected_score,
        });
    }
    Ok(solutions)
}

fn matching_submission_metadata<'a>(
    metadata: &'a BTreeMap<String, SubmissionMetadata>,
    path: &str,
) -> Option<&'a SubmissionMetadata> {
    metadata.get(path).or_else(|| {
        metadata.iter().find_map(|(pattern, value)| {
            pattern
                .strip_suffix('*')
                .filter(|prefix| path.starts_with(prefix))
                .map(|_| value)
        })
    })
}

fn program_files(paths: &BTreeSet<&str>, prefix: &str) -> Vec<ProgramSpec> {
    paths
        .iter()
        .filter(|path| path.starts_with(prefix))
        .filter(|path| !path.ends_with(".yaml") && !path.ends_with(".yml"))
        .enumerate()
        .map(|(index, path)| ProgramSpec {
            id: format!("imported-{:02}", index + 1),
            source_path: (*path).to_string(),
            language: language_for_path(path),
            arguments: vec![],
        })
        .collect()
}

fn publication(
    problem: &ProblemYaml,
    paths: &BTreeSet<&str>,
    titles: &BTreeMap<String, String>,
) -> Result<PublicationSpecV1> {
    let mut samples = Vec::new();
    for path in paths {
        let Some(name) = path
            .strip_prefix("data/sample/")
            .and_then(|path| path.strip_suffix(".in"))
        else {
            continue;
        };
        let output = format!("data/sample/{name}.ans");
        if !paths.contains(output.as_str()) {
            continue;
        }
        samples.push(PublicationSampleV1 {
            name: name.into(),
            input_file: (*path).into(),
            output_file: output,
        });
    }
    let statement_sections = titles
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
    let allowed_languages = problem.languages.values();
    Ok(PublicationSpecV1 {
        category: "Algorithm".into(),
        difficulty: "Unrated".into(),
        grading_category: "algorithmic".into(),
        tags: problem.keywords.clone(),
        allowed_languages,
        statement_sections,
        samples,
    })
}

fn source_attribution(problem: &ProblemYaml) -> Vec<SourceAttribution> {
    let Some(source) = problem.source.as_ref() else {
        return vec![];
    };
    let external_id = match source {
        serde_yaml_ng::Value::String(value) => value.clone(),
        value => serde_yaml_ng::to_string(value)
            .unwrap_or_else(|_| "unparsed ICPC source".into())
            .trim()
            .to_owned(),
    };
    vec![SourceAttribution {
        provider: "ICPC package".into(),
        external_id,
        canonical_url: String::new(),
        license_name: problem.license.clone().unwrap_or_else(|| "unknown".into()),
        attribution: problem.rights_owner.clone().unwrap_or_default(),
    }]
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
        Some("yaml" | "yml") => "application/yaml",
        Some("json") => "application/json",
        Some("py") => "text/x-python",
        Some("rs") => "text/x-rust",
        Some("c" | "h" | "cc" | "cpp" | "cxx" | "hpp") => "text/x-c",
        Some("java" | "kt" | "go") => "text/plain",
        _ => "application/octet-stream",
    }
}

fn language_for_path(path: &str) -> String {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("py") => "python3",
        Some("cc" | "cpp" | "cxx") => "cpp20",
        Some("c") => "c17",
        Some("rs") => "rust",
        Some("java") => "java17",
        Some("kt") => "kotlin",
        Some("go") => "go",
        _ => "unknown",
    }
    .into()
}

fn read_yaml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_yaml_ng::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icpc_export::export_icpc_2025_09;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    fn add_validator(manifest: &mut ReleaseManifestV1, root: &Path) {
        let path = "validators/main.py";
        let bytes = b"raise SystemExit(0)\n";
        let invalid_path = "validators/invalid.in";
        let invalid = b"invalid\n";
        fs::create_dir_all(root.join("validators")).unwrap();
        fs::write(root.join(path), bytes).unwrap();
        fs::write(root.join(invalid_path), invalid).unwrap();
        manifest.files.push(ManifestFile {
            path: path.into(),
            sha256: Sha256Digest::from_bytes(bytes),
            size_bytes: bytes.len() as u64,
            media_type: "text/x-python".into(),
            executable: true,
        });
        manifest.files.push(ManifestFile {
            path: invalid_path.into(),
            sha256: Sha256Digest::from_bytes(invalid),
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
    fn imports_an_exported_standard_package_and_preserves_meaningful_fields() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir(&source).unwrap();
        crate::init_project(&source, "Round Trip").unwrap();
        let mut original = crate::read_manifest(&source.join("reporch.problem.json")).unwrap();
        add_validator(&mut original, &source);
        let archive = temporary.path().join("roundtrip.zip");
        export_icpc_2025_09(&original, &source, &archive).unwrap();
        let destination = temporary.path().join("imported");

        let imported = import_icpc_2025_09(&archive, &destination).unwrap();

        assert_eq!(imported.project_id, original.project_id);
        assert_eq!(imported.problem_type, ProblemType::Standard);
        assert_eq!(imported.title, original.title);
        assert_eq!(imported.judging.limits, original.judging.limits);
        assert_eq!(imported.judging.tests.len(), original.judging.tests.len());
        assert_eq!(
            imported
                .solutions
                .iter()
                .filter(|solution| solution.expected_verdict == ExpectedVerdict::Accepted)
                .count(),
            2
        );
        assert!(validate_manifest(&imported).is_empty());
        assert!(destination.join("reporch.import-report.json").is_file());
    }

    #[test]
    fn imports_sidecar_free_submit_answer_directories_in_stable_test_order() {
        let temporary = tempfile::tempdir().unwrap();
        let archive_path = temporary.path().join("external-submit-answer.zip");
        let mut archive = ZipWriter::new(File::create(&archive_path).unwrap());
        let options = SimpleFileOptions::DEFAULT.compression_method(CompressionMethod::Deflated);
        let project_id = Uuid::now_v7();
        let files = [
            (
                "fixture/problem.yaml",
                format!(
                    "problem_format_version: 2025-09\ntype: pass-fail submit-answer\nname: External Submit Answer\nuuid: {project_id}\n"
                ),
            ),
            (
                "fixture/statement/problem.en.md",
                "# External Submit Answer\n\nProvide the answer.\n".into(),
            ),
            ("fixture/data/secret/0001.in", "question\n".into()),
            ("fixture/data/secret/0001.ans", "42\n".into()),
            (
                "fixture/input_validators/main.py",
                "raise SystemExit(42)\n".into(),
            ),
            ("fixture/data/invalid_input/0001.in", "invalid\n".into()),
            (
                "fixture/submissions/accepted/official/0001.out",
                "42\n".into(),
            ),
            (
                "fixture/submissions/wrong_answer/known-wrong/0001.out",
                "41\n".into(),
            ),
        ];
        for (path, contents) in files {
            archive.start_file(path, options).unwrap();
            archive.write_all(contents.as_bytes()).unwrap();
        }
        archive.finish().unwrap();

        let imported = import_icpc_2025_09(
            &archive_path,
            &temporary.path().join("external-submit-answer"),
        )
        .unwrap();

        assert_eq!(imported.problem_type, ProblemType::OutputOnly);
        assert_eq!(imported.output_submissions.len(), 2);
        assert!(
            imported
                .output_submissions
                .iter()
                .any(|submission| submission.expected_verdict == ExpectedVerdict::Accepted)
        );
        assert!(
            imported
                .output_submissions
                .iter()
                .all(|submission| submission.outputs.len() == 1)
        );
        assert!(validate_manifest(&imported).is_empty());
    }

    #[test]
    fn rejects_traversal_and_removes_the_partial_destination() {
        let temporary = tempfile::tempdir().unwrap();
        let archive_path = temporary.path().join("bad.zip");
        let mut archive = ZipWriter::new(File::create(&archive_path).unwrap());
        archive
            .start_file(
                "fixture/../../escape",
                SimpleFileOptions::DEFAULT.compression_method(CompressionMethod::Stored),
            )
            .unwrap();
        archive.write_all(b"bad").unwrap();
        archive.finish().unwrap();
        let destination = temporary.path().join("imported");

        let error = import_icpc_2025_09(&archive_path, &destination).unwrap_err();

        assert!(error.to_string().contains("unsafe ZIP entry"));
        assert!(!destination.exists());
        assert!(!temporary.path().join("escape").exists());
    }

    #[test]
    fn imports_an_external_deflated_scoring_package() {
        let temporary = tempfile::tempdir().unwrap();
        let archive_path = temporary.path().join("external.zip");
        let mut archive = ZipWriter::new(File::create(&archive_path).unwrap());
        let options = SimpleFileOptions::DEFAULT.compression_method(CompressionMethod::Deflated);
        let project_id = Uuid::now_v7();
        let files = [
            (
                "external/problem.yaml",
                format!(
                    "problem_format_version: 2025-09\nname:\n  en: External Scored\nuuid: {project_id}\ntype: scoring\nlimits:\n  time_limit: 2.0\n  memory: 512\n  output: 4\n"
                ),
            ),
            (
                "external/statement/problem.en.md",
                "# External Scored\n\nSolve it.\n".into(),
            ),
            (
                "external/data/secret/test_group.yaml",
                "max_score: 100\nscore_aggregation: sum\n".into(),
            ),
            (
                "external/data/secret/easy/test_group.yaml",
                "max_score: 50\nscore_aggregation: pass-fail\n".into(),
            ),
            ("external/data/secret/easy/01.in", "1\n".into()),
            ("external/data/secret/easy/01.ans", "1\n".into()),
            (
                "external/data/secret/hard/test_group.yaml",
                "max_score: 50\nscore_aggregation: pass-fail\nrequire_pass: secret/easy\n".into(),
            ),
            ("external/data/secret/hard/02.in", "2\n".into()),
            ("external/data/secret/hard/02.ans", "2\n".into()),
            (
                "external/input_validators/main.py",
                "raise SystemExit(0)\n".into(),
            ),
            ("external/data/invalid_input/01.in", "invalid\n".into()),
            (
                "external/submissions/accepted/a.py",
                "print(input())\n".into(),
            ),
            (
                "external/submissions/accepted/b.py",
                "import sys\nprint(sys.stdin.readline().strip())\n".into(),
            ),
            (
                "external/submissions/rejected/partial.py",
                "print(1)\n".into(),
            ),
            (
                "external/submissions/submissions.yaml",
                "rejected/partial.py:\n  score: 50\n".into(),
            ),
        ];
        for (path, contents) in files {
            archive.start_file(path, options).unwrap();
            archive.write_all(contents.as_bytes()).unwrap();
        }
        archive.finish().unwrap();
        let destination = temporary.path().join("imported");

        let imported = import_icpc_2025_09(&archive_path, &destination).unwrap();

        assert_eq!(imported.problem_type, ProblemType::Scored);
        assert_eq!(imported.judging.limits.time_ms, 2_000);
        assert_eq!(imported.judging.limits.memory_mib, 512);
        assert_eq!(imported.judging.limits.output_kib, 4_096);
        assert_eq!(imported.judging.groups.len(), 2);
        assert_eq!(
            imported
                .judging
                .groups
                .iter()
                .find(|group| group.id == "hard")
                .unwrap()
                .depends_on,
            vec!["easy"]
        );
        let partial = imported
            .solutions
            .iter()
            .find(|solution| solution.expected_verdict == ExpectedVerdict::Partial)
            .unwrap();
        assert_eq!(partial.expected_score.as_ref().unwrap().minimum, 50.0);
        assert!(validate_manifest(&imported).is_empty());
    }

    #[test]
    fn rejects_symlink_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let archive_path = temporary.path().join("symlink.zip");
        let mut archive = ZipWriter::new(File::create(&archive_path).unwrap());
        archive
            .add_symlink(
                "fixture/problem.yaml",
                "target",
                SimpleFileOptions::DEFAULT
                    .compression_method(CompressionMethod::Stored)
                    .unix_permissions(0o777),
            )
            .unwrap();
        archive.finish().unwrap();

        let error =
            import_icpc_2025_09(&archive_path, &temporary.path().join("imported")).unwrap_err();

        assert!(error.to_string().contains("symlink or special file"));
    }
}
