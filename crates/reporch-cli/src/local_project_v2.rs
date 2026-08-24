use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use reporch_format::{
    AuthoringFileV2, AuthoringSpecV2, parse_authoring_spec_v2, to_authoring_yaml_v2,
};
use studio_core::{ManifestFile, ReleaseManifestV1, ReleaseManifestV2};
use uuid::Uuid;

use crate::local_project::{
    AUTHORING_FILE_NAME, LEGACY_MANIFEST_FILE_NAME, ProjectDiffV1, ProjectStatusV1, RemoteLinkV1,
    atomic_create_new, atomic_replace, ensure_real_directory, hash_regular_project_file,
    reject_non_regular_destination,
};

pub const AUTHORING_V1_BACKUP_FILE_NAME: &str = "reporch.pre-v2.yaml";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MigrationOutcomeV2 {
    pub schema: &'static str,
    pub directory: PathBuf,
    pub authoring_file: PathBuf,
    pub backup_files: Vec<PathBuf>,
    pub project_id: Uuid,
    pub migrated: bool,
}

enum GeneratedManifest {
    V1(ReleaseManifestV1),
    V2(ReleaseManifestV2),
}

pub fn is_v2_project(directory: &Path) -> Result<bool> {
    let root = crate::local_project::discover_project(directory)?;
    let bytes = fs::read(root.join(AUTHORING_FILE_NAME))?;
    Ok(parse_authoring_spec_v2(&bytes).is_ok())
}

pub fn read_authoring_spec(directory: &Path) -> Result<AuthoringSpecV2> {
    let path = directory.join(AUTHORING_FILE_NAME);
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    parse_authoring_spec_v2(&bytes).with_context(|| format!("parse {}", path.display()))
}

pub fn write_authoring_spec_atomic(directory: &Path, spec: &AuthoringSpecV2) -> Result<PathBuf> {
    let root = ensure_real_directory(directory)?;
    let path = root.join(AUTHORING_FILE_NAME);
    reject_non_regular_destination(&path)?;
    let bytes = to_authoring_yaml_v2(spec).context("serialize v2 authoring spec")?;
    atomic_replace(&path, &bytes, 0o644)?;
    Ok(path)
}

pub fn write_authoring_spec_create_new(
    directory: &Path,
    spec: &AuthoringSpecV2,
) -> Result<PathBuf> {
    let root = ensure_real_directory(directory)?;
    let bytes = to_authoring_yaml_v2(spec).context("serialize v2 authoring spec")?;
    let path = root.join(AUTHORING_FILE_NAME);
    atomic_create_new(&path, &bytes)?;
    Ok(path)
}

pub fn update_authoring_spec<F>(directory: &Path, update: F) -> Result<AuthoringSpecV2>
where
    F: FnOnce(&Path, &mut AuthoringSpecV2) -> Result<()>,
{
    let root = crate::local_project::discover_project(directory)?;
    let mut spec = read_authoring_spec(&root)?;
    update(&root, &mut spec)?;
    spec.validate_references()
        .context("updated v2 authoring spec contains invalid references")?;
    write_authoring_spec_atomic(&root, &spec)?;
    Ok(spec)
}

pub fn declare_project_file(
    root: &Path,
    spec: &mut AuthoringSpecV2,
    path: &str,
    media_type: &str,
    executable: bool,
) -> Result<()> {
    let normalized = studio_core::normalize_relative_path(path)?;
    let _ = hash_regular_project_file(root, &normalized)?;
    if let Some(existing) = spec.files.iter_mut().find(|file| file.path == normalized) {
        existing.media_type = media_type.to_owned();
        existing.executable = executable;
    } else {
        spec.files.push(AuthoringFileV2 {
            path: normalized,
            media_type: media_type.to_owned(),
            executable,
        });
        spec.files.sort_by(|left, right| left.path.cmp(&right.path));
    }
    Ok(())
}

pub fn compile_authoring_spec(
    directory: &Path,
    spec: &AuthoringSpecV2,
    commit_id: Uuid,
) -> Result<ReleaseManifestV2> {
    let root = ensure_real_directory(directory)?;
    let files = spec
        .files
        .iter()
        .map(|declared| {
            let (sha256, size_bytes) = hash_regular_project_file(&root, &declared.path)?;
            Ok(ManifestFile {
                path: declared.path.clone(),
                sha256: sha256
                    .parse()
                    .context("parse locally generated SHA-256 digest")?,
                size_bytes,
                media_type: declared.media_type.clone(),
                executable: declared.executable,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    spec.materialize(commit_id, files)
        .context("materialize immutable v2 release manifest")
}

pub fn project_status(directory: &Path) -> Result<ProjectStatusV1> {
    let root = crate::local_project::discover_project(directory)?;
    let spec = read_authoring_spec(&root)?;
    let state = crate::local_project::read_local_state(&root)?;
    let generated = read_generated_manifest(&root)?;
    let generated_project_id = generated.as_ref().map(|manifest| match manifest {
        GeneratedManifest::V1(manifest) => manifest.project_id,
        GeneratedManifest::V2(manifest) => manifest.project_id,
    });
    let generated_commit_id = generated.as_ref().map(|manifest| match manifest {
        GeneratedManifest::V1(manifest) => manifest.commit_id,
        GeneratedManifest::V2(manifest) => manifest.commit_id,
    });
    let commit_id = state.last_commit_id.or_else(|| {
        (generated_project_id == Some(spec.project_id))
            .then_some(generated_commit_id)
            .flatten()
    });
    let manifest = compile_authoring_spec(&root, &spec, commit_id.unwrap_or_else(Uuid::nil))?;
    let working_digest = manifest.digest()?.to_string();
    let baseline_working_digest = match state.baseline_working_digest {
        Some(digest) => Some(digest),
        None => generated
            .as_ref()
            .filter(|_| generated_project_id == Some(spec.project_id))
            .map(|manifest| match manifest {
                GeneratedManifest::V1(manifest) => manifest.digest().map(|value| value.to_string()),
                GeneratedManifest::V2(manifest) => manifest.digest().map(|value| value.to_string()),
            })
            .transpose()?,
    };
    Ok(ProjectStatusV1 {
        schema: "reporch.project-status.v1",
        root,
        project_id: spec.project_id,
        linked: state.remote.is_some(),
        dirty: baseline_working_digest.as_deref() != Some(&working_digest),
        working_digest,
        baseline_working_digest,
        remote: state.remote,
        last_commit_id: state.last_commit_id,
        last_validation_run_id: state.last_validation_run_id,
        last_release_id: state.last_release_id,
    })
}

pub fn project_diff(directory: &Path) -> Result<ProjectDiffV1> {
    let root = crate::local_project::discover_project(directory)?;
    let spec = read_authoring_spec(&root)?;
    let baseline = read_generated_manifest(&root)?;
    let commit_id = baseline
        .as_ref()
        .map_or_else(Uuid::nil, |manifest| match manifest {
            GeneratedManifest::V1(manifest) => manifest.commit_id,
            GeneratedManifest::V2(manifest) => manifest.commit_id,
        });
    let current = compile_authoring_spec(&root, &spec, commit_id)?;
    let baseline_files = baseline
        .as_ref()
        .map(|manifest| match manifest {
            GeneratedManifest::V1(manifest) => manifest_files(&manifest.files),
            GeneratedManifest::V2(manifest) => manifest_files(&manifest.files),
        })
        .unwrap_or_default();
    let current_files = manifest_files(&current.files);
    let baseline_paths: BTreeSet<&str> = baseline_files.keys().copied().collect();
    let current_paths: BTreeSet<&str> = current_files.keys().copied().collect();
    let added = current_paths
        .difference(&baseline_paths)
        .map(|path| (*path).to_owned())
        .collect();
    let removed = baseline_paths
        .difference(&current_paths)
        .map(|path| (*path).to_owned())
        .collect();
    let modified = baseline_paths
        .intersection(&current_paths)
        .filter(|path| baseline_files.get(**path) != current_files.get(**path))
        .map(|path| (*path).to_owned())
        .collect();
    let metadata_changed = baseline.as_ref().is_none_or(|baseline| match baseline {
        GeneratedManifest::V1(_) => true,
        GeneratedManifest::V2(baseline) => {
            let mut baseline = baseline.clone();
            let mut current = current.clone();
            baseline.files.clear();
            current.files.clear();
            baseline != current
        }
    });
    Ok(ProjectDiffV1 {
        schema: "reporch.project-diff.v1",
        root,
        metadata_changed,
        added,
        modified,
        removed,
    })
}

pub fn link_project(directory: &Path, api_url: &str, project_id: Uuid) -> Result<ProjectStatusV1> {
    let root = crate::local_project::discover_project(directory)?;
    let mut spec = read_authoring_spec(&root)?;
    if spec.project_id != project_id {
        spec.project_id = project_id;
        write_authoring_spec_atomic(&root, &spec)?;
    }
    let mut state = crate::local_project::read_local_state(&root)?;
    state.remote = Some(RemoteLinkV1 {
        api_url: api_url.trim_end_matches('/').to_owned(),
        project_id,
    });
    state.baseline_working_digest = None;
    state.base_revision = None;
    state.last_commit_id = None;
    state.last_validation_run_id = None;
    state.last_release_id = None;
    crate::local_project::write_local_state(&root, &state)?;
    project_status(&root)
}

fn manifest_files(files: &[ManifestFile]) -> BTreeMap<&str, (&str, u64, &str, bool)> {
    files
        .iter()
        .map(|file| {
            (
                file.path.as_str(),
                (
                    file.sha256.as_str(),
                    file.size_bytes,
                    file.media_type.as_str(),
                    file.executable,
                ),
            )
        })
        .collect()
}

fn read_generated_manifest(directory: &Path) -> Result<Option<GeneratedManifest>> {
    let path = directory.join(LEGACY_MANIFEST_FILE_NAME);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "generated manifest is larger than 16 MiB"
    );
    let schema = serde_json::from_slice::<serde_json::Value>(&bytes)?
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .context("generated manifest has no schema")?
        .to_owned();
    match schema.as_str() {
        studio_core::RELEASE_MANIFEST_SCHEMA_V1 => {
            Ok(Some(GeneratedManifest::V1(serde_json::from_slice(&bytes)?)))
        }
        studio_core::RELEASE_MANIFEST_SCHEMA_V2 => {
            Ok(Some(GeneratedManifest::V2(serde_json::from_slice(&bytes)?)))
        }
        _ => anyhow::bail!("unsupported generated manifest schema: {schema}"),
    }
}

pub fn write_generated_manifest_atomic(
    directory: &Path,
    manifest: &ReleaseManifestV2,
) -> Result<PathBuf> {
    manifest.validate_references()?;
    let root = ensure_real_directory(directory)?;
    let path = root.join(LEGACY_MANIFEST_FILE_NAME);
    let mut bytes = serde_json::to_vec_pretty(manifest)?;
    bytes.push(b'\n');
    atomic_replace(&path, &bytes, 0o644)?;
    Ok(path)
}

pub fn migrate_v1_authoring_file(directory: &Path) -> Result<AuthoringSpecV2> {
    let root = ensure_real_directory(directory)?;
    let path = root.join(AUTHORING_FILE_NAME);
    let original = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    if let Ok(v2) = parse_authoring_spec_v2(&original) {
        return Ok(v2);
    }
    let v1 = reporch_format::parse_authoring_spec(&original)
        .with_context(|| format!("parse legacy authoring file {}", path.display()))?;
    let v2 = AuthoringSpecV2::migrate_v1(&v1)?;
    let bytes = to_authoring_yaml_v2(&v2)?;
    let reparsed = parse_authoring_spec_v2(&bytes)?;
    ensure!(reparsed == v2, "v2 migration round-trip changed meaning");

    let backup = root.join(AUTHORING_V1_BACKUP_FILE_NAME);
    match atomic_create_new(&backup, &original) {
        Ok(()) => {}
        Err(error) if backup.exists() => {
            let existing = fs::read(&backup)?;
            ensure!(
                existing == original,
                "refusing to overwrite a different v1 authoring backup: {}",
                backup.display()
            );
            let _ = error;
        }
        Err(error) => return Err(error),
    }
    atomic_replace(&path, &bytes, 0o644)?;
    Ok(v2)
}

pub fn migrate_project(directory: &Path) -> Result<MigrationOutcomeV2> {
    let root = ensure_real_directory(directory)?;
    let authoring_path = root.join(AUTHORING_FILE_NAME);
    let had_authoring = authoring_path.exists();
    let was_v2 = if had_authoring {
        let bytes = fs::read(&authoring_path)?;
        parse_authoring_spec_v2(&bytes).is_ok()
    } else {
        false
    };
    let mut backup_files = Vec::new();
    if !had_authoring {
        let legacy = root.join(LEGACY_MANIFEST_FILE_NAME);
        if !legacy.exists() {
            bail!(
                "neither {AUTHORING_FILE_NAME} nor {LEGACY_MANIFEST_FILE_NAME} exists in {}",
                root.display()
            );
        }
        let outcome = crate::local_project::migrate_legacy_project(&root)?;
        if let Some(backup) = outcome.backup_file {
            backup_files.push(backup);
        }
    }
    let spec = migrate_v1_authoring_file(&root)?;
    let v1_backup = root.join(AUTHORING_V1_BACKUP_FILE_NAME);
    if v1_backup.exists() {
        backup_files.push(v1_backup);
    }
    backup_files.sort();
    backup_files.dedup();
    Ok(MigrationOutcomeV2 {
        schema: "reporch.migration-result.v2",
        directory: root,
        authoring_file: authoring_path,
        backup_files,
        project_id: spec.project_id,
        migrated: !was_v2,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use reporch_format::{AUTHORING_SPEC_SCHEMA_V1, AuthoringFileV1, AuthoringSpecV1};
    use studio_core::{CheckerSpec, JudgingSpec, PackageProfile, ProblemType, ResourceLimits};

    use super::*;

    fn write_minimal_v1(directory: &Path) -> AuthoringSpecV1 {
        fs::create_dir_all(directory.join("statements")).unwrap();
        fs::write(directory.join("statements/ko.md"), "# 합\n").unwrap();
        let spec = AuthoringSpecV1 {
            schema: AUTHORING_SPEC_SCHEMA_V1.into(),
            project_id: Uuid::now_v7(),
            problem_type: ProblemType::Standard,
            package_profile: PackageProfile::ReporchNative,
            default_locale: "ko".into(),
            title: BTreeMap::from([("ko".into(), "합".into())]),
            statements: BTreeMap::from([("ko".into(), "statements/ko.md".into())]),
            files: vec![AuthoringFileV1 {
                path: "statements/ko.md".into(),
                media_type: "text/markdown".into(),
                executable: false,
            }],
            toolchains: BTreeMap::new(),
            judging: JudgingSpec {
                limits: ResourceLimits {
                    time_ms: 1_000,
                    memory_mib: 256,
                    output_kib: 65_536,
                },
                checker: CheckerSpec::Token,
                tests: vec![],
                groups: vec![],
                generators: vec![],
                validator_path: None,
                validator_language: None,
                extra_validator_paths: vec![],
                extra_validators: vec![],
                validator_tests: vec![],
                checker_tests: vec![],
                interactor_path: None,
                interactor_language: None,
                grader_path: None,
                grader_language: None,
                harness: None,
            },
            sources: vec![],
            solutions: vec![],
            output_submissions: vec![],
            publication: None,
            policy_version: "studio-policy-v1".into(),
        };
        crate::local_project::write_authoring_spec_create_new(directory, &spec).unwrap();
        spec
    }

    #[test]
    fn v1_file_migrates_once_and_preserves_a_create_only_backup() {
        let temporary = tempfile::tempdir().unwrap();
        let original = write_minimal_v1(temporary.path());
        let migrated = migrate_v1_authoring_file(temporary.path()).unwrap();
        assert_eq!(migrated.project_id, original.project_id);
        assert_eq!(read_authoring_spec(temporary.path()).unwrap(), migrated);
        let backup = fs::read(temporary.path().join(AUTHORING_V1_BACKUP_FILE_NAME)).unwrap();
        assert!(matches!(
            reporch_format::parse_versioned_authoring_spec(&backup).unwrap(),
            reporch_format::VersionedAuthoringSpec::V1(_)
        ));
        assert_eq!(
            migrate_v1_authoring_file(temporary.path()).unwrap(),
            migrated
        );
    }

    #[test]
    fn v2_compiler_hashes_declared_files_and_rejects_inventory_drift() {
        let temporary = tempfile::tempdir().unwrap();
        write_minimal_v1(temporary.path());
        let v2 = migrate_v1_authoring_file(temporary.path()).unwrap();
        let manifest = compile_authoring_spec(temporary.path(), &v2, Uuid::now_v7()).unwrap();
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].path, "statements/ko.md");
    }

    #[test]
    fn project_migration_is_idempotent_after_reaching_v2() {
        let temporary = tempfile::tempdir().unwrap();
        write_minimal_v1(temporary.path());
        assert!(migrate_project(temporary.path()).unwrap().migrated);
        assert!(!migrate_project(temporary.path()).unwrap().migrated);
    }
}
