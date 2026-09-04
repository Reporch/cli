use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use reporch_format::{
    AuthoringFileV2, AuthoringSpecV2, MAX_AUTHORING_SPEC_BYTES, parse_authoring_spec_v2,
    to_authoring_yaml_v2,
};
use studio_core::{ManifestFile, ReleaseManifestV1, ReleaseManifestV2};
use uuid::Uuid;

use crate::local_project::{
    AUTHORING_FILE_NAME, LEGACY_MANIFEST_FILE_NAME, ProjectDiffV1, ProjectStatusV1, RemoteLinkV1,
    atomic_create_new, atomic_replace, ensure_real_directory, hash_regular_project_file,
    read_bounded_regular_file, reject_non_regular_destination,
};

pub const AUTHORING_V1_BACKUP_FILE_NAME: &str = "reporch.pre-v2.yaml";
const AUTHORING_LOCK_FILE_NAME: &str = "authoring.lock";
const AUTHORING_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MigrationOutcomeV2 {
    pub schema: &'static str,
    pub directory: PathBuf,
    pub authoring_file: PathBuf,
    pub backup_files: Vec<PathBuf>,
    pub project_id: Uuid,
    pub migrated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProjectPruneResultV1 {
    pub schema: &'static str,
    pub applied: bool,
    pub inventory_removed: Vec<String>,
    pub files_preserved: Vec<String>,
    pub files_trashed: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trash_directory: Option<PathBuf>,
}

enum GeneratedManifest {
    V1(Box<ReleaseManifestV1>),
    V2(Box<ReleaseManifestV2>),
}

struct AuthoringLock(fs::File);

impl Drop for AuthoringLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

pub fn is_v2_project(directory: &Path) -> Result<bool> {
    let root = crate::local_project::discover_project(directory)?;
    let _lock = acquire_authoring_lock(&root)?;
    let bytes = read_bounded_regular_file(
        &root.join(AUTHORING_FILE_NAME),
        MAX_AUTHORING_SPEC_BYTES as u64,
    )?;
    Ok(matches!(
        reporch_format::parse_versioned_authoring_spec(&bytes)?,
        reporch_format::VersionedAuthoringSpec::V2(_)
    ))
}

pub fn read_authoring_spec(directory: &Path) -> Result<AuthoringSpecV2> {
    let path = directory.join(AUTHORING_FILE_NAME);
    let bytes = read_bounded_regular_file(&path, MAX_AUTHORING_SPEC_BYTES as u64)?;
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
    let _lock = acquire_authoring_lock(&root)?;
    let mut spec = read_authoring_spec(&root)?;
    update(&root, &mut spec)?;
    spec.validate_references()
        .context("updated v2 authoring spec contains invalid references")?;
    write_authoring_spec_atomic(&root, &spec)?;
    Ok(spec)
}

fn acquire_authoring_lock(root: &Path) -> Result<AuthoringLock> {
    let state_directory = root.join(crate::local_project::LOCAL_STATE_DIRECTORY);
    ensure_private_lock_directory(&state_directory)?;
    let path = state_directory.join(AUTHORING_LOCK_FILE_NAME);
    let deadline = Instant::now() + AUTHORING_LOCK_TIMEOUT;
    loop {
        let file = open_authoring_lock(&path)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(AuthoringLock(file)),
            Err(error) if lock_is_contended(&error) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) if lock_is_contended(&error) => {
                bail!("another local authoring update is still in progress")
            }
            Err(error) => return Err(error).context("lock local authoring project"),
        }
    }
}

fn ensure_private_lock_directory(path: &Path) -> Result<()> {
    loop {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
                    "local authoring lock path must be a real directory"
                );
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(path) {
                    Ok(()) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("create local authoring lock directory {}", path.display())
                        });
                    }
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect local authoring lock {}", path.display()));
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn lock_is_contended(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        return matches!(error.raw_os_error(), Some(32 | 33));
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(unix)]
fn open_authoring_lock(path: &Path) -> Result<fs::File> {
    use rustix::fs::{Mode, OFlags, open};
    use std::os::unix::fs::PermissionsExt as _;

    let file = open(
        path,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .context("open local authoring lock without following symlinks")?;
    let file = fs::File::from(file);
    ensure!(file.metadata()?.is_file(), "authoring lock is not a file");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(windows)]
fn open_authoring_lock(path: &Path) -> Result<fs::File> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .context("open local authoring lock")?;
    ensure!(
        file.metadata()?.is_file() && !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "authoring lock is not a regular file"
    );
    Ok(file)
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
    validate_statement_documents(&root, spec)?;
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

pub fn validate_statement_documents(directory: &Path, spec: &AuthoringSpecV2) -> Result<()> {
    let root = ensure_real_directory(directory)?;
    for path in spec.statements.values().chain(spec.tutorials.values()) {
        let declared = spec
            .files
            .iter()
            .find(|file| file.path == *path)
            .with_context(|| format!("Markdown document is not declared: {path}"))?;
        ensure!(
            declared.media_type == "text/markdown" && !declared.executable,
            "Markdown document must be non-executable text/markdown: {path}"
        );
        let bytes = read_bounded_regular_file(&root.join(path), MAX_AUTHORING_SPEC_BYTES as u64)?;
        let markdown = std::str::from_utf8(&bytes)
            .with_context(|| format!("Markdown document is not UTF-8: {path}"))?;
        let assets = studio_core::statement_image_paths(markdown).map_err(|issues| {
            anyhow::anyhow!(
                "statement Markdown is unsafe in {path}: {}",
                issues
                    .iter()
                    .map(|issue| issue.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })?;
        for asset in assets {
            ensure!(
                spec.files.iter().any(|file| file.path == asset),
                "statement image asset is not declared: {asset}; add it to the project before checking or rendering"
            );
            hash_regular_project_file(&root, &asset)
                .with_context(|| format!("statement image asset is missing or unsafe: {asset}"))?;
        }
    }
    Ok(())
}

/// Build the complete structured and Markdown-asset reference graph for V2.
pub fn referenced_project_paths(root: &Path, spec: &AuthoringSpecV2) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    paths.extend(spec.statements.values().cloned());
    paths.extend(spec.tutorials.values().cloned());
    for statement in spec.statements.values().chain(spec.tutorials.values()) {
        let bytes =
            read_bounded_regular_file(&root.join(statement), MAX_AUTHORING_SPEC_BYTES as u64)
                .with_context(|| format!("read Markdown document {statement}"))?;
        let markdown = std::str::from_utf8(&bytes)
            .with_context(|| format!("Markdown document is not UTF-8: {statement}"))?;
        let assets = studio_core::statement_image_paths(markdown).map_err(|issues| {
            anyhow::anyhow!(
                "cannot prune while Markdown references are invalid in {statement}: {}",
                serde_json::to_string(&issues).unwrap_or_default()
            )
        })?;
        paths.extend(assets);
    }
    for test in &spec.testing.tests {
        paths.insert(test.input_file.clone());
        paths.extend(test.answer_file.iter().cloned());
    }
    for generator in &spec.testing.generators {
        paths.insert(generator.program.source_path.clone());
    }
    paths.extend(
        spec.testing
            .validators
            .primary
            .iter()
            .chain(spec.testing.validators.extra.iter())
            .map(|program| program.source_path.clone()),
    );
    paths.extend(
        spec.testing
            .validators
            .unit_tests
            .iter()
            .map(|unit| unit.input_file.clone()),
    );
    if let studio_core::CheckerSpec::Custom { source_path, .. } = &spec.testing.checker.checker {
        paths.insert(source_path.clone());
    }
    for unit in &spec.testing.checker.unit_tests {
        paths.insert(unit.input_file.clone());
        paths.insert(unit.answer_file.clone());
        paths.insert(unit.output_file.clone());
    }
    paths.extend(
        spec.testing
            .solutions
            .iter()
            .map(|solution| solution.program.source_path.clone()),
    );
    if let Some(interactive) = &spec.execution.interactive {
        paths.insert(interactive.interactor.source_path.clone());
        paths.extend(
            interactive
                .unit_tests
                .iter()
                .map(|unit| unit.input_file.clone()),
        );
    }
    if let Some(harness) = &spec.execution.harness {
        paths.extend(harness.interface_files.iter().cloned());
        paths.extend(harness.public_files.iter().cloned());
        paths.extend(harness.private_files.iter().cloned());
        paths.extend(harness.stub_templates.values().cloned());
        for profile in harness.profiles.values() {
            paths.insert(profile.source_path.clone());
            paths.extend(profile.submission_source_path.iter().cloned());
            paths.extend(profile.asset_paths.iter().cloned());
            paths.extend(profile.compile_script.iter().cloned());
            paths.extend(profile.run_script.iter().cloned());
        }
    }
    for submission in &spec.output_submissions {
        paths.extend(submission.outputs.values().cloned());
    }
    if let Some(publication) = &spec.publication {
        for sample in &publication.samples {
            paths.insert(sample.input_file.clone());
            paths.insert(sample.output_file.clone());
        }
    }
    paths
        .into_iter()
        .map(|path| studio_core::normalize_relative_path(&path).map_err(Into::into))
        .collect()
}

/// Remove unreferenced inventory entries under the authoring lock.
///
/// Optional physical deletion is implemented as a recoverable atomic rename
/// into `.reporch/prune-trash`, so an interrupted operation never destroys a
/// file that a concurrent authoring command has just referenced.
pub fn prune_project(
    directory: &Path,
    apply: bool,
    delete_files: bool,
) -> Result<ProjectPruneResultV1> {
    ensure!(apply || !delete_files, "--delete-files requires --apply");
    let root = crate::local_project::discover_project(directory)?;
    let _lock = acquire_authoring_lock(&root)?;
    let mut spec = read_authoring_spec(&root)?;
    let references = referenced_project_paths(&root, &spec)?;
    let candidates = spec
        .files
        .iter()
        .filter(|file| !references.contains(&file.path))
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    if !apply {
        return Ok(ProjectPruneResultV1 {
            schema: "reporch.project-prune-result.v1",
            applied: false,
            inventory_removed: candidates.clone(),
            files_preserved: candidates,
            files_trashed: Vec::new(),
            trash_directory: None,
        });
    }

    let operation_id = Uuid::now_v7();
    let trash_root = root
        .join(crate::local_project::LOCAL_STATE_DIRECTORY)
        .join("prune-trash")
        .join(operation_id.to_string());
    let mut moved = Vec::<(PathBuf, PathBuf)>::new();
    if delete_files {
        let trash_parent = trash_root
            .parent()
            .context("prune trash directory has no parent")?;
        ensure_private_lock_directory(trash_parent)?;
        fs::create_dir(&trash_root)
            .with_context(|| format!("create recoverable prune trash {}", trash_root.display()))?;
        for path in &candidates {
            // Hashing performs the canonical containment, regular-file and
            // symlink-ancestor checks before any rename occurs.
            let _ = hash_regular_project_file(&root, path)?;
        }
        for path in &candidates {
            let source = root.join(path);
            let destination = trash_root.join(path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("create recoverable prune trash {}", parent.display())
                })?;
            }
            if let Err(error) = fs::rename(&source, &destination) {
                for (original, trashed) in moved.iter().rev() {
                    let _ = fs::rename(trashed, original);
                }
                return Err(error).with_context(|| format!("trash unreferenced file {path}"));
            }
            moved.push((source, destination));
        }
    }

    spec.files.retain(|file| references.contains(&file.path));
    if let Err(error) = spec
        .validate_references()
        .context("pruned authoring spec contains invalid references")
        .and_then(|()| write_authoring_spec_atomic(&root, &spec).map(|_| ()))
    {
        for (original, trashed) in moved.iter().rev() {
            if let Some(parent) = original.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::rename(trashed, original);
        }
        return Err(error);
    }
    Ok(ProjectPruneResultV1 {
        schema: "reporch.project-prune-result.v1",
        applied: true,
        inventory_removed: candidates.clone(),
        files_preserved: if delete_files {
            Vec::new()
        } else {
            candidates.clone()
        },
        files_trashed: if delete_files { candidates } else { Vec::new() },
        trash_directory: delete_files.then_some(trash_root),
    })
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
            let mut baseline = (**baseline).clone();
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
        studio_core::RELEASE_MANIFEST_SCHEMA_V1 => Ok(Some(GeneratedManifest::V1(Box::new(
            serde_json::from_slice(&bytes)?,
        )))),
        studio_core::RELEASE_MANIFEST_SCHEMA_V2 => Ok(Some(GeneratedManifest::V2(Box::new(
            serde_json::from_slice(&bytes)?,
        )))),
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
        let legacy_bytes = fs::read(&legacy)?;
        let schema = serde_json::from_slice::<serde_json::Value>(&legacy_bytes)?
            .get("schema")
            .and_then(serde_json::Value::as_str)
            .context("generated manifest has no schema")?
            .to_owned();
        if schema == studio_core::RELEASE_MANIFEST_SCHEMA_V2 {
            let manifest: ReleaseManifestV2 = serde_json::from_slice(&legacy_bytes)?;
            manifest.validate_references()?;
            return Ok(MigrationOutcomeV2 {
                schema: "reporch.migration-result.v2",
                directory: root,
                authoring_file: authoring_path,
                backup_files,
                project_id: manifest.project_id,
                migrated: false,
            });
        } else {
            let outcome = crate::local_project::migrate_legacy_project(&root)?;
            if let Some(backup) = outcome.backup_file {
                backup_files.push(backup);
            }
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

    #[test]
    fn freshly_initialized_v2_project_can_be_linked_without_v1_parsing() {
        let temporary = tempfile::tempdir().unwrap();
        let local_id = Uuid::now_v7();
        let remote_id = Uuid::now_v7();
        crate::init_project_with_id(temporary.path(), "Linked V2", local_id).unwrap();

        let status =
            link_project(temporary.path(), "https://studio.reporch.com/", remote_id).unwrap();

        assert_eq!(status.project_id, remote_id);
        assert!(status.linked);
        assert_eq!(
            read_authoring_spec(temporary.path()).unwrap().project_id,
            remote_id
        );
        assert_eq!(status.remote.unwrap().api_url, "https://studio.reporch.com");
    }

    #[test]
    fn project_prune_is_dry_run_by_default_and_physical_removal_is_recoverable() {
        let temporary = tempfile::tempdir().unwrap();
        write_minimal_v1(temporary.path());
        migrate_v1_authoring_file(temporary.path()).unwrap();
        fs::write(temporary.path().join("unused.txt"), b"preserve me\n").unwrap();
        update_authoring_spec(temporary.path(), |_root, spec| {
            spec.files.push(AuthoringFileV2 {
                path: "unused.txt".into(),
                media_type: "text/plain".into(),
                executable: false,
            });
            Ok(())
        })
        .unwrap();

        let preview = prune_project(temporary.path(), false, false).unwrap();
        assert!(!preview.applied);
        assert_eq!(preview.inventory_removed, vec!["unused.txt"]);
        assert!(temporary.path().join("unused.txt").exists());
        assert!(
            read_authoring_spec(temporary.path())
                .unwrap()
                .files
                .iter()
                .any(|file| file.path == "unused.txt")
        );

        let inventory_only = prune_project(temporary.path(), true, false).unwrap();
        assert!(inventory_only.applied);
        assert_eq!(inventory_only.files_preserved, vec!["unused.txt"]);
        assert!(temporary.path().join("unused.txt").exists());

        update_authoring_spec(temporary.path(), |_root, spec| {
            spec.files.push(AuthoringFileV2 {
                path: "unused.txt".into(),
                media_type: "text/plain".into(),
                executable: false,
            });
            Ok(())
        })
        .unwrap();
        let removed = prune_project(temporary.path(), true, true).unwrap();
        assert_eq!(removed.files_trashed, vec!["unused.txt"]);
        assert!(!temporary.path().join("unused.txt").exists());
        let trash = removed.trash_directory.unwrap();
        assert_eq!(
            fs::read(trash.join("unused.txt")).unwrap(),
            b"preserve me\n"
        );
    }
}
