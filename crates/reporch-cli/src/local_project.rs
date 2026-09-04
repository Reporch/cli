use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use reporch_format::{
    AuthoringSpecV1, MAX_AUTHORING_SPEC_BYTES, parse_authoring_spec, to_authoring_yaml,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use studio_core::{ManifestFile, ReleaseManifestV1, validate_manifest};
use tempfile::NamedTempFile;
use uuid::Uuid;

pub const AUTHORING_FILE_NAME: &str = "reporch.yaml";
pub const LEGACY_MANIFEST_FILE_NAME: &str = "reporch.problem.json";
pub const LEGACY_BACKUP_FILE_NAME: &str = "reporch.problem.pre-1.0.json";
pub const LOCAL_STATE_DIRECTORY: &str = ".reporch";
pub const LOCAL_STATE_FILE_NAME: &str = "state.json";
const MAX_LEGACY_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LOCAL_STATE_BYTES: u64 = 1024 * 1024;
const ATOMIC_REPLACE_RETRY_TIMEOUT: Duration = Duration::from_secs(2);
const ATOMIC_REPLACE_RETRY_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteLinkV1 {
    pub api_url: String,
    pub project_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalStateV1 {
    pub schema: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteLinkV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_working_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commit_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_validation_run_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_release_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pending_idempotency_keys: BTreeMap<String, String>,
}

impl Default for LocalStateV1 {
    fn default() -> Self {
        Self {
            schema: "reporch.local-state.v1".into(),
            remote: None,
            base_revision: None,
            baseline_working_digest: None,
            last_commit_id: None,
            last_validation_run_id: None,
            last_release_id: None,
            pending_idempotency_keys: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectStatusV1 {
    pub schema: &'static str,
    pub root: PathBuf,
    pub project_id: Uuid,
    pub linked: bool,
    pub dirty: bool,
    pub working_digest: String,
    pub baseline_working_digest: Option<String>,
    pub remote: Option<RemoteLinkV1>,
    pub last_commit_id: Option<Uuid>,
    pub last_validation_run_id: Option<Uuid>,
    pub last_release_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectDiffV1 {
    pub schema: &'static str,
    pub root: PathBuf,
    pub metadata_changed: bool,
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationOutcome {
    pub schema: &'static str,
    pub directory: PathBuf,
    pub authoring_file: PathBuf,
    pub backup_file: Option<PathBuf>,
    pub project_id: Uuid,
    pub migrated: bool,
}

pub fn read_authoring_spec(directory: &Path) -> Result<AuthoringSpecV1> {
    let path = directory.join(AUTHORING_FILE_NAME);
    let bytes = read_bounded_regular_file(&path, MAX_AUTHORING_SPEC_BYTES as u64)?;
    parse_authoring_spec(&bytes).with_context(|| format!("parse {}", path.display()))
}

pub fn discover_project(start: &Path) -> Result<PathBuf> {
    let mut current = fs::canonicalize(start)
        .with_context(|| format!("resolve working directory {}", start.display()))?;
    if current.is_file() {
        current = current
            .parent()
            .context("working path has no parent directory")?
            .to_path_buf();
    }
    loop {
        if current.join(AUTHORING_FILE_NAME).is_file() {
            return Ok(current);
        }
        if !current.pop() {
            bail!(
                "no {AUTHORING_FILE_NAME} found from {} or its parents",
                start.display()
            );
        }
    }
}

pub fn read_local_state(directory: &Path) -> Result<LocalStateV1> {
    let path = local_state_path(directory);
    if !path.exists() {
        return Ok(LocalStateV1::default());
    }
    let bytes = read_bounded_regular_file(&path, MAX_LOCAL_STATE_BYTES)?;
    let state: LocalStateV1 =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    ensure!(
        state.schema == "reporch.local-state.v1",
        "unsupported local state schema: {}",
        state.schema
    );
    Ok(state)
}

pub fn write_local_state(directory: &Path, state: &LocalStateV1) -> Result<PathBuf> {
    ensure!(
        state.schema == "reporch.local-state.v1",
        "unsupported local state schema: {}",
        state.schema
    );
    let root = ensure_real_directory(directory)?;
    let state_directory = root.join(LOCAL_STATE_DIRECTORY);
    ensure_private_state_directory(&state_directory)?;
    let path = state_directory.join(LOCAL_STATE_FILE_NAME);
    let mut bytes = serde_json::to_vec_pretty(state)?;
    bytes.push(b'\n');
    atomic_replace(&path, &bytes, 0o600)?;
    Ok(path)
}

pub fn project_status(directory: &Path) -> Result<ProjectStatusV1> {
    let root = discover_project(directory)?;
    let spec = read_authoring_spec(&root)?;
    let state = read_local_state(&root)?;
    let generated_manifest_path = root.join(LEGACY_MANIFEST_FILE_NAME);
    let generated_manifest = if generated_manifest_path.exists() {
        let bytes = read_bounded_regular_file(&generated_manifest_path, MAX_LEGACY_MANIFEST_BYTES)?;
        Some(
            serde_json::from_slice::<ReleaseManifestV1>(&bytes)
                .with_context(|| format!("parse {}", generated_manifest_path.display()))?,
        )
    } else {
        None
    };
    let commit_id = state.last_commit_id.or_else(|| {
        generated_manifest
            .as_ref()
            .filter(|manifest| manifest.project_id == spec.project_id)
            .map(|manifest| manifest.commit_id)
    });
    let commit_id = commit_id.unwrap_or_else(Uuid::nil);
    let manifest = compile_authoring_spec(&root, &spec, commit_id)?;
    let working_digest = manifest.digest()?.to_string();
    let baseline_working_digest = match state.baseline_working_digest {
        Some(digest) => Some(digest),
        None => generated_manifest
            .as_ref()
            .filter(|manifest| manifest.project_id == spec.project_id)
            .map(ReleaseManifestV1::digest)
            .transpose()?
            .map(|digest| digest.to_string()),
    };
    let dirty = baseline_working_digest.as_deref() != Some(&working_digest);
    Ok(ProjectStatusV1 {
        schema: "reporch.project-status.v1",
        root,
        project_id: spec.project_id,
        linked: state.remote.is_some(),
        dirty,
        working_digest,
        baseline_working_digest,
        remote: state.remote,
        last_commit_id: state.last_commit_id,
        last_validation_run_id: state.last_validation_run_id,
        last_release_id: state.last_release_id,
    })
}

pub fn project_diff(directory: &Path) -> Result<ProjectDiffV1> {
    let root = discover_project(directory)?;
    let spec = read_authoring_spec(&root)?;
    let baseline_path = root.join(LEGACY_MANIFEST_FILE_NAME);
    let baseline = if baseline_path.exists() {
        let bytes = read_bounded_regular_file(&baseline_path, MAX_LEGACY_MANIFEST_BYTES)?;
        Some(
            serde_json::from_slice::<ReleaseManifestV1>(&bytes)
                .with_context(|| format!("parse {}", baseline_path.display()))?,
        )
    } else {
        None
    };
    let commit_id = baseline
        .as_ref()
        .map_or_else(Uuid::nil, |manifest| manifest.commit_id);
    let current = compile_authoring_spec(&root, &spec, commit_id)?;

    let baseline_files: BTreeMap<&str, (&str, u64, &str, bool)> = baseline
        .as_ref()
        .map(|manifest| {
            manifest
                .files
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
        })
        .unwrap_or_default();
    let current_files: BTreeMap<&str, (&str, u64, &str, bool)> = current
        .files
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
        .collect();
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

    let metadata_changed = baseline.as_ref().is_none_or(|baseline| {
        let mut current = current.clone();
        let mut baseline = baseline.clone();
        current.files.clear();
        baseline.files.clear();
        current != baseline
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
    let root = discover_project(directory)?;
    let mut spec = read_authoring_spec(&root)?;
    if spec.project_id != project_id {
        spec.project_id = project_id;
        write_authoring_spec_atomic(&root, &spec)?;
    }
    let mut state = read_local_state(&root)?;
    state.remote = Some(RemoteLinkV1 {
        api_url: api_url.trim_end_matches('/').to_owned(),
        project_id,
    });
    state.baseline_working_digest = None;
    state.base_revision = None;
    state.last_commit_id = None;
    state.last_validation_run_id = None;
    state.last_release_id = None;
    write_local_state(&root, &state)?;
    project_status(&root)
}

pub fn write_authoring_spec_atomic(directory: &Path, spec: &AuthoringSpecV1) -> Result<PathBuf> {
    let root = ensure_real_directory(directory)?;
    let path = root.join(AUTHORING_FILE_NAME);
    reject_non_regular_destination(&path)?;
    let bytes = to_authoring_yaml(spec).context("serialize authoring spec")?;
    atomic_replace(&path, &bytes, 0o644)?;
    Ok(path)
}

pub fn update_authoring_spec<F>(directory: &Path, update: F) -> Result<AuthoringSpecV1>
where
    F: FnOnce(&Path, &mut AuthoringSpecV1) -> Result<()>,
{
    let root = discover_project(directory)?;
    let mut spec = read_authoring_spec(&root)?;
    update(&root, &mut spec)?;
    spec.validate_references()
        .context("updated authoring spec contains invalid references")?;
    write_authoring_spec_atomic(&root, &spec)?;
    Ok(spec)
}

pub fn declare_project_file(
    root: &Path,
    spec: &mut AuthoringSpecV1,
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
        spec.files.push(reporch_format::AuthoringFileV1 {
            path: normalized,
            media_type: media_type.to_owned(),
            executable,
        });
        spec.files.sort_by(|left, right| left.path.cmp(&right.path));
    }
    Ok(())
}

pub fn write_generated_manifest_atomic(
    directory: &Path,
    manifest: &ReleaseManifestV1,
) -> Result<PathBuf> {
    manifest.validate_references()?;
    let issues = validate_manifest(manifest);
    ensure!(
        issues.is_empty(),
        "refusing to persist an invalid generated manifest: {}",
        serde_json::to_string(&issues)?
    );
    let root = ensure_real_directory(directory)?;
    let path = root.join(LEGACY_MANIFEST_FILE_NAME);
    let mut bytes = serde_json::to_vec_pretty(manifest)?;
    bytes.push(b'\n');
    atomic_replace(&path, &bytes, 0o644)?;
    Ok(path)
}

pub fn write_authoring_spec_create_new(
    directory: &Path,
    spec: &AuthoringSpecV1,
) -> Result<PathBuf> {
    let directory = ensure_real_directory(directory)?;
    let bytes = to_authoring_yaml(spec).context("serialize authoring spec")?;
    let destination = directory.join(AUTHORING_FILE_NAME);
    atomic_create_new(&destination, &bytes)?;
    Ok(destination)
}

pub fn compile_authoring_spec(
    directory: &Path,
    spec: &AuthoringSpecV1,
    commit_id: Uuid,
) -> Result<ReleaseManifestV1> {
    validate_statement_documents(directory, spec)?;
    let files = hash_declared_files(directory, spec)?;
    let manifest = spec
        .materialize(commit_id, files)
        .context("materialize immutable release manifest")?;
    let issues = validate_manifest(&manifest);
    if !issues.is_empty() {
        bail!(
            "authoring spec failed static validation: {}",
            serde_json::to_string(&issues)?
        );
    }
    Ok(manifest)
}

pub fn validate_statement_documents(directory: &Path, spec: &AuthoringSpecV1) -> Result<()> {
    let root = ensure_real_directory(directory)?;
    for path in spec.statements.values() {
        let declared = spec
            .files
            .iter()
            .find(|file| file.path == *path)
            .with_context(|| format!("Markdown document is not declared: {path}"))?;
        ensure!(
            declared.media_type == "text/markdown" && !declared.executable,
            "Markdown document must be non-executable text/markdown: {path}"
        );
        hash_regular_project_file(&root, path)?;
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

pub fn migrate_legacy_project(directory: &Path) -> Result<MigrationOutcome> {
    let directory = ensure_real_directory(directory)?;
    let legacy_path = directory.join(LEGACY_MANIFEST_FILE_NAME);
    let authoring_path = directory.join(AUTHORING_FILE_NAME);
    let backup_path = directory.join(LEGACY_BACKUP_FILE_NAME);

    let legacy_bytes = read_bounded_regular_file(&legacy_path, MAX_LEGACY_MANIFEST_BYTES)?;
    let manifest: ReleaseManifestV1 = serde_json::from_slice(&legacy_bytes)
        .with_context(|| format!("parse legacy manifest {}", legacy_path.display()))?;
    let issues = validate_manifest(&manifest);
    if !issues.is_empty() {
        bail!(
            "legacy manifest failed static validation: {}",
            serde_json::to_string(&issues)?
        );
    }

    let spec = AuthoringSpecV1::from_manifest(&manifest);
    let yaml = to_authoring_yaml(&spec).context("serialize migrated authoring spec")?;
    let parsed = parse_authoring_spec(&yaml).context("verify migrated authoring spec")?;
    let reconstructed = parsed
        .materialize(manifest.commit_id, manifest.files.clone())
        .context("reconstruct legacy manifest from migrated authoring spec")?;
    ensure!(
        reconstructed == manifest,
        "migration changed the meaning of the legacy manifest"
    );
    verify_manifest_files(&directory, &manifest)?;

    if authoring_path.exists() {
        let existing = read_authoring_spec(&directory)?;
        ensure!(
            existing == spec,
            "{} already exists with different content",
            authoring_path.display()
        );
        return Ok(MigrationOutcome {
            schema: "reporch.migration-result.v1",
            directory,
            authoring_file: authoring_path,
            backup_file: backup_path.exists().then_some(backup_path),
            project_id: manifest.project_id,
            migrated: false,
        });
    }

    create_or_verify_backup(&backup_path, &legacy_bytes)?;
    atomic_create_new(&authoring_path, &yaml)?;

    Ok(MigrationOutcome {
        schema: "reporch.migration-result.v1",
        directory,
        authoring_file: authoring_path,
        backup_file: Some(backup_path),
        project_id: manifest.project_id,
        migrated: true,
    })
}

fn hash_declared_files(directory: &Path, spec: &AuthoringSpecV1) -> Result<Vec<ManifestFile>> {
    let root = ensure_real_directory(directory)?;
    spec.files
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
        .collect()
}

fn verify_manifest_files(directory: &Path, manifest: &ReleaseManifestV1) -> Result<()> {
    for expected in &manifest.files {
        let (actual_sha256, actual_size) = hash_regular_project_file(directory, &expected.path)?;
        ensure!(
            actual_size == expected.size_bytes,
            "legacy file size mismatch: {} (expected {}, got {})",
            expected.path,
            expected.size_bytes,
            actual_size
        );
        ensure!(
            actual_sha256 == expected.sha256.as_str(),
            "legacy file digest mismatch: {}",
            expected.path
        );
    }
    Ok(())
}

pub(crate) fn hash_regular_project_file(root: &Path, relative_path: &str) -> Result<(String, u64)> {
    studio_core::validate_relative_path(relative_path)?;
    let source = root.join(relative_path);
    let metadata = fs::symlink_metadata(&source)
        .with_context(|| format!("inspect project file {relative_path}"))?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "project file is not a regular file (a non-symlink is required): {relative_path}"
    );
    let canonical = fs::canonicalize(&source)
        .with_context(|| format!("resolve project file {relative_path}"))?;
    ensure!(
        canonical.starts_with(root) && canonical == source,
        "project file resolves through a symlink or outside the project: {relative_path}"
    );

    let mut file =
        File::open(&canonical).with_context(|| format!("open project file {relative_path}"))?;
    let initial_size = file.metadata()?.len();
    let mut actual_size = 0_u64;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read project file {relative_path}"))?;
        if read == 0 {
            break;
        }
        actual_size = actual_size
            .checked_add(read as u64)
            .context("project file size overflow")?;
        ensure!(
            actual_size <= initial_size,
            "project file grew while hashing: {relative_path}"
        );
        hasher.update(&buffer[..read]);
    }
    ensure!(
        actual_size == initial_size,
        "project file size changed while hashing: {relative_path}"
    );
    Ok((hex::encode(hasher.finalize()), actual_size))
}

fn create_or_verify_backup(path: &Path, expected: &[u8]) -> Result<()> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(expected)?;
            file.sync_all()?;
            sync_parent(path)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = read_bounded_regular_file(path, MAX_LEGACY_MANIFEST_BYTES)?;
            ensure!(
                existing == expected,
                "refusing to overwrite a different migration backup: {}",
                path.display()
            );
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("create backup {}", path.display())),
    }
}

pub(crate) fn atomic_create_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("destination has no parent directory")?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary file in {}", parent.display()))?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("create {} without overwrite", path.display()))?;
    sync_parent(path)?;
    Ok(())
}

pub(crate) fn atomic_replace(path: &Path, bytes: &[u8], unix_mode: u32) -> Result<()> {
    reject_non_regular_destination(path)?;
    let parent = path
        .parent()
        .context("destination has no parent directory")?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary file in {}", parent.display()))?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(unix_mode))?;
    }
    #[cfg(not(unix))]
    let _ = unix_mode;
    let deadline = Instant::now() + ATOMIC_REPLACE_RETRY_TIMEOUT;
    loop {
        match temporary.persist(path) {
            Ok(_) => break,
            Err(error) => {
                let retryable =
                    transient_atomic_replace_error(&error.error) && Instant::now() < deadline;
                temporary = error.file;
                if retryable {
                    std::thread::sleep(ATOMIC_REPLACE_RETRY_INTERVAL);
                    continue;
                }
                return Err(error.error)
                    .with_context(|| format!("atomically replace {}", path.display()));
            }
        }
    }
    sync_parent(path)?;
    Ok(())
}

fn transient_atomic_replace_error(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        error.kind() == std::io::ErrorKind::PermissionDenied
            || matches!(error.raw_os_error(), Some(5 | 32 | 33))
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

pub(crate) fn reject_non_regular_destination(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "refusing to replace a non-regular file: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    Ok(())
}

fn ensure_private_state_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
            "local state path must be a real directory"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)
                .with_context(|| format!("create local state directory {}", path.display()))?;
        }
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn local_state_path(directory: &Path) -> PathBuf {
    directory
        .join(LOCAL_STATE_DIRECTORY)
        .join(LOCAL_STATE_FILE_NAME)
}

pub fn read_bounded_regular_file(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "expected a regular file: {}",
        path.display()
    );
    ensure!(
        metadata.len() <= maximum,
        "file is too large: {} (maximum {} bytes)",
        path.display(),
        maximum
    );
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= maximum && bytes.len() as u64 == metadata.len(),
        "file changed or exceeded its bound while being read: {}",
        path.display()
    );
    Ok(bytes)
}

pub(crate) fn ensure_real_directory(directory: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("inspect project directory {}", directory.display()))?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "project directory must be a real directory"
    );
    fs::canonicalize(directory)
        .with_context(|| format!("resolve project directory {}", directory.display()))
}

fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path.parent().context("path has no parent directory")?;
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_template::init_legacy_v1_project_template;

    fn init_project_with_id(directory: &Path, title: &str, project_id: Uuid) -> Result<()> {
        init_legacy_v1_project_template(
            directory,
            title,
            project_id,
            studio_core::ProblemType::Standard,
        )
    }

    fn legacy_project() -> tempfile::TempDir {
        let temporary = tempfile::tempdir().unwrap();
        init_project_with_id(temporary.path(), "Migration", Uuid::now_v7()).unwrap();
        fs::remove_file(temporary.path().join(AUTHORING_FILE_NAME)).unwrap();
        temporary
    }

    #[test]
    fn migrates_without_changing_manifest_meaning_or_hashes() {
        let temporary = legacy_project();
        let legacy: ReleaseManifestV1 = serde_json::from_slice(
            &fs::read(temporary.path().join(LEGACY_MANIFEST_FILE_NAME)).unwrap(),
        )
        .unwrap();

        let outcome = migrate_legacy_project(temporary.path()).unwrap();
        assert!(outcome.migrated);
        assert_eq!(
            fs::read(temporary.path().join(LEGACY_BACKUP_FILE_NAME)).unwrap(),
            fs::read(temporary.path().join(LEGACY_MANIFEST_FILE_NAME)).unwrap()
        );
        let spec = read_authoring_spec(temporary.path()).unwrap();
        assert_eq!(
            spec.materialize(legacy.commit_id, legacy.files.clone())
                .unwrap(),
            legacy
        );
    }

    #[test]
    fn migration_is_idempotent_but_never_overwrites_a_backup() {
        let temporary = legacy_project();
        assert!(migrate_legacy_project(temporary.path()).unwrap().migrated);
        assert!(!migrate_legacy_project(temporary.path()).unwrap().migrated);

        fs::remove_file(temporary.path().join(AUTHORING_FILE_NAME)).unwrap();
        fs::write(temporary.path().join(LEGACY_BACKUP_FILE_NAME), b"different").unwrap();
        let error = migrate_legacy_project(temporary.path()).unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
    }

    #[test]
    fn compiler_hashes_current_files_and_rejects_symlinks() {
        let temporary = tempfile::tempdir().unwrap();
        init_project_with_id(temporary.path(), "Compile", Uuid::now_v7()).unwrap();
        let spec = read_authoring_spec(temporary.path()).unwrap();
        let manifest = compile_authoring_spec(temporary.path(), &spec, Uuid::now_v7()).unwrap();
        assert!(validate_manifest(&manifest).is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let statement = temporary.path().join("statements/ko.md");
            let copy = temporary.path().join("statement-copy.md");
            fs::copy(&statement, &copy).unwrap();
            fs::remove_file(&statement).unwrap();
            symlink(&copy, &statement).unwrap();
            assert!(
                compile_authoring_spec(temporary.path(), &spec, Uuid::now_v7())
                    .unwrap_err()
                    .to_string()
                    .contains("not a regular file")
            );
        }
    }

    #[test]
    fn state_contains_linkage_but_never_credentials() {
        let temporary = tempfile::tempdir().unwrap();
        let project_id = Uuid::now_v7();
        init_project_with_id(temporary.path(), "State", project_id).unwrap();
        let status =
            link_project(temporary.path(), "https://studio.reporch.com/", project_id).unwrap();
        assert!(status.linked);
        let bytes = fs::read(temporary.path().join(".reporch").join("state.json")).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("token"));
        assert_eq!(
            read_local_state(temporary.path()).unwrap().remote,
            Some(RemoteLinkV1 {
                api_url: "https://studio.reporch.com".into(),
                project_id,
            })
        );
    }

    #[test]
    fn status_and_diff_detect_content_changes_without_a_new_commit_id() {
        let temporary = tempfile::tempdir().unwrap();
        init_project_with_id(temporary.path(), "Diff", Uuid::now_v7()).unwrap();
        let initial = project_status(temporary.path()).unwrap();
        assert!(!initial.dirty);
        let initial_diff = project_diff(temporary.path()).unwrap();
        assert!(!initial_diff.metadata_changed);
        assert!(initial_diff.modified.is_empty());

        fs::write(temporary.path().join("statements/ko.md"), b"# Changed\n").unwrap();
        assert!(project_status(temporary.path()).unwrap().dirty);
        let changed = project_diff(temporary.path()).unwrap();
        assert_eq!(changed.modified, vec!["statements/ko.md"]);
    }
}
