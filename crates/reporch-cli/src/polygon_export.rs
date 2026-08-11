use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use quick_xml::escape::escape;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use studio_core::{
    CheckerSpec, ExpectedVerdict, IssueSeverity, PackageProfile, ReleaseManifestV1, Sha256Digest,
    compatibility_report, validate_manifest,
};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, System, ZipWriter};

pub const POLYGON_SIDECAR_PATH: &str = "-reporch-polygon/sidecar-v1.json";
pub const POLYGON_REPORT_PATH: &str = "-reporch-polygon/compatibility-v1.json";
pub const POLYGON_SIDECAR_SCHEMA_V1: &str = "reporch.polygon-package-sidecar.v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolygonSidecarV1 {
    pub schema: String,
    pub descriptor_sha256: Sha256Digest,
    pub manifest_digest: Sha256Digest,
    pub manifest: ReleaseManifestV1,
}

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

pub fn export_polygon_package(
    manifest: &ReleaseManifestV1,
    source_root: &Path,
    output: &Path,
) -> Result<()> {
    let blocking = validate_manifest(manifest)
        .into_iter()
        .filter(|issue| issue.severity == IssueSeverity::Error)
        .collect::<Vec<_>>();
    if !blocking.is_empty() {
        bail!(
            "native manifest validation failed: {}",
            serde_json::to_string(&blocking)?
        );
    }
    let report = compatibility_report(manifest, PackageProfile::PolygonCompatible);
    if !report.exportable {
        bail!(
            "Polygon package export is blocked: {}",
            serde_json::to_string(&report)?
        );
    }
    ensure!(
        output.extension().and_then(|value| value.to_str()) == Some("zip"),
        "Polygon package output must use the .zip extension"
    );

    let short_name = output
        .file_stem()
        .and_then(|value| value.to_str())
        .map(sanitize_component)
        .filter(|value| !value.is_empty())
        .context("output filename cannot be represented as a Polygon short name")?;
    let descriptor = problem_xml(manifest, &short_name)?;
    let sidecar = PolygonSidecarV1 {
        schema: POLYGON_SIDECAR_SCHEMA_V1.into(),
        descriptor_sha256: Sha256Digest::from_bytes(&descriptor),
        manifest_digest: manifest.digest()?,
        manifest: manifest.clone(),
    };

    let mut entries = Vec::new();
    let mut occupied = BTreeSet::new();
    push_bytes(
        "problem.xml".into(),
        descriptor,
        false,
        &mut occupied,
        &mut entries,
    )?;
    push_bytes(
        POLYGON_REPORT_PATH.into(),
        serde_json::to_vec_pretty(&report)?,
        false,
        &mut occupied,
        &mut entries,
    )?;
    push_bytes(
        POLYGON_SIDECAR_PATH.into(),
        serde_json::to_vec_pretty(&sidecar)?,
        false,
        &mut occupied,
        &mut entries,
    )?;

    // Preserve every native object at its canonical path. This is the
    // lossless layer used on a Studio round trip; Polygon-shaped aliases below
    // are strictly projections and never become the source of truth.
    for file in &manifest.files {
        push_manifest(
            manifest,
            source_root,
            &file.path,
            file.path.clone(),
            file.executable,
            &mut occupied,
            &mut entries,
        )?;
    }
    for (index, test) in manifest.judging.tests.iter().enumerate() {
        push_manifest(
            manifest,
            source_root,
            &test.input_file,
            format!("-reporch-polygon/tests/{:04}", index + 1),
            false,
            &mut occupied,
            &mut entries,
        )?;
        if let Some(answer) = test.answer_file.as_deref() {
            push_manifest(
                manifest,
                source_root,
                answer,
                format!("-reporch-polygon/tests/{:04}.a", index + 1),
                false,
                &mut occupied,
                &mut entries,
            )?;
        }
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
        archive.start_file(&entry.path, zip_options(entry.executable))?;
        match entry.source {
            EntrySource::Bytes(bytes) => archive.write_all(&bytes)?,
            EntrySource::ManifestFile { source, .. } => {
                let mut source = File::open(source)?;
                std::io::copy(&mut source, &mut archive)?;
            }
        }
    }
    archive.finish()?.sync_all()?;
    Ok(())
}

fn problem_xml(manifest: &ReleaseManifestV1, short_name: &str) -> Result<Vec<u8>> {
    let default_title = manifest
        .title
        .get(&manifest.default_locale)
        .context("default locale has no title")?;
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<problem revision=\"1\" short-name=\"{}\" name=\"{}\">\n",
        attr(short_name),
        attr(default_title)
    ));
    xml.push_str("  <names>\n");
    for (locale, title) in &manifest.title {
        xml.push_str(&format!(
            "    <name language=\"{}\" value=\"{}\"/>\n",
            attr(locale),
            attr(title)
        ));
    }
    xml.push_str("  </names>\n  <judging>\n    <testset name=\"tests\">\n");
    xml.push_str(&format!(
        "      <time-limit>{}</time-limit>\n      <memory-limit>{}</memory-limit>\n      <test-count>{}</test-count>\n",
        manifest.judging.limits.time_ms,
        manifest.judging.limits.memory_mib * 1024 * 1024,
        manifest.judging.tests.len()
    ));
    xml.push_str("      <input-path-pattern>-reporch-polygon/tests/%04d</input-path-pattern>\n");
    xml.push_str(
        "      <answer-path-pattern>-reporch-polygon/tests/%04d.a</answer-path-pattern>\n",
    );
    xml.push_str("      <tests>\n");
    let samples = sample_inputs(manifest);
    for test in &manifest.judging.tests {
        let method = if test.generated_by.is_some() {
            "generated"
        } else {
            "manual"
        };
        xml.push_str(&format!(
            "        <test method=\"{method}\" sample=\"{}\" description=\"{}\"",
            samples.contains(test.input_file.as_str()),
            attr(&test.name)
        ));
        if let Some(group) = test.groups.first() {
            xml.push_str(&format!(" group=\"{}\"", attr(group)));
        }
        if let Some(generator) = test.generated_by.as_deref() {
            xml.push_str(&format!(" generator=\"{}\"", attr(generator)));
        }
        if let Some(seed) = test.seed {
            xml.push_str(&format!(" seed=\"{seed}\""));
        }
        xml.push_str("/>\n");
    }
    xml.push_str("      </tests>\n");
    if !manifest.judging.groups.is_empty() {
        xml.push_str("      <groups>\n");
        for group in &manifest.judging.groups {
            xml.push_str(&format!(
                "        <group name=\"{}\" points=\"{}\" points-policy=\"complete-group\" feedback-policy=\"complete\">\n",
                attr(&group.id), group.points
            ));
            for dependency in &group.depends_on {
                xml.push_str(&format!(
                    "          <dependency group=\"{}\"/>\n",
                    attr(dependency)
                ));
            }
            xml.push_str("        </group>\n");
        }
        xml.push_str("      </groups>\n");
    }
    xml.push_str("    </testset>\n  </judging>\n  <files>\n");
    if let Some(path) = manifest.judging.validator_path.as_deref() {
        xml.push_str(&format!("    <validator path=\"{}\"/>\n", attr(path)));
    }
    for validator in &manifest.judging.extra_validator_paths {
        xml.push_str(&format!(
            "    <extra-validator path=\"{}\"/>\n",
            attr(validator)
        ));
    }
    for validator in &manifest.judging.extra_validators {
        xml.push_str(&format!(
            "    <extra-validator path=\"{}\" language=\"{}\"/>\n",
            attr(&validator.source_path),
            attr(&validator.language)
        ));
    }
    match &manifest.judging.checker {
        CheckerSpec::Custom {
            source_path,
            language,
        } => xml.push_str(&format!(
            "    <checker type=\"custom\" path=\"{}\" language=\"{}\"/>\n",
            attr(source_path),
            attr(language)
        )),
        checker => xml.push_str(&format!(
            "    <checker type=\"standard\" name=\"{}\"/>\n",
            checker_name(checker)
        )),
    }
    if let Some(path) = manifest.judging.interactor_path.as_deref() {
        xml.push_str(&format!("    <interactor path=\"{}\"/>\n", attr(path)));
    }
    for generator in &manifest.judging.generators {
        xml.push_str(&format!(
            "    <generator id=\"{}\" path=\"{}\" language=\"{}\"/>\n",
            attr(&generator.id),
            attr(&generator.source_path),
            attr(&generator.language)
        ));
    }
    xml.push_str("  </files>\n  <solutions>\n");
    let mut main_assigned = false;
    for solution in &manifest.solutions {
        let tag = polygon_solution_tag(solution.expected_verdict, &mut main_assigned);
        xml.push_str(&format!(
            "    <solution name=\"{}\" tag=\"{tag}\" language=\"{}\" path=\"{}\"/>\n",
            attr(&solution.name),
            attr(&solution.language),
            attr(&solution.source_path)
        ));
    }
    xml.push_str("  </solutions>\n  <statements>\n");
    for (locale, path) in &manifest.statements {
        xml.push_str(&format!(
            "    <statement language=\"{}\" path=\"{}\" format=\"markdown\"/>\n",
            attr(locale),
            attr(path)
        ));
    }
    xml.push_str("  </statements>\n</problem>\n");
    Ok(xml.into_bytes())
}

fn sample_inputs(manifest: &ReleaseManifestV1) -> BTreeSet<&str> {
    manifest
        .publication
        .iter()
        .flat_map(|publication| publication.samples.iter())
        .map(|sample| sample.input_file.as_str())
        .collect()
}

fn checker_name(checker: &CheckerSpec) -> &'static str {
    match checker {
        CheckerSpec::Exact => "exact",
        CheckerSpec::Token => "token",
        CheckerSpec::CaseInsensitive => "case-insensitive",
        CheckerSpec::Floating { .. } => "floating",
        CheckerSpec::Custom { .. } => unreachable!(),
    }
}

fn polygon_solution_tag(verdict: ExpectedVerdict, main_assigned: &mut bool) -> &'static str {
    match verdict {
        ExpectedVerdict::Accepted if !*main_assigned => {
            *main_assigned = true;
            "MA"
        }
        ExpectedVerdict::Accepted => "OK",
        ExpectedVerdict::WrongAnswer => "WA",
        ExpectedVerdict::TimeLimit => "TL",
        ExpectedVerdict::MemoryLimit => "ML",
        ExpectedVerdict::RuntimeError => "RE",
        ExpectedVerdict::Partial => "RJ",
    }
}

fn push_bytes(
    path: String,
    bytes: Vec<u8>,
    executable: bool,
    occupied: &mut BTreeSet<String>,
    entries: &mut Vec<ExportEntry>,
) -> Result<()> {
    ensure!(
        occupied.insert(path.clone()),
        "duplicate export path {path}"
    );
    entries.push(ExportEntry {
        path,
        executable,
        source: EntrySource::Bytes(bytes),
    });
    Ok(())
}

fn push_manifest(
    manifest: &ReleaseManifestV1,
    source_root: &Path,
    source_path: &str,
    target_path: String,
    executable: bool,
    occupied: &mut BTreeSet<String>,
    entries: &mut Vec<ExportEntry>,
) -> Result<()> {
    ensure!(
        occupied.insert(target_path.clone()),
        "duplicate export path {target_path}"
    );
    let file = manifest
        .files
        .iter()
        .find(|file| file.path == source_path)
        .with_context(|| format!("manifest file is missing: {source_path}"))?;
    entries.push(ExportEntry {
        path: target_path,
        executable,
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
            .context("file size overflow")?;
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

fn attr(value: &str) -> String {
    escape(value).into_owned()
}

fn sanitize_component(value: &str) -> String {
    value
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

fn zip_options(executable: bool) -> SimpleFileOptions {
    SimpleFileOptions::DEFAULT
        .compression_method(CompressionMethod::Stored)
        .system(System::Unix)
        .unix_permissions(if executable { 0o755 } else { 0o644 })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn golden_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../evals/corpus/polygon-basic")
    }

    pub(crate) fn fixture() -> (tempfile::TempDir, ReleaseManifestV1) {
        let temporary = tempfile::tempdir().unwrap();
        super::super::init_project(temporary.path(), "Polygon Fixture").unwrap();
        let mut manifest =
            super::super::read_manifest(&temporary.path().join("reporch.problem.json")).unwrap();
        let path = "validators/main.py";
        let bytes = b"raise SystemExit(0)\n";
        let invalid_path = "validators/invalid.in";
        let invalid = b"invalid\n";
        std::fs::create_dir_all(temporary.path().join("validators")).unwrap();
        std::fs::write(temporary.path().join(path), bytes).unwrap();
        std::fs::write(temporary.path().join(invalid_path), invalid).unwrap();
        manifest.files.push(studio_core::ManifestFile {
            path: path.into(),
            sha256: Sha256Digest::from_bytes(bytes),
            size_bytes: bytes.len() as u64,
            media_type: "text/x-python".into(),
            executable: true,
        });
        manifest.files.push(studio_core::ManifestFile {
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
        (temporary, manifest)
    }

    #[test]
    fn exports_deterministic_descriptor_and_lossless_sidecar() {
        let (temporary, manifest) = fixture();
        let first = temporary.path().join("polygon.zip");
        let replay_dir = temporary.path().join("replay");
        std::fs::create_dir(&replay_dir).unwrap();
        let second = replay_dir.join("polygon.zip");

        export_polygon_package(&manifest, temporary.path(), &first).unwrap();
        export_polygon_package(&manifest, temporary.path(), &second).unwrap();

        assert_eq!(
            std::fs::read(&first).unwrap(),
            std::fs::read(&second).unwrap()
        );
        let mut archive = zip::ZipArchive::new(File::open(first).unwrap()).unwrap();
        let paths = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_owned())
            .collect::<BTreeSet<_>>();
        assert!(paths.contains("problem.xml"));
        assert!(paths.contains(POLYGON_SIDECAR_PATH));
        assert!(paths.contains("-reporch-polygon/tests/0001"));
        assert!(paths.contains("-reporch-polygon/tests/0001.a"));
        assert!(paths.contains("validators/main.py"));
        let mut descriptor = String::new();
        archive
            .by_name("problem.xml")
            .unwrap()
            .read_to_string(&mut descriptor)
            .unwrap();
        assert!(descriptor.contains("<solution name=\"accepted\" tag=\"MA\""));
        assert!(descriptor.contains("<solution name=\"known-wrong\" tag=\"WA\""));
    }

    #[test]
    fn rejects_changed_source_bytes_before_creating_an_archive() {
        let (temporary, manifest) = fixture();
        std::fs::write(
            temporary.path().join("solutions/accepted.py"),
            b"tampered\n",
        )
        .unwrap();
        let output = temporary.path().join("polygon.zip");

        let error = export_polygon_package(&manifest, temporary.path(), &output).unwrap_err();

        assert!(error.to_string().contains("source file size changed"));
        assert!(!output.exists());
    }

    #[test]
    fn preserves_scored_groups_dependencies_and_test_membership() {
        let (temporary, mut manifest) = fixture();
        manifest.problem_type = studio_core::ProblemType::Scored;
        manifest.judging.groups = vec![studio_core::TestGroupSpec {
            id: "full".into(),
            points: 100.0,
            depends_on: vec![],
            feedback_policy: studio_core::GroupFeedbackPolicyV1::Complete,
        }];
        manifest.judging.tests[0].groups = vec!["full".into()];
        let output = temporary.path().join("scored.zip");

        export_polygon_package(&manifest, temporary.path(), &output).unwrap();

        let mut archive = zip::ZipArchive::new(File::open(output).unwrap()).unwrap();
        let mut descriptor = String::new();
        archive
            .by_name("problem.xml")
            .unwrap()
            .read_to_string(&mut descriptor)
            .unwrap();
        assert!(descriptor.contains("<test method=\"manual\" sample=\"true\""));
        assert!(descriptor.contains("group=\"full\""));
        assert!(descriptor.contains("<group name=\"full\" points=\"100\""));
    }

    #[test]
    fn fixed_golden_corpus_locks_manifest_descriptor_and_round_trip() {
        let root = golden_root();
        let manifest = super::super::read_manifest(&root.join("reporch.problem.json")).unwrap();
        assert_eq!(
            manifest.digest().unwrap().as_str(),
            "fbbb21d4ef63770c6aa47b5095ff8192206eb1d4ac3b0ddd88defdf662b3ac6f"
        );
        let temporary = tempfile::tempdir().unwrap();
        let output = temporary.path().join("golden.zip");
        export_polygon_package(&manifest, &root, &output).unwrap();
        let mut archive = zip::ZipArchive::new(File::open(&output).unwrap()).unwrap();
        let mut descriptor = Vec::new();
        archive
            .by_name("problem.xml")
            .unwrap()
            .read_to_end(&mut descriptor)
            .unwrap();
        assert_eq!(
            Sha256Digest::from_bytes(&descriptor).as_str(),
            "e8a4a2f7bc968de59f3dec55287e100e598537ba103ce784e5f14ccc858c78a6"
        );
        drop(archive);

        let imported = super::super::polygon_import::import_polygon_package(
            &output,
            &temporary.path().join("imported"),
        )
        .unwrap();
        assert_eq!(imported.digest().unwrap(), manifest.digest().unwrap());
    }
}
