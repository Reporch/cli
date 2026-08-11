use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use studio_core::{
    CheckerSpec, CheckerTestSpec, ExpectedVerdict, JudgingSpec, ManifestFile, PackageProfile,
    ProblemType, ProgramSpec, PublicationSampleV1, PublicationSpecV1, RELEASE_MANIFEST_SCHEMA_V1,
    ReleaseManifestV1, ResourceLimits, Sha256Digest, SolutionSpec, SourceAttribution,
    StatementSectionsV1, TestCaseSpec, ValidatorTestSpec, compatibility_report,
    normalize_relative_path, validate_manifest,
};
use uuid::Uuid;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, System, ZipArchive, ZipWriter};

use crate::icpc_import::{extract_archive, scan_archive};
use crate::statement_tex::{escape_latex, markdown_to_tex};

const SIDECAR_SCHEMA_V1: &str = "reporch.icpc-legacy-sidecar.v1";
const SIDECAR_PATH: &str = "reporch_legacy/sidecar-v1.json";
const NATIVE_PREFIX: &str = "reporch_legacy/native/";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarFileV1 {
    path: String,
    sha256: String,
    size_bytes: u64,
    executable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySidecarV1 {
    schema: String,
    manifest_digest: String,
    manifest: ReleaseManifestV1,
    projection_files: Vec<SidecarFileV1>,
    native_files: Vec<SidecarFileV1>,
}

enum EntrySource {
    Bytes(Vec<u8>),
    File {
        path: PathBuf,
        sha256: String,
        size_bytes: u64,
    },
}

struct ExportEntry {
    path: String,
    executable: bool,
    source: EntrySource,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyProblemYaml {
    problem_format_version: String,
    name: String,
    #[serde(default)]
    uuid: Option<Uuid>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    rights_owner: Option<String>,
    #[serde(default)]
    limits: LegacyLimitsYaml,
    #[serde(default)]
    keywords: Option<String>,
    #[serde(default)]
    validation: Option<String>,
    #[serde(default)]
    #[serde(rename = "validator_flags")]
    _validator_flags: Option<String>,
    #[serde(default)]
    author: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyLimitsYaml {
    #[serde(default)]
    time_multiplier: Option<f64>,
    #[serde(default)]
    #[serde(rename = "time_safety_margin")]
    _time_safety_margin: Option<f64>,
    #[serde(default)]
    memory: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
    #[serde(default)]
    #[serde(rename = "code")]
    _code: Option<u64>,
    #[serde(default)]
    #[serde(rename = "compilation_time")]
    _compilation_time: Option<f64>,
    #[serde(default)]
    #[serde(rename = "compilation_memory")]
    _compilation_memory: Option<u64>,
    #[serde(default)]
    #[serde(rename = "validation_time")]
    _validation_time: Option<f64>,
    #[serde(default)]
    #[serde(rename = "validation_memory")]
    _validation_memory: Option<u64>,
    #[serde(default)]
    #[serde(rename = "validation_output")]
    _validation_output: Option<u64>,
}

struct CreatedDirectory {
    path: PathBuf,
    armed: bool,
}

impl CreatedDirectory {
    fn create(path: &Path) -> Result<Self> {
        ensure!(!path.exists(), "import destination already exists");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir(path)?;
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

pub fn export_icpc_legacy(
    manifest: &ReleaseManifestV1,
    source_root: &Path,
    output: &Path,
) -> Result<()> {
    let errors = validate_manifest(manifest)
        .into_iter()
        .filter(|issue| issue.severity == studio_core::IssueSeverity::Error)
        .collect::<Vec<_>>();
    ensure!(
        errors.is_empty(),
        "native manifest validation failed: {}",
        serde_json::to_string(&errors)?
    );
    let report = compatibility_report(manifest, PackageProfile::IcpcLegacy);
    ensure!(
        report.exportable,
        "legacy ICPC export is blocked: {}",
        serde_json::to_string(&report)?
    );
    ensure!(
        manifest.problem_type == ProblemType::Standard,
        "legacy ICPC v1 supports only standard pass/fail problems"
    );
    let root = package_root(output)?;
    let mut occupied = BTreeSet::new();
    let mut entries = Vec::new();
    let mut projection = Vec::new();

    add_projection_bytes(
        "problem.yaml",
        legacy_problem_yaml(manifest).into_bytes(),
        false,
        &mut occupied,
        &mut entries,
        &mut projection,
    )?;
    add_projection_bytes(
        "reporch_compatibility.json",
        json_text(&report)?,
        false,
        &mut occupied,
        &mut entries,
        &mut projection,
    )?;

    for (locale, source_path) in &manifest.statements {
        let bytes = read_native_file(manifest, source_root, source_path)?;
        let markdown = String::from_utf8(bytes)
            .with_context(|| format!("legacy statement is not UTF-8: {source_path}"))?;
        let title = manifest
            .title
            .get(locale)
            .or_else(|| manifest.title.get(&manifest.default_locale))
            .context("validated default title is missing")?;
        let mut tex = format!("\\problemname{{{}}}\n", escape_latex(title));
        tex.push_str(&markdown_to_tex(&markdown));
        if !tex.ends_with('\n') {
            tex.push('\n');
        }
        add_projection_bytes(
            &format!("problem_statement/problem.{}.tex", legacy_locale(locale)?),
            tex.into_bytes(),
            false,
            &mut occupied,
            &mut entries,
            &mut projection,
        )?;
    }

    for (index, test) in manifest.judging.tests.iter().enumerate() {
        let name = format!("{:04}", index + 1);
        add_projection_source(
            manifest,
            source_root,
            &test.input_file,
            &format!("data/secret/{name}.in"),
            false,
            &mut occupied,
            &mut entries,
            &mut projection,
        )?;
        let answer = test
            .answer_file
            .as_deref()
            .with_context(|| format!("legacy secret test {} has no answer", test.name))?;
        add_projection_source(
            manifest,
            source_root,
            answer,
            &format!("data/secret/{name}.ans"),
            false,
            &mut occupied,
            &mut entries,
            &mut projection,
        )?;
    }
    if let Some(publication) = manifest.publication.as_ref() {
        for (index, sample) in publication.samples.iter().enumerate() {
            let name = format!("{:04}", index + 1);
            for (source, suffix) in [(&sample.input_file, "in"), (&sample.output_file, "ans")] {
                add_projection_source(
                    manifest,
                    source_root,
                    source,
                    &format!("data/sample/{name}.{suffix}"),
                    false,
                    &mut occupied,
                    &mut entries,
                    &mut projection,
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
        add_projection_source(
            manifest,
            source_root,
            &test.input_file,
            &format!("data/invalid_input/{:04}.in", index + 1),
            false,
            &mut occupied,
            &mut entries,
            &mut projection,
        )?;
    }
    for (index, test) in manifest.judging.checker_tests.iter().enumerate() {
        let directory = if test.expected_accepted {
            "valid_output"
        } else {
            "invalid_output"
        };
        let name = format!("{:04}", index + 1);
        for (source, suffix) in [
            (&test.input_file, "in"),
            (&test.answer_file, "ans"),
            (&test.output_file, "out"),
        ] {
            add_projection_source(
                manifest,
                source_root,
                source,
                &format!("data/{directory}/{name}.{suffix}"),
                false,
                &mut occupied,
                &mut entries,
                &mut projection,
            )?;
        }
    }

    for (index, solution) in manifest.solutions.iter().enumerate() {
        let directory = legacy_verdict_directory(solution.expected_verdict);
        let extension = Path::new(&solution.source_path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(legacy_component)
            .filter(|extension| !extension.is_empty())
            .unwrap_or_else(|| "txt".into());
        add_projection_source(
            manifest,
            source_root,
            &solution.source_path,
            &format!(
                "submissions/{directory}/{:02}_{}.{}",
                index + 1,
                legacy_component(&solution.name),
                extension
            ),
            false,
            &mut occupied,
            &mut entries,
            &mut projection,
        )?;
    }
    let mut validator_index = 0_usize;
    if let Some(path) = manifest.judging.validator_path.as_deref() {
        add_program_projection(
            manifest,
            source_root,
            path,
            "input_validators",
            &mut validator_index,
            &mut occupied,
            &mut entries,
            &mut projection,
        )?;
    }
    for path in &manifest.judging.extra_validator_paths {
        add_program_projection(
            manifest,
            source_root,
            path,
            "input_validators",
            &mut validator_index,
            &mut occupied,
            &mut entries,
            &mut projection,
        )?;
    }
    for validator in &manifest.judging.extra_validators {
        add_program_projection(
            manifest,
            source_root,
            &validator.source_path,
            "input_validators",
            &mut validator_index,
            &mut occupied,
            &mut entries,
            &mut projection,
        )?;
    }
    if let CheckerSpec::Custom { source_path, .. } = &manifest.judging.checker {
        let mut checker_index = 0;
        add_program_projection(
            manifest,
            source_root,
            source_path,
            "output_validators",
            &mut checker_index,
            &mut occupied,
            &mut entries,
            &mut projection,
        )?;
    }

    let mut native_files = Vec::with_capacity(manifest.files.len());
    for (index, file) in manifest.files.iter().enumerate() {
        let archive_path = format!("{NATIVE_PREFIX}{:08}.bin", index + 1);
        add_file_entry(
            &archive_path,
            source_root.join(&file.path),
            file.sha256.as_str(),
            file.size_bytes,
            file.executable,
            &mut occupied,
            &mut entries,
        )?;
        native_files.push(SidecarFileV1 {
            path: file.path.clone(),
            sha256: file.sha256.as_str().into(),
            size_bytes: file.size_bytes,
            executable: file.executable,
        });
    }
    let sidecar = LegacySidecarV1 {
        schema: SIDECAR_SCHEMA_V1.into(),
        manifest_digest: manifest.digest()?.to_string(),
        manifest: manifest.clone(),
        projection_files: projection,
        native_files,
    };
    add_bytes_entry(
        SIDECAR_PATH,
        json_text(&sidecar)?,
        false,
        &mut occupied,
        &mut entries,
    )?;

    entries.sort_by(|left, right| left.path.cmp(&right.path));
    for entry in &entries {
        verify_export_entry(entry)?;
    }
    let output_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .with_context(|| format!("create legacy ICPC archive {}", output.display()))?;
    let mut archive = ZipWriter::new(output_file);
    for entry in entries {
        let archive_path = format!("{root}/{}", entry.path);
        archive.start_file(&archive_path, zip_options(entry.executable))?;
        match entry.source {
            EntrySource::Bytes(bytes) => archive.write_all(&bytes)?,
            EntrySource::File { path, .. } => {
                let mut source = File::open(path)?;
                std::io::copy(&mut source, &mut archive)?;
            }
        }
    }
    archive.finish()?.sync_all()?;
    Ok(())
}

pub fn import_icpc_legacy(input: &Path, directory: &Path) -> Result<ReleaseManifestV1> {
    let input_file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut archive = ZipArchive::new(input_file).context("read legacy ICPC ZIP")?;
    let (root, entries) = scan_archive(&mut archive)?;
    ensure_legacy_root(input, &root)?;
    for path in entries
        .iter()
        .filter_map(|entry| entry.relative_path.as_deref())
    {
        validate_legacy_package_path(path)?;
    }
    let temporary = tempfile::tempdir()?;
    extract_archive(&mut archive, temporary.path(), &root, &entries)?;
    validate_legacy_text_files(temporary.path(), &entries)?;
    if temporary.path().join(SIDECAR_PATH).is_file() {
        import_with_sidecar(input, directory, &root, temporary.path(), &entries)
    } else {
        import_external_legacy(input, directory, &root, temporary.path(), &entries)
    }
}

fn import_with_sidecar(
    input: &Path,
    directory: &Path,
    root: &str,
    extracted: &Path,
    entries: &[crate::icpc_import::ScannedEntry],
) -> Result<ReleaseManifestV1> {
    let sidecar: LegacySidecarV1 = serde_json::from_slice(&fs::read(extracted.join(SIDECAR_PATH))?)
        .context("parse legacy ICPC sidecar")?;
    ensure!(
        sidecar.schema == SIDECAR_SCHEMA_V1,
        "unsupported legacy sidecar schema"
    );
    ensure!(
        sidecar.manifest.digest()?.as_str() == sidecar.manifest_digest,
        "legacy sidecar manifest digest mismatch"
    );
    let errors = validate_manifest(&sidecar.manifest)
        .into_iter()
        .filter(|issue| issue.severity == studio_core::IssueSeverity::Error)
        .collect::<Vec<_>>();
    ensure!(errors.is_empty(), "legacy sidecar manifest is invalid");

    let mut declared = BTreeSet::new();
    let executable_by_path = entries
        .iter()
        .filter_map(|entry| {
            entry
                .relative_path
                .as_ref()
                .map(|path| (path.as_str(), entry.executable))
        })
        .collect::<BTreeMap<_, _>>();
    for file in &sidecar.projection_files {
        verify_extracted_file(extracted, file, &file.path)?;
        ensure!(
            executable_by_path.get(file.path.as_str()) == Some(&file.executable),
            "sidecar projection executable bit mismatch"
        );
        ensure!(
            declared.insert(file.path.clone()),
            "duplicate sidecar projection path"
        );
    }
    ensure!(
        sidecar.native_files.len() == sidecar.manifest.files.len(),
        "legacy sidecar native file count mismatch"
    );
    for (index, (declared_file, manifest_file)) in sidecar
        .native_files
        .iter()
        .zip(&sidecar.manifest.files)
        .enumerate()
    {
        ensure!(
            declared_file.path == manifest_file.path
                && declared_file.sha256 == manifest_file.sha256.as_str()
                && declared_file.size_bytes == manifest_file.size_bytes
                && declared_file.executable == manifest_file.executable,
            "legacy sidecar native declaration disagrees with manifest"
        );
        let archive_path = format!("{NATIVE_PREFIX}{:08}.bin", index + 1);
        verify_extracted_file(extracted, declared_file, &archive_path)?;
        ensure!(
            executable_by_path.get(archive_path.as_str()) == Some(&declared_file.executable),
            "sidecar native executable bit mismatch"
        );
        ensure!(
            declared.insert(archive_path),
            "duplicate sidecar native path"
        );
    }
    ensure!(
        declared.insert(SIDECAR_PATH.into()),
        "duplicate sidecar path"
    );
    let actual = entries
        .iter()
        .filter_map(|entry| entry.relative_path.as_ref())
        .filter(|path| extracted.join(path).is_file())
        .cloned()
        .collect::<BTreeSet<_>>();
    ensure!(
        actual == declared,
        "legacy package contains undeclared or missing files"
    );

    let destination = CreatedDirectory::create(directory)?;
    for (index, file) in sidecar.manifest.files.iter().enumerate() {
        let source = extracted.join(format!("{NATIVE_PREFIX}{:08}.bin", index + 1));
        let target = directory.join(&file.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, &target)?;
        set_executable(&target, file.executable)?;
    }
    write_new(
        &directory.join("reporch.problem.json"),
        &serde_json::to_vec_pretty(&sidecar.manifest)?,
    )?;
    write_import_report(
        input,
        directory,
        root,
        true,
        &sidecar.manifest,
        "checksummed Reporch sidecar restored the exact native manifest and file set",
    )?;
    destination.finish();
    Ok(sidecar.manifest)
}

fn import_external_legacy(
    input: &Path,
    directory: &Path,
    root: &str,
    extracted: &Path,
    entries: &[crate::icpc_import::ScannedEntry],
) -> Result<ReleaseManifestV1> {
    ensure!(
        !extracted.join("reporch.problem.json").exists(),
        "external package reserves reporch.problem.json"
    );
    let problem: LegacyProblemYaml =
        serde_yaml_ng::from_slice(&fs::read(extracted.join("problem.yaml"))?)
            .context("parse legacy problem.yaml")?;
    ensure!(
        problem.problem_format_version == "legacy-icpc",
        "expected problem_format_version: legacy-icpc"
    );
    ensure!(
        !problem.name.trim().is_empty(),
        "legacy problem name is empty"
    );
    if let Some(validation) = problem.validation.as_deref() {
        ensure!(
            matches!(validation, "default" | "custom"),
            "unsupported legacy validation mode"
        );
    }
    let paths = entries
        .iter()
        .filter_map(|entry| entry.relative_path.as_deref())
        .filter(|path| extracted.join(path).is_file())
        .collect::<BTreeSet<_>>();
    let statements = import_legacy_statements(extracted, &paths)?;
    let default_locale = if statements.contains_key("en") {
        "en".into()
    } else {
        statements
            .keys()
            .next()
            .context("legacy package has no statement")?
            .clone()
    };
    let title = statements
        .keys()
        .map(|locale| (locale.clone(), problem.name.clone()))
        .collect();
    let tests = import_legacy_tests(&paths)?;
    let solutions = import_legacy_solutions(extracted, &paths)?;
    let validator_programs = import_programs(&paths, "input_validators/")?;
    let validator = validator_programs.first().cloned();
    let extra_validators = validator_programs.into_iter().skip(1).collect::<Vec<_>>();
    let checker_program = import_programs(&paths, "output_validators/")?
        .into_iter()
        .next();
    let checker = checker_program
        .as_ref()
        .map_or(CheckerSpec::Token, |program| CheckerSpec::Custom {
            source_path: program.source_path.clone(),
            language: program.language.clone(),
        });
    let validator_tests = import_legacy_validator_tests(&paths, &tests, validator.is_some());
    let checker_tests = import_legacy_checker_tests(&paths, checker_program.is_some())?;
    let publication = import_legacy_publication(&problem, &paths, &default_locale);
    let time_multiplier = problem.limits.time_multiplier.unwrap_or(5.0);
    ensure!(
        time_multiplier.is_finite() && time_multiplier > 0.0,
        "invalid legacy time multiplier"
    );
    let files = entries
        .iter()
        .filter_map(|entry| {
            entry.relative_path.as_deref().and_then(|path| {
                extracted
                    .join(path)
                    .is_file()
                    .then_some((path, entry.executable))
            })
        })
        .map(|(path, executable)| manifest_file(extracted, path, executable))
        .collect::<Result<Vec<_>>>()?;
    let manifest = ReleaseManifestV1 {
        schema: RELEASE_MANIFEST_SCHEMA_V1.into(),
        project_id: problem.uuid.unwrap_or_else(Uuid::now_v7),
        commit_id: Uuid::now_v7(),
        problem_type: ProblemType::Standard,
        package_profile: PackageProfile::IcpcLegacy,
        default_locale: default_locale.clone(),
        title,
        statements,
        files,
        toolchains: BTreeMap::new(),
        judging: JudgingSpec {
            limits: ResourceLimits {
                time_ms: 1_000,
                memory_mib: problem.limits.memory.unwrap_or(1024),
                output_kib: problem.limits.output.unwrap_or(64).saturating_mul(1024),
            },
            checker,
            tests,
            groups: vec![],
            generators: vec![],
            validator_path: validator
                .as_ref()
                .map(|program| program.source_path.clone()),
            validator_language: validator.as_ref().map(|program| program.language.clone()),
            extra_validator_paths: vec![],
            extra_validators,
            validator_tests,
            checker_tests,
            interactor_path: None,
            interactor_language: None,
            grader_path: None,
            grader_language: None,
            harness: None,
        },
        sources: import_legacy_sources(&problem),
        solutions,
        output_submissions: vec![],
        publication: Some(publication),
        policy_version: "studio-policy-v1".into(),
    };
    let destination = CreatedDirectory::create(directory)?;
    copy_tree(extracted, directory)?;
    write_new(
        &directory.join("reporch.problem.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    write_import_report(
        input,
        directory,
        root,
        false,
        &manifest,
        "sidecar-free legacy semantics were imported; review TeX/PDF statements and auxiliary files",
    )?;
    destination.finish();
    Ok(manifest)
}

fn legacy_problem_yaml(manifest: &ReleaseManifestV1) -> String {
    let title = manifest
        .title
        .get(&manifest.default_locale)
        .expect("validated default title");
    let mut yaml = format!(
        "problem_format_version: legacy-icpc\nname: {}\nuuid: {}\n",
        json_string(title),
        manifest.project_id
    );
    if let Some(source) = manifest.sources.first() {
        yaml.push_str(&format!(
            "source: {}\n",
            json_string(&format!("{} {}", source.provider, source.external_id))
        ));
        if !source.canonical_url.is_empty() {
            yaml.push_str(&format!(
                "source_url: {}\n",
                json_string(&source.canonical_url)
            ));
        }
        let license = legacy_license(&source.license_name);
        yaml.push_str(&format!("license: {}\n", json_string(license)));
        if !matches!(license, "unknown" | "public domain") && !source.attribution.is_empty() {
            yaml.push_str(&format!(
                "rights_owner: {}\n",
                json_string(&source.attribution)
            ));
        }
    } else {
        yaml.push_str("license: unknown\n");
    }
    yaml.push_str(&format!(
        "limits:\n  time_multiplier: 1.0\n  memory: {}\n  output: {}\n",
        manifest.judging.limits.memory_mib,
        manifest.judging.limits.output_kib.div_ceil(1024).max(1)
    ));
    yaml.push_str(
        if matches!(manifest.judging.checker, CheckerSpec::Custom { .. }) {
            "validation: custom\n"
        } else {
            "validation: default\n"
        },
    );
    if let Some(publication) = manifest.publication.as_ref()
        && !publication.tags.is_empty()
    {
        yaml.push_str(&format!(
            "keywords: {}\n",
            json_string(&publication.tags.join(" "))
        ));
    }
    yaml
}

fn import_legacy_statements(
    directory: &Path,
    paths: &BTreeSet<&str>,
) -> Result<BTreeMap<String, String>> {
    let mut statements = BTreeMap::new();
    for path in paths {
        let Some(relative) = path.strip_prefix("problem_statement/problem.") else {
            continue;
        };
        let locale = match relative {
            "tex" | "pdf" => Some("en"),
            _ => relative
                .strip_suffix(".tex")
                .or_else(|| relative.strip_suffix(".pdf")),
        };
        let Some(locale) = locale else { continue };
        ensure!(
            !locale.is_empty() && !locale.contains('.'),
            "invalid legacy statement locale"
        );
        legacy_locale(locale)?;
        if path.ends_with(".tex") {
            let statement = fs::read_to_string(directory.join(path))?;
            ensure!(
                statement.contains("\\problemname{"),
                "legacy TeX statement must declare \\problemname"
            );
        }
        ensure!(
            statements.insert(locale.into(), (*path).into()).is_none(),
            "duplicate legacy statement locale"
        );
    }
    ensure!(
        !statements.is_empty(),
        "legacy package has no TeX or PDF statement"
    );
    Ok(statements)
}

fn import_legacy_tests(paths: &BTreeSet<&str>) -> Result<Vec<TestCaseSpec>> {
    let mut tests = Vec::new();
    for path in paths {
        let Some(name) = path
            .strip_prefix("data/secret/")
            .and_then(|path| path.strip_suffix(".in"))
        else {
            continue;
        };
        let answer = format!("data/secret/{name}.ans");
        ensure!(
            paths.contains(answer.as_str()),
            "secret test {name} has no answer"
        );
        tests.push(TestCaseSpec {
            id: Uuid::now_v7(),
            name: name.into(),
            input_file: (*path).into(),
            answer_file: Some(answer),
            groups: vec![],
            generated_by: None,
            generator_arguments: vec![],
            seed: None,
        });
    }
    ensure!(!tests.is_empty(), "legacy package has no secret tests");
    Ok(tests)
}

fn import_legacy_solutions(directory: &Path, paths: &BTreeSet<&str>) -> Result<Vec<SolutionSpec>> {
    paths
        .iter()
        .filter_map(|path| {
            let relative = path.strip_prefix("submissions/")?;
            let (verdict_directory, filename) = relative.split_once('/')?;
            let expected_verdict = match verdict_directory {
                "accepted" => ExpectedVerdict::Accepted,
                "wrong_answer" => ExpectedVerdict::WrongAnswer,
                "time_limit_exceeded" | "brute_force" => ExpectedVerdict::TimeLimit,
                "run_time_error" => ExpectedVerdict::RuntimeError,
                _ => return None,
            };
            Some((path, filename, expected_verdict))
        })
        .map(|(path, filename, expected_verdict)| {
            ensure!(
                !filename.contains('/') && directory.join(path).is_file(),
                "legacy v1 importer supports only single-file submissions"
            );
            Ok(SolutionSpec {
                name: filename.into(),
                source_path: (*path).into(),
                language: language_for_path(path),
                expected_verdict,
                expected_score: None,
            })
        })
        .collect()
}

fn import_programs(paths: &BTreeSet<&str>, prefix: &str) -> Result<Vec<ProgramSpec>> {
    paths
        .iter()
        .filter(|path| path.starts_with(prefix))
        .filter(|path| !path.ends_with(".yaml") && !path.ends_with(".yml"))
        .enumerate()
        .map(|(index, path)| ProgramSpec {
            id: format!("imported-{:02}", index + 1),
            source_path: (*path).into(),
            language: language_for_path(path),
            arguments: vec![],
        })
        .map(|program| {
            let relative = program
                .source_path
                .strip_prefix(prefix)
                .expect("filtered prefix");
            ensure!(
                !relative.contains('/'),
                "legacy v1 importer supports only single-file validator programs"
            );
            Ok(program)
        })
        .collect()
}

fn import_legacy_validator_tests(
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

fn import_legacy_checker_tests(
    paths: &BTreeSet<&str>,
    has_checker: bool,
) -> Result<Vec<CheckerTestSpec>> {
    if !has_checker {
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
            let answer = format!("{prefix}{name}.ans");
            let output = format!("{prefix}{name}.out");
            ensure!(
                paths.contains(answer.as_str()) && paths.contains(output.as_str()),
                "legacy checker test {directory}/{name} is incomplete"
            );
            imported.push(CheckerTestSpec {
                name: format!("imported-{directory}-{name}"),
                input_file: (*path).into(),
                answer_file: answer,
                output_file: output,
                expected_accepted,
            });
        }
    }
    Ok(imported)
}

fn import_legacy_publication(
    problem: &LegacyProblemYaml,
    paths: &BTreeSet<&str>,
    default_locale: &str,
) -> PublicationSpecV1 {
    let samples = paths
        .iter()
        .filter_map(|path| {
            let name = path.strip_prefix("data/sample/")?.strip_suffix(".in")?;
            let output = format!("data/sample/{name}.ans");
            paths
                .contains(output.as_str())
                .then_some(PublicationSampleV1 {
                    name: name.into(),
                    input_file: (*path).into(),
                    output_file: output,
                })
        })
        .collect();
    PublicationSpecV1 {
        category: "Algorithm".into(),
        difficulty: "Unrated".into(),
        grading_category: "algorithmic".into(),
        tags: problem
            .keywords
            .as_deref()
            .map(|keywords| keywords.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
        allowed_languages: vec![],
        statement_sections: BTreeMap::from([(
            default_locale.into(),
            StatementSectionsV1 {
                input_format: String::new(),
                output_format: String::new(),
                note: String::new(),
            },
        )]),
        samples,
    }
}

fn import_legacy_sources(problem: &LegacyProblemYaml) -> Vec<SourceAttribution> {
    let Some(source) = problem.source.as_ref() else {
        return vec![];
    };
    vec![SourceAttribution {
        provider: "ICPC legacy package".into(),
        external_id: source.clone(),
        canonical_url: problem.source_url.clone().unwrap_or_default(),
        license_name: problem.license.clone().unwrap_or_else(|| "unknown".into()),
        attribution: problem
            .rights_owner
            .clone()
            .or_else(|| problem.author.clone())
            .unwrap_or_default(),
    }]
}

#[allow(clippy::too_many_arguments)]
fn add_program_projection(
    manifest: &ReleaseManifestV1,
    source_root: &Path,
    source_path: &str,
    directory: &str,
    index: &mut usize,
    occupied: &mut BTreeSet<String>,
    entries: &mut Vec<ExportEntry>,
    projection: &mut Vec<SidecarFileV1>,
) -> Result<()> {
    *index += 1;
    let filename = Path::new(source_path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(legacy_component)
        .filter(|name| !name.is_empty())
        .context("program filename cannot be represented in legacy ICPC")?;
    add_projection_source(
        manifest,
        source_root,
        source_path,
        &format!("{directory}/{:02}_{filename}", *index),
        true,
        occupied,
        entries,
        projection,
    )
}

#[allow(clippy::too_many_arguments)]
fn add_projection_source(
    manifest: &ReleaseManifestV1,
    source_root: &Path,
    source_path: &str,
    target_path: &str,
    force_executable: bool,
    occupied: &mut BTreeSet<String>,
    entries: &mut Vec<ExportEntry>,
    projection: &mut Vec<SidecarFileV1>,
) -> Result<()> {
    let file = manifest
        .files
        .iter()
        .find(|file| file.path == source_path)
        .with_context(|| format!("manifest file is missing: {source_path}"))?;
    let executable = force_executable || file.executable;
    add_file_entry(
        target_path,
        source_root.join(source_path),
        file.sha256.as_str(),
        file.size_bytes,
        executable,
        occupied,
        entries,
    )?;
    projection.push(SidecarFileV1 {
        path: target_path.into(),
        sha256: file.sha256.as_str().into(),
        size_bytes: file.size_bytes,
        executable,
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_projection_bytes(
    target_path: &str,
    bytes: Vec<u8>,
    executable: bool,
    occupied: &mut BTreeSet<String>,
    entries: &mut Vec<ExportEntry>,
    projection: &mut Vec<SidecarFileV1>,
) -> Result<()> {
    let declaration = SidecarFileV1 {
        path: target_path.into(),
        sha256: hex::encode(Sha256::digest(&bytes)),
        size_bytes: bytes.len() as u64,
        executable,
    };
    add_bytes_entry(target_path, bytes, executable, occupied, entries)?;
    projection.push(declaration);
    Ok(())
}

fn add_file_entry(
    target_path: &str,
    source: PathBuf,
    sha256: &str,
    size_bytes: u64,
    executable: bool,
    occupied: &mut BTreeSet<String>,
    entries: &mut Vec<ExportEntry>,
) -> Result<()> {
    validate_legacy_package_path(target_path)?;
    ensure!(
        occupied.insert(target_path.into()),
        "duplicate legacy export path"
    );
    entries.push(ExportEntry {
        path: target_path.into(),
        executable,
        source: EntrySource::File {
            path: source,
            sha256: sha256.into(),
            size_bytes,
        },
    });
    Ok(())
}

fn add_bytes_entry(
    target_path: &str,
    bytes: Vec<u8>,
    executable: bool,
    occupied: &mut BTreeSet<String>,
    entries: &mut Vec<ExportEntry>,
) -> Result<()> {
    validate_legacy_package_path(target_path)?;
    ensure!(
        occupied.insert(target_path.into()),
        "duplicate legacy export path"
    );
    entries.push(ExportEntry {
        path: target_path.into(),
        executable,
        source: EntrySource::Bytes(bytes),
    });
    Ok(())
}

fn verify_export_entry(entry: &ExportEntry) -> Result<()> {
    if let EntrySource::Bytes(bytes) = &entry.source {
        return validate_legacy_text_bytes(&entry.path, bytes);
    }
    let EntrySource::File {
        path,
        sha256,
        size_bytes,
    } = &entry.source
    else {
        return Ok(());
    };
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    ensure!(
        bytes.len() as u64 == *size_bytes,
        "legacy source size changed"
    );
    ensure!(
        hex::encode(Sha256::digest(&bytes)) == *sha256,
        "legacy source digest changed"
    );
    validate_legacy_text_bytes(&entry.path, &bytes)?;
    Ok(())
}

fn read_native_file(
    manifest: &ReleaseManifestV1,
    source_root: &Path,
    source_path: &str,
) -> Result<Vec<u8>> {
    let file = manifest
        .files
        .iter()
        .find(|file| file.path == source_path)
        .context("manifest file is missing")?;
    let bytes = fs::read(source_root.join(source_path))?;
    ensure!(
        bytes.len() as u64 == file.size_bytes,
        "native file size changed"
    );
    ensure!(
        Sha256Digest::from_bytes(&bytes) == file.sha256,
        "native file digest changed"
    );
    Ok(bytes)
}

fn verify_extracted_file(extracted: &Path, file: &SidecarFileV1, archive_path: &str) -> Result<()> {
    ensure!(
        normalize_relative_path(archive_path)? == archive_path,
        "unsafe sidecar path"
    );
    let metadata = fs::metadata(extracted.join(archive_path))
        .with_context(|| format!("missing sidecar-declared file {archive_path}"))?;
    ensure!(
        metadata.is_file() && metadata.len() == file.size_bytes,
        "sidecar file size mismatch"
    );
    let bytes = fs::read(extracted.join(archive_path))?;
    ensure!(
        hex::encode(Sha256::digest(&bytes)) == file.sha256,
        "sidecar file digest mismatch"
    );
    Ok(())
}

fn package_root(output: &Path) -> Result<String> {
    ensure!(
        output.extension().and_then(|extension| extension.to_str()) == Some("zip"),
        "legacy ICPC output must use .zip"
    );
    let root = output
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("legacy ICPC output has no UTF-8 stem")?;
    ensure!(
        !root.is_empty()
            && root
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
        "legacy ICPC package name must contain only lowercase ASCII letters and digits"
    );
    Ok(root.into())
}

fn ensure_legacy_root(input: &Path, root: &str) -> Result<()> {
    ensure!(
        !root.is_empty()
            && root
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
        "legacy ICPC root must contain only lowercase ASCII letters and digits"
    );
    let stem = input
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("legacy archive has no UTF-8 stem")?;
    ensure!(
        stem == root,
        "legacy archive filename and package root must match"
    );
    Ok(())
}

fn validate_legacy_package_path(path: &str) -> Result<()> {
    ensure!(
        normalize_relative_path(path)? == path,
        "legacy path is not canonical"
    );
    for component in path.split('/') {
        ensure!(
            component.len() >= 2
                && component.len() <= 255
                && component
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && component
                    .bytes()
                    .last()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
            "legacy path component is not portable: {component:?}"
        );
    }
    Ok(())
}

fn legacy_locale(locale: &str) -> Result<String> {
    let mut parts = locale.split('-');
    let language = parts.next().unwrap_or_default();
    let region = parts.next();
    ensure!(
        matches!(language.len(), 2 | 3)
            && language.bytes().all(|byte| byte.is_ascii_lowercase())
            && parts.next().is_none()
            && region.is_none_or(|value| {
                value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_uppercase())
            }),
        "legacy statement locale must be an ISO 639 language with an optional uppercase region"
    );
    Ok(locale.into())
}

fn legacy_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches(['.', '-'])
        .to_owned()
}

fn legacy_verdict_directory(verdict: ExpectedVerdict) -> &'static str {
    match verdict {
        ExpectedVerdict::Accepted => "accepted",
        ExpectedVerdict::WrongAnswer | ExpectedVerdict::Partial => "wrong_answer",
        ExpectedVerdict::TimeLimit => "time_limit_exceeded",
        ExpectedVerdict::RuntimeError | ExpectedVerdict::MemoryLimit => "run_time_error",
    }
}

fn legacy_license(value: &str) -> &'static str {
    match value.to_ascii_lowercase().as_str() {
        "public domain" => "public domain",
        "cc0" => "cc0",
        "cc by" => "cc by",
        "cc by-sa" => "cc by-sa",
        "educational" => "educational",
        "permission" => "permission",
        _ => "unknown",
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
        Some("pdf") => "application/pdf",
        Some("yaml" | "yml") => "application/yaml",
        Some("json") => "application/json",
        Some("py") => "text/x-python",
        Some("rs") => "text/x-rust",
        Some("c" | "h" | "cc" | "cpp" | "cxx" | "hpp") => "text/x-c",
        _ => "application/octet-stream",
    }
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn validate_legacy_text_files(
    directory: &Path,
    entries: &[crate::icpc_import::ScannedEntry],
) -> Result<()> {
    for path in entries
        .iter()
        .filter_map(|entry| entry.relative_path.as_deref())
    {
        if path.starts_with(NATIVE_PREFIX) || !is_legacy_text_path(path) {
            continue;
        }
        validate_legacy_text_bytes(path, &fs::read(directory.join(path))?)?;
    }
    Ok(())
}

fn validate_legacy_text_bytes(path: &str, bytes: &[u8]) -> Result<()> {
    if !is_legacy_text_path(path) {
        return Ok(());
    }
    ensure!(
        !bytes.starts_with(&[0xef, 0xbb, 0xbf]),
        "legacy text file has a UTF-8 BOM: {path}"
    );
    ensure!(
        !bytes.contains(&b'\r'),
        "legacy text file has non-LF line endings: {path}"
    );
    ensure!(
        bytes.is_empty() || bytes.ends_with(b"\n"),
        "legacy text file does not end with LF: {path}"
    );
    std::str::from_utf8(bytes).with_context(|| format!("legacy text file is not UTF-8: {path}"))?;
    Ok(())
}

fn is_legacy_text_path(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str()),
        Some(
            "ans"
                | "c"
                | "cc"
                | "cpp"
                | "ctd"
                | "desc"
                | "go"
                | "h"
                | "hint"
                | "hpp"
                | "in"
                | "java"
                | "json"
                | "kt"
                | "md"
                | "out"
                | "py"
                | "rs"
                | "tex"
                | "txt"
                | "viva"
                | "yaml"
                | "yml"
        )
    )
}

fn set_executable(path: &Path, executable: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
        )?;
    }
    #[cfg(not(unix))]
    let _ = (path, executable);
    Ok(())
}

fn zip_options(executable: bool) -> SimpleFileOptions {
    SimpleFileOptions::DEFAULT
        .compression_method(CompressionMethod::Stored)
        .system(System::Unix)
        .unix_permissions(if executable { 0o755 } else { 0o644 })
}

fn write_import_report(
    input: &Path,
    directory: &Path,
    root: &str,
    exact: bool,
    manifest: &ReleaseManifestV1,
    note: &str,
) -> Result<()> {
    write_new(
        &directory.join("reporch.import-report.json"),
        &serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "reporch.import-report.v1",
            "source_profile": "icpc_legacy",
            "target_profile": "reporch_native",
            "source_archive": input.file_name().and_then(|name| name.to_str()),
            "package_root": root,
            "exact_native_round_trip": exact,
            "manifest_digest": manifest.digest()?.to_string(),
            "note": note,
        }))?,
    )
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("string serialization cannot fail")
}

fn json_text(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/corpus/polygon-basic")
    }

    fn external_corpus() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/corpus/icpc-legacy-basic/external")
    }

    fn write_external_corpus_archive(path: &Path) {
        let source = external_corpus();
        let mut archive = ZipWriter::new(File::create(path).unwrap());
        for relative in [
            "problem.yaml",
            "problem_statement/problem.en.tex",
            "data/sample/01.in",
            "data/sample/01.ans",
            "data/secret/01.in",
            "data/secret/01.ans",
            "input_validators/validator.py",
            "submissions/accepted/main.py",
            "submissions/wrong_answer/constant.py",
        ] {
            archive
                .start_file(
                    format!("external/{relative}"),
                    zip_options(relative.starts_with("input_validators/")),
                )
                .unwrap();
            archive
                .write_all(&fs::read(source.join(relative)).unwrap())
                .unwrap();
        }
        archive.finish().unwrap();
    }

    #[test]
    fn exported_package_is_deterministic_and_round_trips_exactly() {
        let source = corpus();
        let expected: serde_json::Value = serde_json::from_slice(
            &fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../evals/corpus/icpc-legacy-basic/expected.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let manifest: ReleaseManifestV1 =
            serde_json::from_slice(&fs::read(source.join("reporch.problem.json")).unwrap())
                .unwrap();
        assert_eq!(
            manifest.digest().unwrap().as_str(),
            expected["native_manifest_sha256"].as_str().unwrap()
        );
        let temporary = tempfile::tempdir().unwrap();
        let first = temporary.path().join("legacy.zip");
        export_icpc_legacy(&manifest, &source, &first).unwrap();
        let first_bytes = fs::read(&first).unwrap();
        assert_eq!(
            hex::encode(Sha256::digest(&first_bytes)),
            expected["legacy_zip_sha256"].as_str().unwrap()
        );
        let copy_dir = tempfile::tempdir().unwrap();
        let second = copy_dir.path().join("legacy.zip");
        export_icpc_legacy(&manifest, &source, &second).unwrap();
        assert_eq!(first_bytes, fs::read(second).unwrap());

        let imported_dir = temporary.path().join("imported");
        let imported = import_icpc_legacy(&first, &imported_dir).unwrap();
        assert_eq!(imported.digest().unwrap(), manifest.digest().unwrap());
        for file in &manifest.files {
            assert_eq!(
                fs::read(source.join(&file.path)).unwrap(),
                fs::read(imported_dir.join(&file.path)).unwrap()
            );
        }
    }

    #[test]
    fn imports_a_sidecar_free_minimal_legacy_package() {
        let temporary = tempfile::tempdir().unwrap();
        let archive_path = temporary.path().join("external.zip");
        write_external_corpus_archive(&archive_path);
        let destination = temporary.path().join("imported");
        let manifest = import_icpc_legacy(&archive_path, &destination).unwrap();
        assert_eq!(manifest.package_profile, PackageProfile::IcpcLegacy);
        assert_eq!(manifest.problem_type, ProblemType::Standard);
        assert_eq!(manifest.judging.tests.len(), 1);
        assert_eq!(manifest.solutions.len(), 2);
        assert_eq!(
            manifest.project_id.to_string(),
            "019f0000-0000-7000-8000-000000000101"
        );
        assert_eq!(
            manifest.publication.as_ref().unwrap().tags,
            ["implementation", "io"]
        );
        assert!(
            destination
                .join("problem_statement/problem.en.tex")
                .is_file()
        );
    }

    #[test]
    fn rejects_tampered_sidecar_native_payload() {
        let source = corpus();
        let manifest: ReleaseManifestV1 =
            serde_json::from_slice(&fs::read(source.join("reporch.problem.json")).unwrap())
                .unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let archive_path = temporary.path().join("legacy.zip");
        export_icpc_legacy(&manifest, &source, &archive_path).unwrap();
        let tampered_directory = tempfile::tempdir().unwrap();
        let tampered_path = tampered_directory.path().join("legacy.zip");
        let mut source_archive = ZipArchive::new(File::open(&archive_path).unwrap()).unwrap();
        let target = File::create(&tampered_path).unwrap();
        let mut target_archive = ZipWriter::new(target);
        for index in 0..source_archive.len() {
            let mut entry = source_archive.by_index(index).unwrap();
            let name = entry.name().to_owned();
            target_archive
                .start_file(
                    &name,
                    zip_options(entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0)),
                )
                .unwrap();
            if name.ends_with("reporch_legacy/native/00000001.bin") {
                target_archive.write_all(b"tampered\n").unwrap();
            } else {
                std::io::copy(&mut entry, &mut target_archive).unwrap();
            }
        }
        target_archive.finish().unwrap();
        let destination = temporary.path().join("destination");
        assert!(import_icpc_legacy(&tampered_path, &destination).is_err());
        assert!(!destination.exists());
    }

    #[test]
    fn rejects_non_portable_names_and_non_lf_text() {
        for (name, problem_yaml) in [
            (
                "_invalid.in",
                b"problem_format_version: legacy-icpc\nname: Echo\n".as_slice(),
            ),
            (
                "valid.in",
                b"problem_format_version: legacy-icpc\r\nname: Echo\r\n".as_slice(),
            ),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let archive_path = temporary.path().join("external.zip");
            let mut archive = ZipWriter::new(File::create(&archive_path).unwrap());
            archive
                .start_file("external/problem.yaml", zip_options(false))
                .unwrap();
            archive.write_all(problem_yaml).unwrap();
            archive
                .start_file(format!("external/data/secret/{name}"), zip_options(false))
                .unwrap();
            archive.write_all(b"1\n").unwrap();
            archive.finish().unwrap();
            assert!(import_icpc_legacy(&archive_path, &temporary.path().join("imported")).is_err());
        }
    }
}
