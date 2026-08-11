use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use sha2::{Digest, Sha256};
use studio_contracts::{
    NATIVE_RELEASE_PACKAGE_SCHEMA_V1, NATIVE_SOURCE_PACKAGE_SCHEMA_V1,
    NativeReleasePackageMetadataV1, NativeSourcePackageMetadataV1, ValidationReportV1,
    ValidationRunStatus,
};
use studio_core::{
    IssueSeverity, ReleaseManifestV1, Sha256Digest, normalize_relative_path, validate_manifest,
};
use tempfile::Builder;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, System, ZipArchive, ZipWriter};

const SOURCE_METADATA_PATH: &str = "META-INF/reporch-source.json";
const RELEASE_METADATA_PATH: &str = "META-INF/reporch-release.json";
const SOURCE_MANIFEST_PATH: &str = "reporch.problem.json";
const RELEASE_MANIFEST_PATH: &str = "manifest.json";
const VALIDATION_REPORT_PATH: &str = "validation-report.json";
const IMPORT_REPORT_PATH: &str = "reporch.import-report.json";
const MAX_ARCHIVE_FILES: usize = 50_003;
const MAX_ARCHIVE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 64 * 1024;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_VALIDATION_REPORT_BYTES: u64 = 64 * 1024 * 1024;

enum WriteSource {
    Bytes(Vec<u8>),
    File {
        path: PathBuf,
        expected_size: u64,
        expected_digest: Sha256Digest,
    },
}

struct WriteEntry {
    path: String,
    executable: bool,
    source: WriteSource,
}

#[derive(Debug, Clone)]
struct ScannedEntry {
    index: usize,
    raw_name: Vec<u8>,
    path: String,
    size: u64,
    executable: bool,
}

enum NativePackageKind {
    Source {
        metadata: NativeSourcePackageMetadataV1,
    },
    Release {
        metadata: NativeReleasePackageMetadataV1,
        validation_report: Box<ValidationReportV1>,
        validation_report_bytes: Vec<u8>,
        metadata_bytes: Vec<u8>,
    },
}

struct CreatedDirectory {
    path: PathBuf,
    armed: bool,
}

impl CreatedDirectory {
    fn create(path: &Path) -> Result<Self> {
        ensure!(!path.exists(), "import destination already exists");
        if let Some(parent) = nonempty_parent(path) {
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

pub fn export_native(
    manifest: &ReleaseManifestV1,
    source_root: &Path,
    output: &Path,
) -> Result<()> {
    ensure_valid_manifest(manifest)?;
    ensure!(!output.exists(), "export destination already exists");
    let source_root = fs::canonicalize(source_root)
        .with_context(|| format!("resolve source root {}", source_root.display()))?;
    ensure!(source_root.is_dir(), "source root is not a directory");

    let manifest_json = manifest.canonical_json()?;
    ensure!(
        manifest_json.len() as u64 <= MAX_MANIFEST_BYTES,
        "manifest exceeds the 16 MiB native package limit"
    );
    let file_bytes = manifest.files.iter().try_fold(0_u64, |total, file| {
        total
            .checked_add(file.size_bytes)
            .context("manifest file size overflow")
    })?;
    ensure!(
        file_bytes <= MAX_ARCHIVE_BYTES,
        "manifest exceeds the 5 GiB native package limit"
    );
    let metadata = NativeSourcePackageMetadataV1 {
        schema: NATIVE_SOURCE_PACKAGE_SCHEMA_V1.into(),
        manifest_digest: manifest.digest()?,
        source_profile: manifest.package_profile,
        file_count: manifest.files.len() as u64,
        file_bytes,
    };
    let mut entries = vec![
        WriteEntry {
            path: SOURCE_METADATA_PATH.into(),
            executable: false,
            source: WriteSource::Bytes(serde_json::to_vec(&metadata)?),
        },
        WriteEntry {
            path: SOURCE_MANIFEST_PATH.into(),
            executable: false,
            source: WriteSource::Bytes(manifest_json),
        },
    ];
    for manifest_file in &manifest.files {
        ensure!(
            manifest_file.size_bytes <= MAX_ENTRY_BYTES,
            "manifest file exceeds the 1 GiB native package limit: {}",
            manifest_file.path
        );
        let source = source_root.join(&manifest_file.path);
        let link_metadata = fs::symlink_metadata(&source)
            .with_context(|| format!("inspect source file {}", source.display()))?;
        ensure!(
            link_metadata.file_type().is_file() && !link_metadata.file_type().is_symlink(),
            "native package source is not a regular file: {}",
            source.display()
        );
        let canonical_source = fs::canonicalize(&source)
            .with_context(|| format!("resolve source file {}", source.display()))?;
        ensure!(
            canonical_source.starts_with(&source_root),
            "native package source escapes its canonical root: {}",
            source.display()
        );
        entries.push(WriteEntry {
            path: manifest_file.path.clone(),
            executable: manifest_file.executable,
            source: WriteSource::File {
                path: canonical_source,
                expected_size: manifest_file.size_bytes,
                expected_digest: manifest_file.sha256.clone(),
            },
        });
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));

    let output_parent = nonempty_parent(output).unwrap_or_else(|| Path::new("."));
    ensure!(
        output_parent.is_dir(),
        "export destination parent does not exist"
    );
    let mut temporary = Builder::new()
        .prefix(".reporch-native-")
        .tempfile_in(output_parent)
        .with_context(|| format!("create package beside {}", output.display()))?;
    {
        let mut archive = ZipWriter::new(temporary.as_file_mut());
        for entry in entries {
            let size = match &entry.source {
                WriteSource::Bytes(bytes) => bytes.len() as u64,
                WriteSource::File { expected_size, .. } => *expected_size,
            };
            archive.start_file(&entry.path, options(entry.executable, size))?;
            match entry.source {
                WriteSource::Bytes(bytes) => archive.write_all(&bytes)?,
                WriteSource::File {
                    path,
                    expected_size,
                    expected_digest,
                } => write_verified_file(&mut archive, &path, expected_size, &expected_digest)?,
            }
        }
        archive.finish()?.sync_all()?;
    }
    temporary
        .persist_noclobber(output)
        .map_err(|error| error.error)
        .with_context(|| format!("install native package {}", output.display()))?;
    Ok(())
}

pub fn import_native(input: &Path, directory: &Path) -> Result<ReleaseManifestV1> {
    let input_file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut archive = ZipArchive::new(input_file).context("read Reporch Native ZIP")?;
    let entries = scan_archive(&mut archive)?;
    let by_path = entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let source_metadata = by_path.get(SOURCE_METADATA_PATH).copied();
    let release_metadata = by_path.get(RELEASE_METADATA_PATH).copied();
    ensure!(
        source_metadata.is_some() ^ release_metadata.is_some(),
        "native package must contain exactly one source or immutable-release descriptor"
    );

    let (manifest, kind) = if let Some(entry) = source_metadata {
        let metadata_bytes = read_entry(&mut archive, entry, MAX_METADATA_BYTES)?;
        let metadata: NativeSourcePackageMetadataV1 =
            serde_json::from_slice(&metadata_bytes).context("parse native source metadata")?;
        ensure!(
            metadata.schema == NATIVE_SOURCE_PACKAGE_SCHEMA_V1,
            "unsupported native source package schema"
        );
        let manifest_entry = by_path
            .get(SOURCE_MANIFEST_PATH)
            .copied()
            .context("native source package is missing reporch.problem.json")?;
        let manifest_bytes = read_entry(&mut archive, manifest_entry, MAX_MANIFEST_BYTES)?;
        let manifest: ReleaseManifestV1 =
            serde_json::from_slice(&manifest_bytes).context("parse native source manifest")?;
        (manifest, NativePackageKind::Source { metadata })
    } else {
        let entry = release_metadata.expect("exclusive descriptor checked");
        let metadata_bytes = read_entry(&mut archive, entry, MAX_METADATA_BYTES)?;
        let metadata: NativeReleasePackageMetadataV1 =
            serde_json::from_slice(&metadata_bytes).context("parse native release metadata")?;
        ensure!(
            metadata.schema == NATIVE_RELEASE_PACKAGE_SCHEMA_V1,
            "unsupported native release package schema"
        );
        let manifest_entry = by_path
            .get(RELEASE_MANIFEST_PATH)
            .copied()
            .context("native release package is missing manifest.json")?;
        let manifest_bytes = read_entry(&mut archive, manifest_entry, MAX_MANIFEST_BYTES)?;
        let manifest: ReleaseManifestV1 =
            serde_json::from_slice(&manifest_bytes).context("parse native release manifest")?;
        let report_entry = by_path
            .get(VALIDATION_REPORT_PATH)
            .copied()
            .context("native release package is missing validation-report.json")?;
        let validation_report_bytes =
            read_entry(&mut archive, report_entry, MAX_VALIDATION_REPORT_BYTES)?;
        ensure!(
            Sha256Digest::from_bytes(&validation_report_bytes) == metadata.validation_report_digest,
            "native release validation report digest mismatch"
        );
        let validation_report: ValidationReportV1 =
            serde_json::from_slice(&validation_report_bytes)
                .context("parse native release validation report")?;
        (
            manifest,
            NativePackageKind::Release {
                metadata,
                validation_report: Box::new(validation_report),
                validation_report_bytes,
                metadata_bytes,
            },
        )
    };

    ensure_valid_manifest(&manifest)?;
    let manifest_digest = manifest.digest()?;
    match &kind {
        NativePackageKind::Source { metadata } => {
            ensure!(
                metadata.manifest_digest == manifest_digest,
                "native source manifest digest mismatch"
            );
            ensure!(
                metadata.source_profile == manifest.package_profile,
                "native source profile does not match its manifest"
            );
            ensure!(
                metadata.file_count == manifest.files.len() as u64,
                "native source file count mismatch"
            );
            let file_bytes = manifest.files.iter().try_fold(0_u64, |total, file| {
                total
                    .checked_add(file.size_bytes)
                    .context("manifest file size overflow")
            })?;
            ensure!(
                metadata.file_bytes == file_bytes,
                "native source file byte count mismatch"
            );
        }
        NativePackageKind::Release {
            metadata,
            validation_report,
            ..
        } => {
            ensure!(
                metadata.manifest_digest == manifest_digest,
                "native release manifest digest mismatch"
            );
            ensure!(
                metadata.project_id == manifest.project_id
                    && metadata.commit_id == manifest.commit_id,
                "native release identity does not match its manifest"
            );
            ensure!(
                validation_report.schema == "reporch.validation-report.v1"
                    && validation_report.status == ValidationRunStatus::Passed
                    && validation_report.manifest_digest == manifest_digest
                    && validation_report.started_at <= validation_report.finished_at,
                "native release validation evidence is not a passed report for this manifest"
            );
        }
    }

    let mut expected_paths = manifest
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    match &kind {
        NativePackageKind::Source { .. } => {
            expected_paths.insert(SOURCE_METADATA_PATH.into());
            expected_paths.insert(SOURCE_MANIFEST_PATH.into());
        }
        NativePackageKind::Release { .. } => {
            expected_paths.insert(RELEASE_METADATA_PATH.into());
            expected_paths.insert(RELEASE_MANIFEST_PATH.into());
            expected_paths.insert(VALIDATION_REPORT_PATH.into());
        }
    }
    let actual_paths = entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    ensure!(
        actual_paths == expected_paths,
        "native package archive inventory does not exactly match its manifest"
    );
    for file in &manifest.files {
        let entry = by_path
            .get(file.path.as_str())
            .copied()
            .with_context(|| format!("native package is missing {}", file.path))?;
        ensure!(
            entry.size == file.size_bytes && entry.executable == file.executable,
            "native package metadata mismatch for {}",
            file.path
        );
    }

    let destination = CreatedDirectory::create(directory)?;
    let mut files = manifest.files.iter().collect::<Vec<_>>();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    for manifest_file in files {
        let entry = by_path
            .get(manifest_file.path.as_str())
            .copied()
            .expect("inventory checked");
        extract_verified_file(
            &mut archive,
            entry,
            directory,
            manifest_file.size_bytes,
            &manifest_file.sha256,
            manifest_file.executable,
        )?;
    }
    write_new(
        &directory.join(SOURCE_MANIFEST_PATH),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;

    let import_report = match &kind {
        NativePackageKind::Source { metadata } => json!({
            "schema": "reporch.import-report.v1",
            "source_profile": "reporch_native",
            "target_profile": "reporch_native",
            "package_kind": "source",
            "source_schema": metadata.schema,
            "manifest_digest": manifest_digest,
            "lossless": true,
            "losses": [],
            "transformations": []
        }),
        NativePackageKind::Release {
            metadata,
            validation_report_bytes,
            metadata_bytes,
            ..
        } => {
            write_new(&directory.join(RELEASE_METADATA_PATH), metadata_bytes)?;
            write_new(
                &directory.join(VALIDATION_REPORT_PATH),
                validation_report_bytes,
            )?;
            json!({
                "schema": "reporch.import-report.v1",
                "source_profile": "reporch_native",
                "target_profile": "reporch_native",
                "package_kind": "immutable_release",
                "source_schema": metadata.schema,
                "release_id": metadata.release_id,
                "manifest_digest": manifest_digest,
                "validation_report_digest": metadata.validation_report_digest,
                "lossless": true,
                "losses": [],
                "transformations": ["manifest.json is exposed locally as reporch.problem.json"]
            })
        }
    };
    write_new(
        &directory.join(IMPORT_REPORT_PATH),
        &serde_json::to_vec_pretty(&import_report)?,
    )?;
    destination.finish();
    Ok(manifest)
}

fn ensure_valid_manifest(manifest: &ReleaseManifestV1) -> Result<()> {
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
    Ok(())
}

fn scan_archive(archive: &mut ZipArchive<File>) -> Result<Vec<ScannedEntry>> {
    ensure!(
        archive.len() <= MAX_ARCHIVE_FILES,
        "native archive exceeds the {MAX_ARCHIVE_FILES} entry limit"
    );
    let mut paths = BTreeSet::new();
    let mut portable_paths = BTreeSet::new();
    let mut total_size = 0_u64;
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let file = archive.by_index(index)?;
        ensure!(!file.encrypted(), "encrypted ZIP entries are not supported");
        ensure!(
            matches!(
                file.compression(),
                CompressionMethod::Stored | CompressionMethod::Deflated
            ),
            "unsupported ZIP compression method"
        );
        let raw_name =
            std::str::from_utf8(file.name_raw()).context("ZIP entry name is not valid UTF-8")?;
        ensure!(!raw_name.contains('\\'), "ZIP entry uses a backslash path");
        ensure!(
            !file.is_dir(),
            "native packages do not contain directory entries"
        );
        let path = normalize_relative_path(raw_name)
            .with_context(|| format!("unsafe ZIP entry {raw_name:?}"))?;
        ensure!(
            paths.insert(path.clone()),
            "duplicate or Unicode-colliding ZIP entry {path}"
        );
        ensure!(
            portable_paths.insert(path.to_lowercase()),
            "case-colliding ZIP entry is not portable: {path}"
        );
        let file_type = file.unix_mode().unwrap_or_default() & 0o170000;
        ensure!(
            file_type == 0 || file_type == 0o100000,
            "ZIP entry is a symlink or special file: {path}"
        );
        ensure!(
            file.size() <= MAX_ENTRY_BYTES,
            "ZIP entry exceeds the 1 GiB limit: {path}"
        );
        total_size = total_size
            .checked_add(file.size())
            .context("ZIP uncompressed size overflow")?;
        ensure!(
            total_size <= MAX_ARCHIVE_BYTES + MAX_VALIDATION_REPORT_BYTES + MAX_MANIFEST_BYTES,
            "native archive exceeds its uncompressed size limit"
        );
        entries.push(ScannedEntry {
            index,
            raw_name: file.name_raw().to_vec(),
            path,
            size: file.size(),
            executable: file.unix_mode().is_some_and(|mode| mode & 0o111 != 0),
        });
    }
    ensure!(!entries.is_empty(), "native package is empty");
    Ok(entries)
}

fn read_entry(
    archive: &mut ZipArchive<File>,
    entry: &ScannedEntry,
    maximum: u64,
) -> Result<Vec<u8>> {
    ensure!(
        entry.size <= maximum,
        "native package control file is too large"
    );
    let mut source = archive.by_index(entry.index)?;
    ensure!(
        source.name_raw() == entry.raw_name,
        "ZIP central directory changed during import"
    );
    let mut bytes = Vec::with_capacity(entry.size as usize);
    source
        .by_ref()
        .take(entry.size + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == entry.size,
        "ZIP entry size changed during import"
    );
    Ok(bytes)
}

fn extract_verified_file(
    archive: &mut ZipArchive<File>,
    entry: &ScannedEntry,
    directory: &Path,
    expected_size: u64,
    expected_digest: &Sha256Digest,
    executable: bool,
) -> Result<()> {
    let target = directory.join(&entry.path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut source = archive.by_index(entry.index)?;
    ensure!(
        source.name_raw() == entry.raw_name,
        "ZIP central directory changed during extraction"
    );
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .with_context(|| format!("create extracted file {}", target.display()))?;
    let (size, digest) = copy_and_hash(&mut source, &mut output)?;
    ensure!(
        size == expected_size && digest == *expected_digest,
        "native package file digest or size mismatch: {}",
        entry.path
    );
    output.sync_all()?;
    set_executable(&target, executable)?;
    Ok(())
}

fn write_verified_file<W: Write>(
    output: &mut W,
    source: &Path,
    expected_size: u64,
    expected_digest: &Sha256Digest,
) -> Result<()> {
    let mut source_file = File::open(source)?;
    let (size, digest) = copy_and_hash(&mut source_file, output)?;
    ensure!(
        size == expected_size && digest == *expected_digest,
        "native package source digest or size changed: {}",
        source.display()
    );
    Ok(())
}

fn copy_and_hash<R: Read, W: Write>(reader: &mut R, writer: &mut W) -> Result<(u64, Sha256Digest)> {
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("file size overflow")?;
        digest.update(&buffer[..read]);
        writer.write_all(&buffer[..read])?;
    }
    Ok((
        size,
        hex::encode(digest.finalize())
            .parse()
            .context("generated SHA-256 digest is invalid")?,
    ))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}

fn options(executable: bool, size: u64) -> SimpleFileOptions {
    SimpleFileOptions::DEFAULT
        .compression_method(CompressionMethod::Stored)
        .system(System::Unix)
        .unix_permissions(if executable { 0o755 } else { 0o644 })
        .large_file(size >= u32::MAX as u64)
}

fn nonempty_parent(path: &Path) -> Option<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;
    use reporch_cli::init_project_template;
    use studio_core::{NATIVE_PACKAGE_RESERVED_PATHS, ProblemType};
    use tempfile::TempDir;
    use uuid::Uuid;

    fn fixture() -> (TempDir, ReleaseManifestV1) {
        let temporary = TempDir::new().unwrap();
        let source = temporary.path().join("source");
        init_project_template(
            &source,
            "Native round trip",
            Uuid::now_v7(),
            ProblemType::Standard,
        )
        .unwrap();
        let manifest =
            serde_json::from_slice(&fs::read(source.join(SOURCE_MANIFEST_PATH)).unwrap()).unwrap();
        (temporary, manifest)
    }

    #[test]
    fn source_package_is_deterministic_and_round_trips_exactly() {
        let (temporary, manifest) = fixture();
        let source = temporary.path().join("source");
        let first = temporary.path().join("first.zip");
        let second = temporary.path().join("second.zip");
        export_native(&manifest, &source, &first).unwrap();
        export_native(&manifest, &source, &second).unwrap();
        assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());

        let imported_root = temporary.path().join("imported");
        let imported = import_native(&first, &imported_root).unwrap();
        assert_eq!(imported.digest().unwrap(), manifest.digest().unwrap());
        for file in &manifest.files {
            assert_eq!(
                fs::read(source.join(&file.path)).unwrap(),
                fs::read(imported_root.join(&file.path)).unwrap()
            );
        }
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(imported_root.join(IMPORT_REPORT_PATH)).unwrap())
                .unwrap();
        assert_eq!(report["lossless"], true);
        assert_eq!(report["losses"], json!([]));
    }

    #[test]
    fn import_rejects_tampering_without_leaving_a_partial_directory() {
        let (temporary, manifest) = fixture();
        let source = temporary.path().join("source");
        let original = temporary.path().join("original.zip");
        let tampered = temporary.path().join("tampered.zip");
        export_native(&manifest, &source, &original).unwrap();

        let mut input = ZipArchive::new(File::open(&original).unwrap()).unwrap();
        let mut output = ZipWriter::new(File::create(&tampered).unwrap());
        for index in 0..input.len() {
            let mut entry = input.by_index(index).unwrap();
            let name = entry.name().to_owned();
            let executable = entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0);
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if name == "statements/ko.md" {
                bytes.push(b'!');
            }
            output
                .start_file(&name, options(executable, bytes.len() as u64))
                .unwrap();
            output.write_all(&bytes).unwrap();
        }
        output.finish().unwrap();

        let destination = temporary.path().join("rejected");
        let error = import_native(&tampered, &destination).unwrap_err();
        assert!(
            error.to_string().contains("metadata mismatch") || error.to_string().contains("digest")
        );
        assert!(!destination.exists());
    }

    #[test]
    fn export_refuses_overwrite_and_changed_source_bytes() {
        let (temporary, manifest) = fixture();
        let source = temporary.path().join("source");
        let output = temporary.path().join("native.zip");
        fs::write(source.join("statements/ko.md"), "changed").unwrap();
        assert!(export_native(&manifest, &source, &output).is_err());
        assert!(!output.exists());
        fs::write(&output, b"owned").unwrap();
        assert!(export_native(&manifest, &source, &output).is_err());
        assert_eq!(fs::read(output).unwrap(), b"owned");
    }

    #[test]
    fn reserved_archive_paths_are_part_of_manifest_validation() {
        let (_temporary, mut manifest) = fixture();
        manifest.files[0].path = "Manifest.JSON".into();
        let issues = validate_manifest(&manifest);
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "files.native_package_reserved_path")
        );
    }

    #[test]
    fn reserved_path_inventory_constant_stays_in_sync() {
        for required in [
            SOURCE_METADATA_PATH,
            RELEASE_METADATA_PATH,
            SOURCE_MANIFEST_PATH,
            RELEASE_MANIFEST_PATH,
            VALIDATION_REPORT_PATH,
            IMPORT_REPORT_PATH,
        ] {
            assert!(NATIVE_PACKAGE_RESERVED_PATHS.contains(&required));
        }
    }
}
