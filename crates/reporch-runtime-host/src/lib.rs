#![deny(unsafe_code)]

#[cfg(target_os = "macos")]
mod apple_backend;
mod host_version;
#[cfg(windows)]
mod windows_identity;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use futures_util::StreamExt as _;
use minisign_verify::{PublicKey, Signature};
use reporch_runtime_core::{
    DOCTOR_SCHEMA, HostTarget, INSTALLATION_SCHEMA, RuntimeArtifactKindV1, RuntimeAvailability,
    RuntimeBackend, RuntimeBundleManifestV1, RuntimeDoctorCheckV1, RuntimeDoctorV1, RuntimeError,
    RuntimeInstallationV1, RuntimeStatusV1, RuntimeUpdateV1, STATUS_SCHEMA,
    TOOLCHAIN_INSTALLATION_SCHEMA, ToolchainBundleV2, ToolchainCompressionV2, ToolchainEntryV2,
    ToolchainIndexV2, ToolchainInstallationV2,
};
use reporch_runtime_protocol::{
    GuestJobV1, GuestResultV1, HostChallengeV1, InputChunkV1, MAX_CONTENT_CHUNK_BYTES,
    PROTOCOL_VERSION, RuntimeServiceCommandV1, RuntimeServiceRequestV1, RuntimeServiceResultV1,
    SERVICE_REQUEST_SCHEMA, WireMessageV1, read_service_response, read_wire_message,
    read_wire_message_sync, write_service_request, write_wire_message, write_wire_message_sync,
};
use serde::Serialize;
use serde_bytes::ByteBuf;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use uuid::Uuid;

#[cfg(any(target_os = "linux", windows))]
use reporch_runtime_protocol::RuntimeServiceResponseV1;

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_SIGNATURE_BYTES: usize = 16 * 1024;
const MAX_TOOLCHAIN_INDEX_BYTES: usize = 512 * 1024;
const GUEST_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const GUEST_IO_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const RUNTIME_CHANNEL_BASE: &str =
    "https://github.com/Reporch/cli/releases/download/reporch-runtime-v1-seq24";
const TOOLCHAIN_CHANNEL_BASE: &str =
    "https://github.com/Reporch/cli/releases/download/reporch-toolchains-v2-seq8";
const RUNTIME_PUBLIC_KEY: &str = include_str!("../../../artifacts/runtime-v1.minisign.pub");

pub fn runtime_root() -> Result<PathBuf> {
    if let Some(override_path) = std::env::var_os("REPORCH_RUNTIME_HOME") {
        let path = PathBuf::from(override_path);
        anyhow::ensure!(path.is_absolute(), "REPORCH_RUNTIME_HOME must be absolute");
        return Ok(path);
    }
    #[cfg(target_os = "linux")]
    {
        let system = PathBuf::from("/var/lib/reporch-runtime/runtime");
        if system.join("current.json").is_file() {
            return Ok(system);
        }
    }
    #[cfg(windows)]
    if let Some(program_data) = std::env::var_os("PROGRAMDATA") {
        let system = PathBuf::from(program_data).join("Reporch").join("Runtime");
        if system.join("current.json").is_file() {
            return Ok(system);
        }
    }
    let path = if cfg!(target_os = "windows") {
        PathBuf::from(required_env("LOCALAPPDATA")?)
            .join("Reporch")
            .join("Runtime")
    } else if cfg!(target_os = "macos") {
        PathBuf::from(required_env("HOME")?)
            .join("Library")
            .join("Application Support")
            .join("Reporch")
            .join("Runtime")
    } else if let Some(value) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(value).join("reporch").join("runtime")
    } else {
        PathBuf::from(required_env("HOME")?)
            .join(".local")
            .join("share")
            .join("reporch")
            .join("runtime")
    };
    Ok(path)
}

#[cfg(unix)]
pub fn service_socket_path() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("REPORCH_RUNTIME_SERVICE_SOCKET") {
        let value = PathBuf::from(value);
        anyhow::ensure!(
            value.is_absolute(),
            "runtime service socket override must be absolute"
        );
        return Ok(value);
    }
    #[cfg(target_os = "linux")]
    return Ok(PathBuf::from("/run/reporch-runtime/service-v1.sock"));
    #[cfg(not(target_os = "linux"))]
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from("/run/user").join(rustix::process::getuid().as_raw().to_string())
        });
    #[cfg(not(target_os = "linux"))]
    Ok(root.join("reporch-runtime").join("service-v1.sock"))
}

#[cfg(unix)]
pub fn service_spool_root() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    let root = PathBuf::from("/run/user").join(rustix::process::getuid().as_raw().to_string());
    #[cfg(not(target_os = "linux"))]
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from("/run/user").join(rustix::process::getuid().as_raw().to_string())
        });
    Ok(root.join("reporch-runtime").join("spool"))
}

#[cfg(windows)]
pub fn service_spool_root() -> Result<PathBuf> {
    Ok(runtime_root()?.join("spool"))
}

#[cfg(windows)]
pub fn service_pipe_name() -> Result<String> {
    let value = std::env::var("REPORCH_RUNTIME_SERVICE_PIPE")
        .unwrap_or_else(|_| r"\\.\pipe\reporch-runtime-v1".into());
    anyhow::ensure!(
        value.starts_with(r"\\.\pipe\")
            && (value.len() <= 256)
            && value[r"\\.\pipe\".len()..]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "runtime service pipe name is invalid"
    );
    Ok(value)
}

pub async fn status() -> Result<RuntimeStatusV1> {
    status_at(&runtime_root()?).await
}

pub async fn doctor() -> Result<RuntimeDoctorV1> {
    let root = runtime_root()?;
    let status = status_at(&root).await?;
    let asset_check = match read_installation(&root)? {
        Some(installation) => {
            let verification_root = root.clone();
            tokio::task::spawn_blocking(move || {
                verify_installed_bundle_at(&verification_root, &installation)
            })
            .await
            .context("join runtime bundle verification")?
        }
        None => Err(RuntimeError::BootstrapIncomplete.into()),
    };
    let mut checks = vec![RuntimeDoctorCheckV1 {
        id: "runtime_directory".into(),
        passed: root.parent().is_some_and(Path::exists),
        repairable: true,
        message: format!("Runtime directory: {}", root.display()),
    }];
    checks.push(RuntimeDoctorCheckV1 {
        id: "runtime_installation".into(),
        passed: status.installed_version.is_some(),
        repairable: true,
        message: status.installed_version.as_ref().map_or_else(
            || "Base runtime is not installed".into(),
            |version| format!("Base runtime {version} is installed"),
        ),
    });
    checks.push(RuntimeDoctorCheckV1 {
        id: "runtime_assets".into(),
        passed: asset_check.is_ok(),
        repairable: true,
        message: asset_check.err().map_or_else(
            || "Runtime assets match the signed manifest".into(),
            |error| format!("Runtime asset verification failed: {error:#}"),
        ),
    });
    checks.push(RuntimeDoctorCheckV1 {
        id: "hardware_virtualization".into(),
        passed: status.virtualization_available,
        repairable: false,
        message: if status.virtualization_available {
            "Hardware virtualization is available".into()
        } else {
            status
                .reason
                .clone()
                .unwrap_or_else(|| "Hardware virtualization is unavailable".into())
        },
    });
    checks.push(RuntimeDoctorCheckV1 {
        id: "runtime_service".into(),
        passed: status.service_available,
        repairable: true,
        message: if status.service_available {
            "Runtime host control path is available".into()
        } else {
            "Runtime service is not installed or not responding".into()
        },
    });
    Ok(RuntimeDoctorV1 {
        schema: DOCTOR_SCHEMA.into(),
        status,
        checks,
    })
}

pub async fn verify_installed() -> Result<RuntimeInstallationV1> {
    let root = runtime_root()?;
    let installation =
        read_installation(&root)?.ok_or(reporch_runtime_core::RuntimeError::BootstrapIncomplete)?;
    let verification_root = root.clone();
    let verification = installation.clone();
    tokio::task::spawn_blocking(move || {
        verify_installed_bundle_at(&verification_root, &verification)
    })
    .await
    .context("join runtime bundle verification")??;
    Ok(installation)
}

/// Imports a native-installer seed into the per-user runtime before any
/// network bootstrap. The seed is never trusted because of its install
/// location: its manifest signature, declared contents, modes, and target are
/// verified again after the copy, and `current.json` is committed last.
pub async fn bootstrap_packaged_seed() -> Result<bool> {
    let root = runtime_root()?;
    let Some(seed) = packaged_seed_path()? else {
        return Ok(false);
    };
    bootstrap_packaged_seed_to(&seed, &root).await
}

/// Imports one installer-owned seed into an explicit narrow runtime root.
/// Privileged brokers use this during first service start so package installs
/// never need to download or execute an unverified bootstrap helper.
pub async fn bootstrap_packaged_seed_to(seed: &Path, root: &Path) -> Result<bool> {
    ensure_runtime_root_is_narrow(root)?;
    anyhow::ensure!(
        seed.is_absolute() && root.is_absolute(),
        "packaged runtime seed and destination must be absolute"
    );
    if read_installation(root)?.is_some() {
        return Ok(false);
    }
    if !seed.join("current.json").is_file() {
        return Ok(false);
    }
    let lock_root = root.to_owned();
    let _installation_lock = acquire_installation_lock(&lock_root).await?;
    let seed = seed.to_owned();
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || import_packaged_seed_at(&seed, &root))
        .await
        .context("join packaged runtime seed import")?
}

#[derive(Debug, Clone)]
pub struct VerifiedRuntimeBundleV1 {
    pub installation: RuntimeInstallationV1,
    pub manifest: RuntimeBundleManifestV1,
    pub directory: PathBuf,
}

impl VerifiedRuntimeBundleV1 {
    pub fn artifact_path(&self, kind: RuntimeArtifactKindV1) -> Result<PathBuf> {
        let artifact = self
            .manifest
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == kind)
            .with_context(|| format!("verified runtime bundle is missing {kind:?}"))?;
        Ok(self.directory.join(&artifact.file_name))
    }
}

pub async fn verified_bundle() -> Result<VerifiedRuntimeBundleV1> {
    let root = runtime_root()?;
    let installation = read_installation(&root)?.ok_or(RuntimeError::BootstrapIncomplete)?;
    let verification_root = root.clone();
    let verification = installation.clone();
    let manifest = tokio::task::spawn_blocking(move || {
        verify_installed_bundle_at(&verification_root, &verification)
    })
    .await
    .context("join runtime bundle verification")??;
    let directory = bundle_directory(&root, installation.sequence, &installation.version);
    Ok(VerifiedRuntimeBundleV1 {
        installation,
        manifest,
        directory,
    })
}

#[derive(Debug, Clone)]
pub struct VerifiedToolchainBundleV2 {
    pub installation: ToolchainInstallationV2,
    pub entry: ToolchainEntryV2,
    pub bundle: ToolchainBundleV2,
    pub path: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolchainIndexStateV2 {
    schema: String,
    sequence: u64,
    sha256: String,
}

pub async fn install_toolchain(id: &str) -> Result<VerifiedToolchainBundleV2> {
    validate_toolchain_id(id)?;
    #[cfg(any(target_os = "linux", windows))]
    if probe_runtime_service().await {
        request_service_toolchain_install(id).await?;
        return verified_toolchain(id).await;
    }
    install_toolchain_direct(id).await
}

pub async fn install_toolchain_direct(id: &str) -> Result<VerifiedToolchainBundleV2> {
    validate_toolchain_id(id)?;
    let target = HostTarget::current()
        .ok_or_else(|| RuntimeError::VirtualizationUnavailable("unsupported host target".into()))?;
    let runtime_root = runtime_root()?;
    let _installation_lock = acquire_installation_lock(&runtime_root).await?;
    let root = runtime_root.join("toolchains");
    ensure_runtime_root_is_narrow(&root)?;
    create_private_directory(&root)?;
    let base = toolchain_channel_url()?;
    let client = runtime_download_client()?;
    let index_url = base
        .join("toolchains-v2-index.json")
        .context("build toolchain index URL")?;
    let signature_url = base
        .join("toolchains-v2-index.json.minisig")
        .context("build toolchain signature URL")?;
    let index_bytes = fetch_small(&client, index_url.as_str(), MAX_TOOLCHAIN_INDEX_BYTES).await?;
    let signature_bytes = fetch_small(&client, signature_url.as_str(), MAX_SIGNATURE_BYTES).await?;
    verify_signature(&index_bytes, &signature_bytes)?;
    let index: ToolchainIndexV2 =
        serde_json::from_slice(&index_bytes).context("parse signed toolchain index")?;
    index.validate(Utc::now()).map_err(anyhow::Error::from)?;
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(&index_bytes)));
    enforce_toolchain_index_monotonicity(&root, index.sequence, &digest)?;
    let (entry, bundle) = index
        .entry_for_target(id, target)
        .map_err(anyhow::Error::from)?;

    if let Ok(existing) = verified_toolchain_at(&root, id, target)
        && existing.installation.index_sequence == index.sequence
        && existing.installation.bundle_sha256 == bundle.sha256
        && existing.installation.toolchain_lock_sha256 == entry.toolchain_lock_sha256
    {
        repair_toolchain_read_access(&existing)?;
        return verified_toolchain_at(&root, id, target);
    }

    let safe_digest = bundle
        .sha256
        .strip_prefix("sha256:")
        .context("toolchain bundle digest is invalid")?;
    let destination = root
        .join("bundles")
        .join(id)
        .join(format!("{}-{safe_digest}", index.sequence));
    ensure_runtime_child(&root, &destination)?;
    if destination.exists() {
        let metadata = fs::symlink_metadata(&destination)
            .context("inspect existing toolchain bundle directory")?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "toolchain bundle directory is invalid"
        );
    } else {
        let parent = destination
            .parent()
            .context("toolchain bundle directory has no parent")?;
        create_private_directory(parent)?;
        let staging = root.join(format!(".toolchain-install-{}", Uuid::now_v7()));
        create_private_directory(&staging)?;
        let install = async {
            let archive = staging.join(&bundle.archive_file_name);
            let artifact = staging.join(&bundle.file_name);
            download_cached_verified(
                &client,
                &bundle.source_url,
                &root,
                &archive,
                bundle.archive_size,
                &bundle.archive_sha256,
            )
            .await?;
            expand_toolchain_archive(
                &archive,
                &artifact,
                bundle.compression,
                bundle.size,
                &bundle.sha256,
            )?;
            fs::remove_file(&archive).context("remove expanded toolchain archive link")?;
            set_toolchain_read_only(&artifact)?;
            fs::write(staging.join("index.json"), &index_bytes)
                .context("write installed toolchain index")?;
            fs::write(staging.join("index.json.minisig"), &signature_bytes)
                .context("write installed toolchain signature")?;
            set_toolchain_read_only(&staging.join("index.json"))?;
            set_toolchain_read_only(&staging.join("index.json.minisig"))?;
            Result::<()>::Ok(())
        }
        .await;
        if let Err(error) = install {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        if let Err(error) = fs::rename(&staging, &destination) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error).context("atomically install toolchain bundle");
        }
    }

    let installation = ToolchainInstallationV2 {
        schema: TOOLCHAIN_INSTALLATION_SCHEMA.into(),
        index_sequence: index.sequence,
        id: id.into(),
        target,
        toolchain_lock_sha256: entry.toolchain_lock_sha256.clone(),
        bundle_sha256: bundle.sha256.clone(),
        file_name: bundle.file_name.clone(),
        installed_at: Utc::now(),
    };
    write_json_atomic(&toolchain_state_path(&root, id), &installation)?;
    write_json_atomic(
        &root.join("index-state.json"),
        &ToolchainIndexStateV2 {
            schema: "reporch.toolchain-index-state.v2".into(),
            sequence: index.sequence,
            sha256: digest,
        },
    )?;
    verified_toolchain_at(&root, id, target)
}

fn expand_toolchain_archive(
    archive: &Path,
    output: &Path,
    compression: ToolchainCompressionV2,
    expected_size: u64,
    expected_digest: &str,
) -> Result<()> {
    anyhow::ensure!(!output.exists(), "toolchain image output already exists");
    let archive_metadata = fs::symlink_metadata(archive).context("inspect toolchain archive")?;
    anyhow::ensure!(
        archive_metadata.is_file() && !archive_metadata.file_type().is_symlink(),
        "toolchain archive must be a regular non-symlink file"
    );
    let result = (|| -> Result<()> {
        let archive = fs::File::open(archive).context("open toolchain archive")?;
        let reader = BufReader::new(archive);
        let mut decoder = match compression {
            ToolchainCompressionV2::Zstd => ruzstd::decoding::StreamingDecoder::new(reader)
                .context("initialize toolchain zstd decoder")?,
        };
        let mut output_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)
            .context("create expanded toolchain image")?;
        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        // Windows services use a 1 MiB default main-thread stack. This code
        // also runs inside that current-thread service executor, so a 1 MiB
        // stack buffer can abort the broker while installing a toolchain.
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let read = decoder
                .read(&mut buffer)
                .context("decompress toolchain image")?;
            if read == 0 {
                break;
            }
            size = size
                .checked_add(read as u64)
                .context("toolchain image size overflow")?;
            anyhow::ensure!(
                size <= expected_size,
                "toolchain archive exceeded its signed expanded size"
            );
            hasher.update(&buffer[..read]);
            write_sparse(&mut output_file, &buffer[..read])?;
        }
        anyhow::ensure!(size == expected_size, "toolchain image size mismatch");
        let digest = format!("sha256:{}", hex::encode(hasher.finalize()));
        anyhow::ensure!(digest == expected_digest, "toolchain image digest mismatch");
        output_file
            .sync_data()
            .context("sync sparse toolchain image extents")?;
        drop(output_file);
        let output_file = fs::OpenOptions::new()
            .write(true)
            .open(output)
            .context("reopen sparse toolchain image")?;
        set_sparse_len(&output_file, expected_size)?;
        output_file
            .sync_all()
            .context("sync expanded toolchain image")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(output);
    }
    result
}

fn write_sparse(output: &mut fs::File, bytes: &[u8]) -> Result<()> {
    for block in bytes.chunks(64 * 1024) {
        if block.iter().all(|byte| *byte == 0) {
            output
                .seek(SeekFrom::Current(i64::try_from(block.len())?))
                .context("create sparse toolchain image extent")?;
        } else {
            output
                .write_all(block)
                .context("write expanded toolchain image")?;
        }
    }
    Ok(())
}

fn set_sparse_len(output: &fs::File, length: u64) -> Result<()> {
    #[cfg(unix)]
    rustix::fs::ftruncate(output, length).context("finalize sparse toolchain image size")?;
    #[cfg(not(unix))]
    output
        .set_len(length)
        .context("finalize sparse toolchain image size")?;
    Ok(())
}

pub async fn verified_toolchain(id: &str) -> Result<VerifiedToolchainBundleV2> {
    validate_toolchain_id(id)?;
    let target = HostTarget::current()
        .ok_or_else(|| RuntimeError::VirtualizationUnavailable("unsupported host target".into()))?;
    verified_toolchain_at(&runtime_root()?.join("toolchains"), id, target)
}

fn verified_toolchain_at(
    root: &Path,
    id: &str,
    target: HostTarget,
) -> Result<VerifiedToolchainBundleV2> {
    let state_path = toolchain_state_path(root, id);
    let installation_bytes =
        read_bounded_regular(&state_path, 64 * 1024, "toolchain installation state")?;
    let installation: ToolchainInstallationV2 = serde_json::from_slice(&installation_bytes)
        .context("parse toolchain installation state")?;
    installation.validate().map_err(anyhow::Error::from)?;
    anyhow::ensure!(
        installation.id == id && installation.target == target,
        "toolchain installation state does not match this request"
    );
    let digest = installation
        .bundle_sha256
        .strip_prefix("sha256:")
        .context("installed toolchain digest is invalid")?;
    let directory = root
        .join("bundles")
        .join(id)
        .join(format!("{}-{digest}", installation.index_sequence));
    ensure_runtime_child(root, &directory)?;
    let metadata =
        fs::symlink_metadata(&directory).context("inspect installed toolchain bundle directory")?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "installed toolchain bundle directory is invalid"
    );
    let index_bytes = read_bounded_regular(
        &directory.join("index.json"),
        MAX_TOOLCHAIN_INDEX_BYTES as u64,
        "installed toolchain index",
    )?;
    let signature_bytes = read_bounded_regular(
        &directory.join("index.json.minisig"),
        MAX_SIGNATURE_BYTES as u64,
        "installed toolchain signature",
    )?;
    verify_signature(&index_bytes, &signature_bytes)?;
    let index: ToolchainIndexV2 =
        serde_json::from_slice(&index_bytes).context("parse installed toolchain index")?;
    index.validate(Utc::now()).map_err(anyhow::Error::from)?;
    anyhow::ensure!(
        index.sequence == installation.index_sequence,
        "installed toolchain index sequence changed"
    );
    let (entry, bundle) = index
        .entry_for_target(id, target)
        .map_err(anyhow::Error::from)?;
    anyhow::ensure!(
        entry.toolchain_lock_sha256 == installation.toolchain_lock_sha256
            && bundle.sha256 == installation.bundle_sha256
            && bundle.file_name == installation.file_name,
        "installed toolchain state no longer matches its signed index"
    );
    let path = directory.join(&installation.file_name);
    let metadata = fs::symlink_metadata(&path).context("inspect installed toolchain image")?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() == bundle.size,
        "installed toolchain image type or size changed"
    );
    anyhow::ensure!(
        hash_regular_file(&path, bundle.size)? == bundle.sha256,
        "installed toolchain image digest changed"
    );
    Ok(VerifiedToolchainBundleV2 {
        installation,
        entry: entry.clone(),
        bundle: bundle.clone(),
        path,
    })
}

fn enforce_toolchain_index_monotonicity(root: &Path, sequence: u64, digest: &str) -> Result<()> {
    let path = root.join("index-state.json");
    let state = match fs::read(&path) {
        Ok(bytes) => Some(
            serde_json::from_slice::<ToolchainIndexStateV2>(&bytes)
                .context("parse toolchain index state")?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("read toolchain index state"),
    };
    if let Some(state) = state {
        anyhow::ensure!(
            state.schema == "reporch.toolchain-index-state.v2"
                && state.sequence <= sequence
                && (state.sequence != sequence || state.sha256 == digest),
            "toolchain channel attempted a rollback or sequence reuse"
        );
    }
    Ok(())
}

fn toolchain_state_path(root: &Path, id: &str) -> PathBuf {
    root.join("installed").join(format!("{id}.json"))
}

fn validate_toolchain_id(id: &str) -> Result<()> {
    anyhow::ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'-' | b'_')
            }),
        "invalid toolchain ID"
    );
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RuntimeSpoolReceiptV1 {
    pub schema: &'static str,
    pub object_count: u32,
    pub total_bytes: u64,
    pub reused_objects: u32,
}

#[cfg(any(target_os = "linux", windows))]
async fn request_service_runtime_update(force: bool) -> Result<RuntimeUpdateV1> {
    let request = RuntimeServiceRequestV1 {
        schema: SERVICE_REQUEST_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        id: Uuid::now_v7(),
        command: RuntimeServiceCommandV1::UpdateRuntime { force },
    };
    let response = call_runtime_service(&request, DOWNLOAD_TOTAL_TIMEOUT).await?;
    match response.result {
        RuntimeServiceResultV1::RuntimeUpdated {
            previous_version,
            installed_version,
            sequence,
            target,
            repaired,
        } => {
            let host_target = HostTarget::current().ok_or_else(|| {
                RuntimeError::VirtualizationUnavailable("unsupported host target".into())
            })?;
            anyhow::ensure!(
                target == target_name(host_target),
                "runtime broker returned a different host target"
            );
            Ok(RuntimeUpdateV1 {
                schema: "reporch.runtime-update.v1".into(),
                previous_version,
                installed_version,
                sequence,
                target: host_target,
                repaired,
            })
        }
        RuntimeServiceResultV1::Error(error) => Err(RuntimeError::AssetVerificationFailed(
            format!("{}: {}", error.error_code, error.message),
        )
        .into()),
        _ => Err(RuntimeError::ProtocolIncompatible.into()),
    }
}

#[cfg(any(target_os = "linux", windows))]
async fn request_service_toolchain_install(id: &str) -> Result<()> {
    let request = RuntimeServiceRequestV1 {
        schema: SERVICE_REQUEST_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        id: Uuid::now_v7(),
        command: RuntimeServiceCommandV1::InstallToolchain { id: id.into() },
    };
    let response = call_runtime_service(&request, DOWNLOAD_TOTAL_TIMEOUT).await?;
    match response.result {
        RuntimeServiceResultV1::ToolchainInstalled {
            id: installed_id,
            index_sequence,
            bundle_sha256,
        } => {
            anyhow::ensure!(
                installed_id == id && index_sequence > 0 && bundle_sha256.starts_with("sha256:"),
                "runtime broker returned a different toolchain identity"
            );
            Ok(())
        }
        RuntimeServiceResultV1::Error(error) => Err(RuntimeError::AssetVerificationFailed(
            format!("{}: {}", error.error_code, error.message),
        )
        .into()),
        _ => Err(RuntimeError::ProtocolIncompatible.into()),
    }
}

#[cfg(target_os = "linux")]
async fn call_runtime_service(
    request: &RuntimeServiceRequestV1,
    timeout: Duration,
) -> Result<RuntimeServiceResponseV1> {
    request.validate()?;
    let mut stream = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::net::UnixStream::connect(service_socket_path()?),
    )
    .await
    .map_err(|_| RuntimeError::ServiceUnavailable("connection timed out".into()))?
    .map_err(|error| RuntimeError::ServiceUnavailable(error.to_string()))?;
    tokio::time::timeout(PROBE_TIMEOUT, write_service_request(&mut stream, request))
        .await
        .map_err(|_| RuntimeError::ServiceUnavailable("request timed out".into()))??;
    let response = tokio::time::timeout(timeout, read_service_response(&mut stream))
        .await
        .map_err(|_| RuntimeError::ServiceUnavailable("response timed out".into()))??;
    response.validate_for(request)?;
    Ok(response)
}

#[cfg(windows)]
async fn call_runtime_service(
    request: &RuntimeServiceRequestV1,
    timeout: Duration,
) -> Result<RuntimeServiceResponseV1> {
    request.validate()?;
    let mut stream = connect_windows_service().await?;
    tokio::time::timeout(PROBE_TIMEOUT, write_service_request(&mut stream, request))
        .await
        .map_err(|_| RuntimeError::ServiceUnavailable("request timed out".into()))??;
    let response = tokio::time::timeout(timeout, read_service_response(&mut stream))
        .await
        .map_err(|_| RuntimeError::ServiceUnavailable("response timed out".into()))??;
    response.validate_for(request)?;
    Ok(response)
}

pub fn stage_job_inputs(project_root: &Path, job: &GuestJobV1) -> Result<RuntimeSpoolReceiptV1> {
    stage_job_inputs_at(project_root, job, &service_spool_root()?)
}

fn stage_job_inputs_at(
    project_root: &Path,
    job: &GuestJobV1,
    spool: &Path,
) -> Result<RuntimeSpoolReceiptV1> {
    job.validate()?;
    create_private_directory(spool)?;
    let mut unique = std::collections::HashSet::new();
    let mut total = 0_u64;
    let mut reused = 0_u32;
    for input in &job.inputs {
        total = total
            .checked_add(input.size)
            .context("runtime spool total overflow")?;
        if !unique.insert(input.sha256.as_str()) {
            continue;
        }
        let digest = input
            .sha256
            .strip_prefix("sha256:")
            .context("runtime input digest is invalid")?;
        let prefix = spool.join(&digest[..2]);
        create_private_directory(&prefix)?;
        let destination = prefix.join(digest);
        if verify_spool_object(&destination, input.size, &input.sha256).is_ok() {
            verify_project_input(project_root, input)?;
            reused = reused.saturating_add(1);
            continue;
        }
        if destination.exists() {
            return Err(RuntimeError::AssetVerificationFailed(format!(
                "existing spool object failed verification: {}",
                input.sha256
            ))
            .into());
        }
        let temporary = prefix.join(format!(".spool-{}.tmp", Uuid::now_v7()));
        let result = (|| -> Result<()> {
            let mut source = open_project_input(project_root, &input.path)?;
            anyhow::ensure!(
                source.metadata()?.is_file() && source.metadata()?.len() == input.size,
                "runtime input type or size changed before spooling"
            );
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .context("create temporary runtime spool object")?;
            let mut hasher = Sha256::new();
            let mut size = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = source
                    .read(&mut buffer)
                    .context("read runtime input for spool")?;
                if read == 0 {
                    break;
                }
                size = size
                    .checked_add(read as u64)
                    .context("runtime input size overflow")?;
                anyhow::ensure!(size <= input.size, "runtime input grew while spooling");
                output
                    .write_all(&buffer[..read])
                    .context("write runtime spool object")?;
                hasher.update(&buffer[..read]);
            }
            anyhow::ensure!(
                size == input.size,
                "runtime input was truncated while spooling"
            );
            let actual = format!("sha256:{}", hex::encode(hasher.finalize()));
            anyhow::ensure!(
                actual == input.sha256,
                "runtime input digest changed while spooling"
            );
            output.sync_all().context("sync runtime spool object")?;
            drop(output);
            set_private_read_only(&temporary)?;
            match fs::hard_link(&temporary, &destination) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    verify_spool_object(&destination, input.size, &input.sha256)?;
                }
                Err(error) => return Err(error).context("atomically install runtime spool object"),
            }
            fs::remove_file(&temporary).context("remove linked runtime spool temporary")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
    }
    Ok(RuntimeSpoolReceiptV1 {
        schema: "reporch.runtime-spool-receipt.v1",
        object_count: u32::try_from(unique.len())?,
        total_bytes: total,
        reused_objects: reused,
    })
}

#[cfg(unix)]
pub async fn validate_spool_with_service(job: &GuestJobV1) -> Result<()> {
    let request = RuntimeServiceRequestV1 {
        schema: SERVICE_REQUEST_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        id: Uuid::now_v7(),
        command: RuntimeServiceCommandV1::ValidateSpool {
            objects: job.inputs.clone(),
        },
    };
    request.validate()?;
    let mut stream = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::net::UnixStream::connect(service_socket_path()?),
    )
    .await
    .map_err(|_| RuntimeError::ServiceUnavailable("connection timed out".into()))?
    .map_err(|error| RuntimeError::ServiceUnavailable(error.to_string()))?;
    tokio::time::timeout(PROBE_TIMEOUT, write_service_request(&mut stream, &request))
        .await
        .map_err(|_| RuntimeError::ServiceUnavailable("request timed out".into()))??;
    let response = tokio::time::timeout(PROBE_TIMEOUT, read_service_response(&mut stream))
        .await
        .map_err(|_| RuntimeError::ServiceUnavailable("response timed out".into()))??;
    response.validate_for(&request)?;
    match response.result {
        RuntimeServiceResultV1::SpoolValid { .. } => Ok(()),
        RuntimeServiceResultV1::Error(error) => Err(RuntimeError::AssetVerificationFailed(
            format!("{}: {}", error.error_code, error.message),
        )
        .into()),
        RuntimeServiceResultV1::Pong { .. }
        | RuntimeServiceResultV1::RuntimeUpdated { .. }
        | RuntimeServiceResultV1::ToolchainInstalled { .. }
        | RuntimeServiceResultV1::JobCompleted { .. } => {
            Err(RuntimeError::ProtocolIncompatible.into())
        }
    }
}

#[cfg(windows)]
pub async fn validate_spool_with_service(job: &GuestJobV1) -> Result<()> {
    let request = RuntimeServiceRequestV1 {
        schema: SERVICE_REQUEST_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        id: Uuid::now_v7(),
        command: RuntimeServiceCommandV1::ValidateSpool {
            objects: job.inputs.clone(),
        },
    };
    request.validate()?;
    let mut stream = connect_windows_service().await?;
    tokio::time::timeout(PROBE_TIMEOUT, write_service_request(&mut stream, &request))
        .await
        .map_err(|_| RuntimeError::ServiceUnavailable("request timed out".into()))??;
    let response = tokio::time::timeout(PROBE_TIMEOUT, read_service_response(&mut stream))
        .await
        .map_err(|_| RuntimeError::ServiceUnavailable("response timed out".into()))??;
    response.validate_for(&request)?;
    match response.result {
        RuntimeServiceResultV1::SpoolValid { .. } => Ok(()),
        RuntimeServiceResultV1::Error(error) => Err(RuntimeError::AssetVerificationFailed(
            format!("{}: {}", error.error_code, error.message),
        )
        .into()),
        RuntimeServiceResultV1::Pong { .. }
        | RuntimeServiceResultV1::RuntimeUpdated { .. }
        | RuntimeServiceResultV1::ToolchainInstalled { .. }
        | RuntimeServiceResultV1::JobCompleted { .. } => {
            Err(RuntimeError::ProtocolIncompatible.into())
        }
    }
}

#[cfg(target_os = "linux")]
pub async fn execute_via_service(project_root: &Path, job: &GuestJobV1) -> Result<GuestResultV1> {
    stage_job_inputs(project_root, job)?;
    let bundle = verified_bundle().await?;
    let request = RuntimeServiceRequestV1 {
        schema: SERVICE_REQUEST_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        id: Uuid::now_v7(),
        command: RuntimeServiceCommandV1::RunJob {
            job: Box::new(job.clone()),
            runtime_sequence: bundle.installation.sequence,
            runtime_bundle_digest: bundle.installation.bundle_sha256,
        },
    };
    request.validate()?;
    let mut stream = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::net::UnixStream::connect(service_socket_path()?),
    )
    .await
    .map_err(|_| RuntimeError::ServiceUnavailable("connection timed out".into()))?
    .map_err(|error| RuntimeError::ServiceUnavailable(error.to_string()))?;
    tokio::time::timeout(PROBE_TIMEOUT, write_service_request(&mut stream, &request))
        .await
        .map_err(|_| RuntimeError::ServiceUnavailable("request timed out".into()))??;
    let result_timeout =
        Duration::from_millis(job.limits.timeout_ms).saturating_add(Duration::from_secs(15));
    let response = tokio::time::timeout(result_timeout, read_service_response(&mut stream))
        .await
        .map_err(|_| RuntimeError::GuestUnresponsive)??;
    response.validate_for(&request)?;
    match response.result {
        RuntimeServiceResultV1::JobCompleted { result } => Ok(*result),
        RuntimeServiceResultV1::Error(error) => Err(RuntimeError::GuestBootFailed(format!(
            "{}: {}",
            error.error_code, error.message
        ))
        .into()),
        RuntimeServiceResultV1::Pong { .. }
        | RuntimeServiceResultV1::RuntimeUpdated { .. }
        | RuntimeServiceResultV1::ToolchainInstalled { .. }
        | RuntimeServiceResultV1::SpoolValid { .. } => {
            Err(RuntimeError::ProtocolIncompatible.into())
        }
    }
}

#[cfg(windows)]
pub async fn execute_via_service(project_root: &Path, job: &GuestJobV1) -> Result<GuestResultV1> {
    stage_job_inputs(project_root, job)?;
    let bundle = verified_bundle().await?;
    let request = RuntimeServiceRequestV1 {
        schema: SERVICE_REQUEST_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        id: Uuid::now_v7(),
        command: RuntimeServiceCommandV1::RunJob {
            job: Box::new(job.clone()),
            runtime_sequence: bundle.installation.sequence,
            runtime_bundle_digest: bundle.installation.bundle_sha256,
        },
    };
    request.validate()?;
    let mut stream = connect_windows_service().await?;
    tokio::time::timeout(PROBE_TIMEOUT, write_service_request(&mut stream, &request))
        .await
        .map_err(|_| RuntimeError::ServiceUnavailable("request timed out".into()))??;
    let result_timeout =
        Duration::from_millis(job.limits.timeout_ms).saturating_add(Duration::from_secs(15));
    let response = tokio::time::timeout(result_timeout, read_service_response(&mut stream))
        .await
        .map_err(|_| RuntimeError::GuestUnresponsive)??;
    response.validate_for(&request)?;
    match response.result {
        RuntimeServiceResultV1::JobCompleted { result } => Ok(*result),
        RuntimeServiceResultV1::Error(error) => Err(RuntimeError::GuestBootFailed(format!(
            "{}: {}",
            error.error_code, error.message
        ))
        .into()),
        RuntimeServiceResultV1::Pong { .. }
        | RuntimeServiceResultV1::RuntimeUpdated { .. }
        | RuntimeServiceResultV1::ToolchainInstalled { .. }
        | RuntimeServiceResultV1::SpoolValid { .. } => {
            Err(RuntimeError::ProtocolIncompatible.into())
        }
    }
}

/// Execute one preview job with the native backend selected for this host.
///
/// Linux crosses the least-privilege broker boundary. Apple Virtualization is
/// an in-process framework and therefore boots the VM directly. Windows
/// crosses the signed HCS broker boundary over an identity-checked named pipe.
#[cfg(target_os = "linux")]
pub async fn execute_native(project_root: &Path, job: &GuestJobV1) -> Result<GuestResultV1> {
    execute_via_service(project_root, job).await
}

#[cfg(target_os = "macos")]
pub async fn execute_native(project_root: &Path, job: &GuestJobV1) -> Result<GuestResultV1> {
    job.validate()?;
    let bundle = verified_bundle().await?;
    let toolchain = if job.toolchain_id == "runtime-self-test" {
        None
    } else {
        let toolchain = verified_toolchain(&job.toolchain_id).await?;
        ensure_job_toolchain_identity(job, &toolchain)?;
        Some(toolchain)
    };
    let project_root = project_root.to_owned();
    let job = job.clone();
    let cancellation = Arc::new(AtomicBool::new(false));
    let cancellation_guard = CancellationOnDrop(Some(cancellation.clone()));
    let worker_cancellation = cancellation.clone();
    let execution = tokio::task::spawn_blocking(move || {
        // The blocking lifecycle owns the slot. Dropping the async waiter can
        // request cancellation, but can never release admission while a VM is
        // still booting or cleaning up.
        let _slot = acquire_execution_slot(&worker_cancellation)?;
        apple_backend::execute(
            &bundle,
            toolchain.as_ref(),
            &project_root,
            &job,
            &worker_cancellation,
        )
    })
    .await
    .context("join Apple Virtualization execution")?;
    cancellation_guard.disarm();
    execution
}

#[cfg(target_os = "macos")]
struct CancellationOnDrop(Option<Arc<AtomicBool>>);

#[cfg(target_os = "macos")]
impl CancellationOnDrop {
    fn disarm(mut self) {
        self.0.take();
    }
}

#[cfg(target_os = "macos")]
impl Drop for CancellationOnDrop {
    fn drop(&mut self) {
        if let Some(cancellation) = self.0.as_ref() {
            cancellation.store(true, Ordering::Release);
        }
    }
}

#[cfg(target_os = "macos")]
struct ExecutionSlot(fs::File);

#[cfg(target_os = "macos")]
impl Drop for ExecutionSlot {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

#[cfg(target_os = "macos")]
fn acquire_execution_slot(cancellation: &AtomicBool) -> Result<ExecutionSlot> {
    let slot_root = runtime_root()?.join("execution-slots");
    create_private_directory(&slot_root)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30 * 60);
    loop {
        if cancellation.load(Ordering::Acquire) {
            anyhow::bail!("runtime execution cancelled while waiting for admission");
        }
        for index in 0..2 {
            let path = slot_root.join(format!("slot-{index}.lock"));
            let file = open_installation_lock(&path)?;
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(ExecutionSlot(file)),
                Err(error) if installation_lock_is_contended(&error) => {}
                Err(error) => return Err(error).context("lock runtime execution slot"),
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(RuntimeError::ServiceUnavailable(
                "runtime execution admission timed out".into(),
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(target_os = "macos")]
fn ensure_job_toolchain_identity(
    job: &GuestJobV1,
    toolchain: &VerifiedToolchainBundleV2,
) -> Result<()> {
    anyhow::ensure!(
        job.toolchain_index_sequence == Some(toolchain.installation.index_sequence)
            && job.toolchain_bundle_sha256.as_deref()
                == Some(toolchain.installation.bundle_sha256.as_str())
            && job.toolchain_lock_sha256.as_deref()
                == Some(toolchain.installation.toolchain_lock_sha256.as_str()),
        "runtime job toolchain identity does not match the verified installed bundle"
    );
    Ok(())
}

#[cfg(windows)]
pub async fn execute_native(project_root: &Path, job: &GuestJobV1) -> Result<GuestResultV1> {
    execute_via_service(project_root, job).await
}

/// Run the backend-independent half of a VM execution over a byte stream.
///
/// Platform adapters only boot an isolated VM and return its console/vsock
/// stream. This function owns handshake binding, content transfer, and result
/// validation so every backend enforces the same protocol.
pub async fn exchange_with_guest(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    project_root: &Path,
    job: &GuestJobV1,
    expected_bundle_digest: &str,
) -> Result<GuestResultV1> {
    job.validate()?;
    let handshake = tokio::time::timeout(GUEST_HANDSHAKE_TIMEOUT, read_wire_message(stream))
        .await
        .map_err(|_| RuntimeError::GuestUnresponsive)??;
    let handshake = match handshake {
        WireMessageV1::Handshake(handshake) => handshake,
        _ => return Err(RuntimeError::ProtocolIncompatible.into()),
    };
    handshake.validate(&job.nonce, expected_bundle_digest)?;
    write_wire_with_idle_timeout(stream, &WireMessageV1::Job(job.clone())).await?;

    for (index, input) in job.inputs.iter().enumerate() {
        let index = u32::try_from(index).context("too many runtime input objects")?;
        let mut file = open_project_input(project_root, &input.path)?;
        let metadata = file.metadata().context("inspect opened runtime input")?;
        anyhow::ensure!(
            metadata.is_file() && metadata.len() == input.size,
            "runtime input type or size changed"
        );
        let mut hasher = Sha256::new();
        let mut offset = 0_u64;
        let mut buffer = vec![0_u8; MAX_CONTENT_CHUNK_BYTES.min(1024 * 1024)];
        loop {
            let read = file.read(&mut buffer).context("read runtime input")?;
            if read == 0 {
                break;
            }
            let next = offset
                .checked_add(read as u64)
                .context("runtime input size overflow")?;
            anyhow::ensure!(next <= input.size, "runtime input grew while being sent");
            hasher.update(&buffer[..read]);
            let chunk = InputChunkV1 {
                object_index: index,
                offset,
                bytes: ByteBuf::from(buffer[..read].to_vec()),
                eof: false,
            };
            write_wire_with_idle_timeout(stream, &WireMessageV1::InputChunk(chunk)).await?;
            offset = next;
        }
        anyhow::ensure!(
            offset == input.size,
            "runtime input was truncated while being sent"
        );
        let digest = format!("sha256:{}", hex::encode(hasher.finalize()));
        anyhow::ensure!(digest == input.sha256, "runtime input digest changed");
        let eof = InputChunkV1 {
            object_index: index,
            offset,
            bytes: ByteBuf::new(),
            eof: true,
        };
        write_wire_with_idle_timeout(stream, &WireMessageV1::InputChunk(eof)).await?;
    }

    let result_timeout =
        Duration::from_millis(job.limits.timeout_ms).saturating_add(Duration::from_secs(5));
    let message = tokio::time::timeout(result_timeout, read_wire_message(stream))
        .await
        .map_err(|_| RuntimeError::GuestUnresponsive)??;
    match message {
        WireMessageV1::Result(result) => {
            result.validate_for(job)?;
            Ok(result)
        }
        WireMessageV1::ProtocolError(failure) => Err(RuntimeError::GuestBootFailed(format!(
            "{}: {}",
            failure.error_code, failure.message
        ))
        .into()),
        _ => Err(RuntimeError::ProtocolIncompatible.into()),
    }
}

/// Blocking variant used by Windows Hyper-V sockets. It shares the exact
/// framed protocol and content checks with the async Apple/Linux transport.
pub fn exchange_with_guest_sync(
    stream: &mut (impl std::io::Read + std::io::Write),
    project_root: &Path,
    job: &GuestJobV1,
    expected_bundle_digest: &str,
) -> Result<GuestResultV1> {
    job.validate()?;
    let handshake = match read_wire_message_sync(stream)? {
        WireMessageV1::Handshake(handshake) => handshake,
        _ => return Err(RuntimeError::ProtocolIncompatible.into()),
    };
    handshake.validate(&job.nonce, expected_bundle_digest)?;
    exchange_job_with_guest_sync(stream, project_root, job)
}

/// Send a job only after the caller has authenticated a complete guest
/// handshake. Keeping this phase separate lets the Windows HCS adapter retry
/// an early transport connection without ever executing a job twice.
pub fn exchange_job_with_guest_sync(
    stream: &mut (impl std::io::Read + std::io::Write),
    project_root: &Path,
    job: &GuestJobV1,
) -> Result<GuestResultV1> {
    job.validate()?;
    write_wire_message_sync(stream, &WireMessageV1::Job(job.clone()))?;

    for (index, input) in job.inputs.iter().enumerate() {
        let index = u32::try_from(index).context("too many runtime input objects")?;
        let mut file = open_project_input(project_root, &input.path)?;
        let metadata = file.metadata().context("inspect opened runtime input")?;
        anyhow::ensure!(
            metadata.is_file() && metadata.len() == input.size,
            "runtime input type or size changed"
        );
        let mut hasher = Sha256::new();
        let mut offset = 0_u64;
        let mut buffer = vec![0_u8; MAX_CONTENT_CHUNK_BYTES.min(1024 * 1024)];
        loop {
            let read = file.read(&mut buffer).context("read runtime input")?;
            if read == 0 {
                break;
            }
            let next = offset
                .checked_add(read as u64)
                .context("runtime input size overflow")?;
            anyhow::ensure!(next <= input.size, "runtime input grew while being sent");
            hasher.update(&buffer[..read]);
            write_wire_message_sync(
                stream,
                &WireMessageV1::InputChunk(InputChunkV1 {
                    object_index: index,
                    offset,
                    bytes: ByteBuf::from(buffer[..read].to_vec()),
                    eof: false,
                }),
            )?;
            offset = next;
        }
        anyhow::ensure!(
            offset == input.size,
            "runtime input was truncated while being sent"
        );
        anyhow::ensure!(
            format!("sha256:{}", hex::encode(hasher.finalize())) == input.sha256,
            "runtime input digest changed"
        );
        write_wire_message_sync(
            stream,
            &WireMessageV1::InputChunk(InputChunkV1 {
                object_index: index,
                offset,
                bytes: ByteBuf::new(),
                eof: true,
            }),
        )?;
    }

    match read_wire_message_sync(stream)? {
        WireMessageV1::Result(result) => {
            result.validate_for(job)?;
            Ok(result)
        }
        WireMessageV1::ProtocolError(failure) => Err(RuntimeError::GuestBootFailed(format!(
            "{}: {}",
            failure.error_code, failure.message
        ))
        .into()),
        _ => Err(RuntimeError::ProtocolIncompatible.into()),
    }
}

/// Hyper-V boots one immutable VHDX, so per-job identity cannot be embedded in
/// a writable boot disk. The host sends a VM-specific challenge over the
/// already VM-addressed Hyper-V socket and the verified guest echoes it in the
/// normal handshake before receiving any project content.
pub fn exchange_with_guest_sync_challenged(
    stream: &mut (impl std::io::Read + std::io::Write),
    project_root: &Path,
    job: &GuestJobV1,
    expected_bundle_digest: &str,
) -> Result<GuestResultV1> {
    establish_guest_session_sync_challenged(stream, job, expected_bundle_digest)?;
    exchange_job_with_guest_sync(stream, project_root, job)
}

/// Authenticate a Hyper-V guest before any executable job content is sent.
/// HCS can transiently expose a connectable socket before the guest listener
/// is ready, so callers may safely retry this phase on a fresh connection.
pub fn establish_guest_session_sync_challenged(
    stream: &mut (impl std::io::Read + std::io::Write),
    job: &GuestJobV1,
    expected_bundle_digest: &str,
) -> Result<()> {
    job.validate()?;
    let challenge = HostChallengeV1 {
        schema: reporch_runtime_protocol::HOST_CHALLENGE_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        nonce: job.nonce.clone(),
        runtime_bundle_digest: expected_bundle_digest.into(),
    };
    challenge.validate()?;
    write_wire_message_sync(stream, &WireMessageV1::HostChallenge(challenge))?;
    let handshake = match read_wire_message_sync(stream)? {
        WireMessageV1::Handshake(handshake) => handshake,
        _ => return Err(RuntimeError::ProtocolIncompatible.into()),
    };
    handshake.validate(&job.nonce, expected_bundle_digest)?;
    Ok(())
}

async fn write_wire_with_idle_timeout(
    stream: &mut (impl AsyncWrite + Unpin),
    message: &WireMessageV1,
) -> Result<()> {
    tokio::time::timeout(GUEST_IO_IDLE_TIMEOUT, write_wire_message(stream, message))
        .await
        .map_err(|_| RuntimeError::GuestUnresponsive)??;
    Ok(())
}

#[cfg(unix)]
fn open_project_input(root: &Path, relative: &str) -> Result<fs::File> {
    use rustix::fs::{Mode, OFlags, openat};
    use std::path::Component;

    let root = fs::File::open(root).context("open runtime project root")?;
    anyhow::ensure!(
        root.metadata()?.is_dir(),
        "runtime project root is not a directory"
    );
    let components = Path::new(relative).components().collect::<Vec<_>>();
    anyhow::ensure!(!components.is_empty(), "runtime input path is empty");
    let mut directory = root;
    for component in &components[..components.len() - 1] {
        let Component::Normal(component) = component else {
            anyhow::bail!("runtime input path is not normalized");
        };
        let fd = openat(
            &directory,
            *component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("open runtime input directory without following symlinks")?;
        directory = fd.into();
    }
    let Component::Normal(file_name) = components[components.len() - 1] else {
        anyhow::bail!("runtime input file name is not normalized");
    };
    let fd = openat(
        &directory,
        file_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .context("open runtime input without following symlinks")?;
    Ok(fd.into())
}

#[cfg(windows)]
fn open_project_input(root: &Path, relative: &str) -> Result<fs::File> {
    use cap_std::ambient_authority;
    use cap_std::fs::Dir;

    let directory = Dir::open_ambient_dir(root, ambient_authority())
        .context("open capability-scoped runtime project root")?;
    let file = directory
        .open(relative)
        .context("open capability-scoped runtime project input")?;
    let metadata = file.metadata().context("inspect runtime project input")?;
    anyhow::ensure!(metadata.is_file(), "runtime input is not a regular file");
    Ok(file.into_std())
}

pub async fn update() -> Result<RuntimeUpdateV1> {
    update_or_repair(false).await
}

pub async fn repair() -> Result<RuntimeUpdateV1> {
    update_or_repair(true).await
}

async fn update_or_repair(force: bool) -> Result<RuntimeUpdateV1> {
    #[cfg(any(target_os = "linux", windows))]
    if probe_runtime_service().await {
        return request_service_runtime_update(force).await;
    }
    install_latest(force, None).await
}

pub async fn update_direct_for_service(force: bool) -> Result<RuntimeUpdateV1> {
    install_latest(force, None).await
}

pub async fn reset() -> Result<RuntimeUpdateV1> {
    let root = runtime_root()?;
    ensure_runtime_root_is_narrow(&root)?;
    let _installation_lock = acquire_installation_lock(&root).await?;
    if root.exists() {
        let minimum_sequence = read_installation(&root)?.map(|value| value.sequence);
        let parent = root.parent().context("runtime root has no parent")?;
        let backup = parent.join(format!(".reporch-runtime-reset-{}", Uuid::now_v7()));
        fs::rename(&root, &backup)
            .with_context(|| format!("move existing runtime aside to {}", backup.display()))?;
        match install_latest_locked(false, minimum_sequence, &root).await {
            Ok(result) => {
                let _ = fs::remove_dir_all(&backup);
                return Ok(result);
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&root);
                fs::rename(&backup, &root).context("restore runtime after failed reset")?;
                return Err(error);
            }
        }
    }
    install_latest_locked(false, None, &root).await
}

pub async fn status_at(root: &Path) -> Result<RuntimeStatusV1> {
    let target = HostTarget::current();
    let Some(target) = target else {
        return Ok(RuntimeStatusV1 {
            schema: STATUS_SCHEMA.into(),
            target: None,
            backend: RuntimeBackend::RemoteOnly,
            availability: RuntimeAvailability::RemoteOnly,
            installed_version: None,
            installed_sequence: None,
            protocol_version: reporch_runtime_core::PROTOCOL_VERSION,
            virtualization_available: false,
            service_available: false,
            reason: Some(
                "This operating system or architecture is not supported for local execution".into(),
            ),
        });
    };
    let installation = read_installation(root)?;
    let (virtualization_available, reason) = probe_virtualization(target).await;
    let bundle_verification = match installation.clone() {
        Some(installation) => {
            let verification_root = root.to_owned();
            Some(
                tokio::task::spawn_blocking(move || {
                    verify_installed_bundle_at(&verification_root, &installation)
                        .map(|_| ())
                        .map_err(|error| format!("{error:#}"))
                })
                .await
                .context("join runtime status bundle verification")?,
            )
        }
        None => None,
    };
    let bundle_verified = matches!(&bundle_verification, Some(Ok(())));
    let asset_failure = bundle_verification
        .as_ref()
        .and_then(|verification| verification.as_ref().err())
        .map(|error| format!("Runtime asset verification failed: {error}"));
    // Apple Virtualization runs in-process. Linux and Windows require the
    // least-privilege broker and must never be reported ready merely because
    // its executable is present on disk.
    let service_available = if !bundle_verified {
        false
    } else {
        match target {
            HostTarget::DarwinArm64 | HostTarget::DarwinX64 => true,
            HostTarget::LinuxArm64Gnu | HostTarget::LinuxX64Gnu => probe_runtime_service().await,
            HostTarget::WindowsX64Msvc => probe_runtime_service().await,
        }
    };
    let availability = match (
        &installation,
        bundle_verified,
        virtualization_available,
        service_available,
    ) {
        (None, _, _, _) => RuntimeAvailability::NotInstalled,
        (Some(_), false, _, _) => RuntimeAvailability::Broken,
        (Some(_), true, false, _) => RuntimeAvailability::RemoteOnly,
        (Some(_), true, true, false) => RuntimeAvailability::Broken,
        (Some(_), true, true, true) => RuntimeAvailability::Ready,
    };
    Ok(RuntimeStatusV1 {
        schema: STATUS_SCHEMA.into(),
        target: Some(target),
        backend: if virtualization_available {
            target.native_backend()
        } else {
            RuntimeBackend::RemoteOnly
        },
        availability,
        installed_version: installation.as_ref().map(|value| value.version.clone()),
        installed_sequence: installation.as_ref().map(|value| value.sequence),
        protocol_version: reporch_runtime_core::PROTOCOL_VERSION,
        virtualization_available,
        service_available,
        reason: if installation.is_none() {
            Some(RuntimeError::BootstrapIncomplete.to_string())
        } else {
            asset_failure.or(reason)
        },
    })
}

fn read_installation(root: &Path) -> Result<Option<RuntimeInstallationV1>> {
    let path = root.join("current.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read runtime installation {}", path.display()));
        }
    };
    anyhow::ensure!(
        bytes.len() <= 64 * 1024,
        "runtime installation record is too large"
    );
    let installation: RuntimeInstallationV1 =
        serde_json::from_slice(&bytes).context("parse runtime installation record")?;
    anyhow::ensure!(
        installation.schema == INSTALLATION_SCHEMA,
        "unsupported runtime installation record"
    );
    installation.validate().map_err(anyhow::Error::from)?;
    if let Some(target) = HostTarget::current() {
        anyhow::ensure!(
            installation.target == target,
            "runtime installation target does not match this host"
        );
    }
    Ok(Some(installation))
}

async fn probe_virtualization(target: HostTarget) -> (bool, Option<String>) {
    match target {
        HostTarget::LinuxArm64Gnu | HostTarget::LinuxX64Gnu => {
            let path = Path::new("/dev/kvm");
            let available = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .is_ok();
            (
                available,
                (!available)
                    .then(|| "KVM is unavailable or /dev/kvm is not readable and writable".into()),
            )
        }
        HostTarget::DarwinArm64 | HostTarget::DarwinX64 => {
            let output =
                bounded_command(Path::new("/usr/sbin/sysctl"), &["-n", "kern.hv_support"]).await;
            let available = output.as_deref().is_some_and(|value| value.trim() == "1");
            (
                available,
                (!available).then(|| "Apple hardware virtualization is unavailable".into()),
            )
        }
        HostTarget::WindowsX64Msvc => {
            let available = windows_virtualization_available();
            (
                available,
                (!available).then(|| "Hyper-V is unavailable or disabled".into()),
            )
        }
    }
}

#[cfg(windows)]
fn windows_virtualization_available() -> bool {
    windows_identity::hyper_v_available()
}

#[cfg(not(windows))]
fn windows_virtualization_available() -> bool {
    false
}

#[cfg(unix)]
async fn probe_runtime_service() -> bool {
    let request = RuntimeServiceRequestV1 {
        schema: SERVICE_REQUEST_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        id: Uuid::now_v7(),
        command: RuntimeServiceCommandV1::Ping,
    };
    let Ok(socket) = service_socket_path() else {
        return false;
    };
    let Ok(Ok(mut stream)) =
        tokio::time::timeout(PROBE_TIMEOUT, tokio::net::UnixStream::connect(socket)).await
    else {
        return false;
    };
    if !matches!(
        tokio::time::timeout(PROBE_TIMEOUT, write_service_request(&mut stream, &request)).await,
        Ok(Ok(()))
    ) {
        return false;
    }
    let Ok(Ok(response)) =
        tokio::time::timeout(PROBE_TIMEOUT, read_service_response(&mut stream)).await
    else {
        return false;
    };
    response.validate_for(&request).is_ok()
        && matches!(response.result, RuntimeServiceResultV1::Pong { .. })
}

#[cfg(windows)]
async fn probe_runtime_service() -> bool {
    let request = RuntimeServiceRequestV1 {
        schema: SERVICE_REQUEST_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        id: Uuid::now_v7(),
        command: RuntimeServiceCommandV1::Ping,
    };
    let Ok(mut stream) = connect_windows_service().await else {
        return false;
    };
    if !matches!(
        tokio::time::timeout(PROBE_TIMEOUT, write_service_request(&mut stream, &request)).await,
        Ok(Ok(()))
    ) {
        return false;
    }
    let Ok(Ok(response)) =
        tokio::time::timeout(PROBE_TIMEOUT, read_service_response(&mut stream)).await
    else {
        return false;
    };
    response.validate_for(&request).is_ok()
        && matches!(response.result, RuntimeServiceResultV1::Pong { .. })
}

#[cfg(windows)]
async fn connect_windows_service() -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let pipe = service_pipe_name()?;
    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        match ClientOptions::new().open(&pipe) {
            Ok(client) => {
                windows_identity::authenticate_runtime_pipe_server(&client)?;
                return Ok(client);
            }
            Err(error) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if error.kind() != std::io::ErrorKind::NotFound
                    && error.kind() != std::io::ErrorKind::WouldBlock
                    && error.raw_os_error() != Some(231)
                {
                    return Err(RuntimeError::ServiceUnavailable(error.to_string()).into());
                }
            }
            Err(error) => {
                return Err(RuntimeError::ServiceUnavailable(error.to_string()).into());
            }
        }
    }
}

async fn bounded_command(program: &Path, arguments: &[&str]) -> Option<String> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.take(4_096).read_to_end(&mut bytes).await.ok()?;
        Some(bytes)
    });
    let status = match tokio::time::timeout(PROBE_TIMEOUT, child.wait()).await {
        Ok(result) => result.ok()?,
        Err(_) => {
            let _ = child.kill().await;
            reader.abort();
            return None;
        }
    };
    let stdout = reader.await.ok()??;
    status
        .success()
        .then(|| String::from_utf8_lossy(&stdout).into_owned())
}

async fn install_latest(force: bool, minimum_sequence: Option<u64>) -> Result<RuntimeUpdateV1> {
    let root = runtime_root()?;
    ensure_runtime_root_is_narrow(&root)?;
    let _installation_lock = acquire_installation_lock(&root).await?;
    install_latest_locked(force, minimum_sequence, &root).await
}

async fn install_latest_locked(
    force: bool,
    minimum_sequence: Option<u64>,
    root: &Path,
) -> Result<RuntimeUpdateV1> {
    let target = HostTarget::current().ok_or_else(|| {
        RuntimeError::VirtualizationUnavailable(
            "this operating system or architecture is not supported".into(),
        )
    })?;
    ensure_runtime_root_is_narrow(root)?;
    create_private_directory(root)?;
    let previous = read_installation(root)?;
    let base = runtime_channel_url()?;
    let target_name = target_name(target);
    let manifest_url = base
        .join(&format!("runtime-{target_name}-manifest.json"))
        .context("build runtime manifest URL")?;
    let signature_url = base
        .join(&format!("runtime-{target_name}-manifest.json.minisig"))
        .context("build runtime signature URL")?;
    let client = runtime_download_client()?;
    let manifest_bytes = fetch_small(&client, manifest_url.as_str(), MAX_MANIFEST_BYTES).await?;
    let signature_bytes = fetch_small(&client, signature_url.as_str(), MAX_SIGNATURE_BYTES).await?;
    verify_signature(&manifest_bytes, &signature_bytes)?;
    let manifest: RuntimeBundleManifestV1 =
        serde_json::from_slice(&manifest_bytes).context("parse signed runtime manifest")?;
    let manifest_digest = format!("sha256:{}", hex::encode(Sha256::digest(&manifest_bytes)));
    manifest.validate(Utc::now()).map_err(anyhow::Error::from)?;
    anyhow::ensure!(
        manifest.target == target,
        "runtime manifest target does not match this host"
    );
    host_version::ensure_runtime_supported(&manifest)?;
    if let Some(minimum_sequence) = minimum_sequence {
        anyhow::ensure!(
            manifest.sequence >= minimum_sequence,
            "runtime channel attempted a rollback below sequence {minimum_sequence}"
        );
    }
    if let Some(current) = &previous {
        anyhow::ensure!(
            manifest.sequence >= current.sequence,
            "runtime channel attempted a rollback from sequence {} to {}",
            current.sequence,
            manifest.sequence
        );
        anyhow::ensure!(
            manifest.sequence != current.sequence || manifest_digest == current.bundle_sha256,
            "runtime channel reused sequence {} for different bytes",
            manifest.sequence
        );
        if !force
            && manifest.sequence == current.sequence
            && installed_bundle_complete(root, current)
        {
            return Ok(RuntimeUpdateV1 {
                schema: "reporch.runtime-update.v1".into(),
                previous_version: Some(current.version.clone()),
                installed_version: current.version.clone(),
                sequence: current.sequence,
                target,
                repaired: false,
            });
        }
    }
    let staging = root.join(format!(".install-{}", Uuid::now_v7()));
    create_private_directory(&staging)?;
    let install = install_manifest_artifacts(
        &client,
        &manifest,
        &manifest_bytes,
        &signature_bytes,
        root,
        &staging,
    )
    .await;
    if let Err(error) = install {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    let bundles = root.join("bundles");
    create_private_directory(&bundles)?;
    let destination = bundle_directory(root, manifest.sequence, &manifest.version);
    let replaced = root.join(format!(".replaced-bundle-{}", Uuid::now_v7()));
    let had_destination = destination.exists();
    if destination.exists() {
        ensure_runtime_child(root, &destination)?;
        fs::rename(&destination, &replaced).context("move previous runtime bundle aside")?;
    }
    if let Err(error) = fs::rename(&staging, &destination) {
        if had_destination {
            let _ = fs::rename(&replaced, &destination);
        }
        return Err(error).context("atomically install runtime bundle");
    }
    let installation = RuntimeInstallationV1 {
        schema: INSTALLATION_SCHEMA.into(),
        sequence: manifest.sequence,
        version: manifest.version.clone(),
        target,
        bundle_sha256: manifest_digest,
        installed_at: Utc::now(),
    };
    if let Some(current) = &previous
        && let Err(error) = write_json_atomic(&root.join("previous.json"), current)
    {
        let _ = fs::remove_dir_all(&destination);
        if had_destination {
            let _ = fs::rename(&replaced, &destination);
        }
        return Err(error).context("preserve previous runtime installation state");
    }
    if let Err(error) = write_json_atomic(&root.join("current.json"), &installation) {
        let _ = fs::remove_dir_all(&destination);
        if had_destination {
            let _ = fs::rename(&replaced, &destination);
        }
        return Err(error).context("commit runtime installation state");
    }
    if native_boot_probe_available(target).await
        && let Err(error) = smoke_test_installed_runtime(root).await
    {
        let rollback = rollback_failed_install(
            root,
            previous.as_ref(),
            &destination,
            had_destination.then_some(replaced.as_path()),
        );
        if let Err(rollback_error) = rollback {
            return Err(RuntimeError::CleanupFailed(format!(
                "new runtime failed its boot test ({error:#}) and rollback failed ({rollback_error:#})"
            ))
            .into());
        }
        return Err(RuntimeError::GuestBootFailed(format!(
            "new runtime failed its boot self-test and was rolled back: {error:#}"
        ))
        .into());
    }
    if had_destination {
        let _ = fs::remove_dir_all(&replaced);
    }
    Ok(RuntimeUpdateV1 {
        schema: "reporch.runtime-update.v1".into(),
        previous_version: previous.map(|value| value.version),
        installed_version: manifest.version,
        sequence: manifest.sequence,
        target,
        repaired: force,
    })
}

async fn native_boot_probe_available(target: HostTarget) -> bool {
    if !probe_virtualization(target).await.0 {
        return false;
    }
    match target {
        HostTarget::DarwinArm64 | HostTarget::DarwinX64 => true,
        HostTarget::LinuxArm64Gnu | HostTarget::LinuxX64Gnu => probe_runtime_service().await,
        HostTarget::WindowsX64Msvc => probe_runtime_service().await,
    }
}

async fn smoke_test_installed_runtime(project_root: &Path) -> Result<()> {
    let id = Uuid::now_v7();
    let job = GuestJobV1 {
        schema: reporch_runtime_core::JOB_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        id,
        nonce: format!("runtime-smoke-{}", id.simple()),
        operation: reporch_runtime_protocol::GuestOperationV1::Program,
        toolchain_id: "runtime-self-test".into(),
        toolchain_index_sequence: None,
        toolchain_bundle_sha256: None,
        toolchain_lock_sha256: None,
        command: vec!["/sbin/reporch-guestd".into(), "--self-test-workload".into()],
        environment: BTreeMap::new(),
        inputs: Vec::new(),
        limits: reporch_runtime_protocol::ResourceLimitsV1 {
            timeout_ms: 10_000,
            memory_mib: 128,
            cpu_millis: 1_000,
            pids: 16,
            stdout_bytes: 4_096,
            stderr_bytes: 4_096,
            artifact_bytes: 4_096,
        },
    };
    let result = execute_native(project_root, &job).await?;
    anyhow::ensure!(
        result.exit_code == 0
            && result.stdout.encoding == reporch_runtime_protocol::GuestOutputEncodingV1::Utf8
            && result.stdout.data == "reporch-runtime-self-test-ok\n"
            && !result.stdout.truncated
            && !result.stderr.truncated,
        "runtime self-test returned an unexpected result"
    );
    Ok(())
}

/// Evidence produced by the release-only native VM qualification command.
///
/// This deliberately exercises the installed, signature-verified runtime and
/// the platform broker instead of a mocked transport. It is public so the
/// exact release CLI can be used by self-hosted qualification runners without
/// exposing backend-specific test hooks.
#[derive(Debug, Clone, Serialize)]
pub struct NativeRuntimeQualificationV1 {
    pub schema: &'static str,
    pub target: HostTarget,
    pub backend: RuntimeBackend,
    pub runtime_version: String,
    pub runtime_sequence: u64,
    pub toolchain_id: String,
    pub toolchain_bundle_sha256: String,
    pub iterations: u32,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    pub maximum_ms: u64,
    pub lifecycle: bool,
    pub handshake: bool,
    pub guest_workload: bool,
    pub cleanup: bool,
    pub signed_toolchain_unchanged: bool,
    pub completed_at: String,
    pub passed: bool,
}

/// Boot, handshake with, execute, and tear down the installed native VM
/// repeatedly, then run a real command from one signed toolchain image.
///
/// Host-level qualification scripts additionally assert that no backend
/// process, HCS system, jail, socket, or overlay remains after this returns.
pub async fn qualify_installed_native_runtime(
    iterations: u32,
    toolchain_id: &str,
) -> Result<NativeRuntimeQualificationV1> {
    anyhow::ensure!(
        (1..=1_000).contains(&iterations),
        "native runtime qualification iterations must be between 1 and 1000"
    );
    anyhow::ensure!(
        !toolchain_id.is_empty() && toolchain_id.len() <= 128,
        "native runtime qualification toolchain ID is invalid"
    );

    let status = status().await?;
    anyhow::ensure!(
        status.availability == RuntimeAvailability::Ready,
        "native runtime is not ready for qualification: {:?}",
        status.availability
    );
    let target = status
        .target
        .context("native runtime qualification has no host target")?;
    let runtime_version = status
        .installed_version
        .clone()
        .context("native runtime qualification has no installed version")?;
    let runtime_sequence = status
        .installed_sequence
        .context("native runtime qualification has no installed sequence")?;
    let report = doctor().await?;
    anyhow::ensure!(
        report.checks.iter().all(|check| check.passed),
        "native runtime doctor did not pass every check"
    );

    let project_root =
        std::env::current_dir().context("resolve qualification working directory")?;
    let mut durations = Vec::with_capacity(iterations as usize);
    for iteration in 0..iterations {
        let started = Instant::now();
        smoke_test_installed_runtime(&project_root)
            .await
            .with_context(|| {
                format!(
                    "native runtime qualification iteration {}/{}",
                    iteration + 1,
                    iterations
                )
            })?;
        durations.push(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
    }

    let installed = install_toolchain(toolchain_id)
        .await
        .context("install signed qualification toolchain")?;
    let before = verified_toolchain(toolchain_id)
        .await
        .context("verify qualification toolchain before execution")?;
    let id = Uuid::now_v7();
    let job = GuestJobV1 {
        schema: reporch_runtime_core::JOB_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        id,
        nonce: format!("runtime-toolchain-{}", id.simple()),
        operation: reporch_runtime_protocol::GuestOperationV1::Program,
        toolchain_id: toolchain_id.to_owned(),
        toolchain_index_sequence: Some(installed.installation.index_sequence),
        toolchain_bundle_sha256: Some(installed.installation.bundle_sha256.clone()),
        toolchain_lock_sha256: Some(installed.installation.toolchain_lock_sha256.clone()),
        command: vec![
            "bash".into(),
            "-c".into(),
            "printf 'reporch-toolchain-self-test-ok\\n'".into(),
        ],
        environment: BTreeMap::new(),
        inputs: Vec::new(),
        limits: reporch_runtime_protocol::ResourceLimitsV1 {
            timeout_ms: 10_000,
            memory_mib: 128,
            cpu_millis: 1_000,
            pids: 16,
            stdout_bytes: 4_096,
            stderr_bytes: 4_096,
            artifact_bytes: 4_096,
        },
    };
    job.validate()
        .context("validate qualification toolchain job")?;
    let result = execute_native(&project_root, &job)
        .await
        .context("execute signed qualification toolchain")?;
    anyhow::ensure!(
        result.exit_code == 0
            && result.stdout.encoding == reporch_runtime_protocol::GuestOutputEncodingV1::Utf8
            && result.stdout.data == "reporch-toolchain-self-test-ok\n"
            && !result.stdout.truncated
            && !result.stderr.truncated,
        "signed toolchain qualification returned an unexpected result"
    );
    let after = verified_toolchain(toolchain_id)
        .await
        .context("verify qualification toolchain after execution")?;
    anyhow::ensure!(
        before.bundle.sha256 == after.bundle.sha256
            && before.bundle.size == after.bundle.size
            && before.installation.bundle_sha256 == after.installation.bundle_sha256,
        "signed toolchain image changed during native VM execution"
    );

    durations.sort_unstable();
    let percentile = |value: usize| -> u64 {
        let rank = durations.len().saturating_mul(value).div_ceil(100);
        durations[rank.saturating_sub(1).min(durations.len() - 1)]
    };
    Ok(NativeRuntimeQualificationV1 {
        schema: "reporch.native-runtime-qualification.v1",
        target,
        backend: status.backend,
        runtime_version,
        runtime_sequence,
        toolchain_id: toolchain_id.to_owned(),
        toolchain_bundle_sha256: installed.installation.bundle_sha256,
        iterations,
        p50_ms: percentile(50),
        p95_ms: percentile(95),
        p99_ms: percentile(99),
        maximum_ms: durations.last().copied().unwrap_or_default(),
        lifecycle: true,
        handshake: true,
        guest_workload: true,
        cleanup: true,
        signed_toolchain_unchanged: true,
        completed_at: Utc::now().to_rfc3339(),
        passed: true,
    })
}

fn rollback_failed_install(
    root: &Path,
    previous: Option<&RuntimeInstallationV1>,
    failed_destination: &Path,
    replaced_destination: Option<&Path>,
) -> Result<()> {
    match previous {
        Some(previous) => write_json_atomic(&root.join("current.json"), previous)
            .context("restore previous runtime installation state")?,
        None => match fs::remove_file(root.join("current.json")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove failed runtime installation state"),
        },
    }
    if failed_destination.exists() {
        fs::remove_dir_all(failed_destination).context("remove failed runtime bundle")?;
    }
    if let Some(replaced) = replaced_destination {
        fs::rename(replaced, failed_destination).context("restore replaced runtime bundle")?;
    }
    Ok(())
}

struct InstallationLock(fs::File);

impl Drop for InstallationLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

async fn acquire_installation_lock(runtime_root: &Path) -> Result<InstallationLock> {
    let parent = runtime_root
        .parent()
        .context("runtime root has no parent for its installation lock")?;
    create_private_directory(parent)?;
    let path = parent.join(".reporch-runtime-v1.lock");
    tokio::task::spawn_blocking(move || acquire_installation_lock_blocking(&path))
        .await
        .context("join runtime installation lock")?
}

fn acquire_installation_lock_blocking(path: &Path) -> Result<InstallationLock> {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let file = open_installation_lock(path)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(InstallationLock(file)),
            Err(error)
                if installation_lock_is_contended(&error)
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) if installation_lock_is_contended(&error) => {
                anyhow::bail!("another runtime installation is still in progress")
            }
            Err(error) => return Err(error).context("lock runtime installation"),
        }
    }
}

fn installation_lock_is_contended(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        error.raw_os_error() == Some(windows::Win32::Foundation::ERROR_LOCK_VIOLATION.0 as i32)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(unix)]
fn open_installation_lock(path: &Path) -> Result<fs::File> {
    use rustix::fs::{Mode, OFlags, open};

    let file = open(
        path,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .context("open runtime installation lock without following symlinks")?;
    let file = fs::File::from(file);
    let metadata = file
        .metadata()
        .context("inspect runtime installation lock")?;
    anyhow::ensure!(
        metadata.is_file(),
        "runtime installation lock is not a file"
    );
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .context("restrict runtime installation lock")?;
    Ok(file)
}

#[cfg(windows)]
fn open_installation_lock(path: &Path) -> Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .context("open runtime installation lock")?;
    anyhow::ensure!(
        file.metadata()?.is_file() && !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "runtime installation lock is not a regular file"
    );
    Ok(file)
}

fn runtime_channel_url() -> Result<url::Url> {
    let base = std::env::var("REPORCH_RUNTIME_CHANNEL_URL")
        .unwrap_or_else(|_| RUNTIME_CHANNEL_BASE.into());
    parse_channel_url(&base, "runtime")
}

fn toolchain_channel_url() -> Result<url::Url> {
    let base = std::env::var("REPORCH_TOOLCHAIN_CHANNEL_URL")
        .unwrap_or_else(|_| TOOLCHAIN_CHANNEL_BASE.into());
    parse_channel_url(&base, "toolchain")
}

fn parse_channel_url(base: &str, label: &str) -> Result<url::Url> {
    let base = url::Url::parse(&format!("{}/", base.trim_end_matches('/')))
        .with_context(|| format!("parse {label} channel URL"))?;
    anyhow::ensure!(
        base.scheme() == "https"
            && base.host_str().is_some()
            && base.username().is_empty()
            && base.password().is_none()
            && base.query().is_none()
            && base.fragment().is_none(),
        "{label} channel must be credential-free HTTPS without query or fragment"
    );
    Ok(base)
}

fn runtime_download_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .https_only(true)
        .user_agent(concat!("reporch-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build runtime download client")
}

async fn install_manifest_artifacts(
    client: &reqwest::Client,
    manifest: &RuntimeBundleManifestV1,
    manifest_bytes: &[u8],
    signature_bytes: &[u8],
    cache_root: &Path,
    staging: &Path,
) -> Result<()> {
    for artifact in &manifest.artifacts {
        let destination = staging.join(&artifact.file_name);
        download_cached_verified(
            client,
            &artifact.source_url,
            cache_root,
            &destination,
            artifact.size,
            &artifact.sha256,
        )
        .await?;
        set_runtime_artifact_permissions(&destination, artifact.kind)?;
    }
    fs::write(staging.join("manifest.json"), manifest_bytes)
        .context("write installed runtime manifest")?;
    fs::write(staging.join("manifest.json.minisig"), signature_bytes)
        .context("write installed runtime signature")?;
    fs::write(
        staging.join(".complete"),
        format!("sha256:{}\n", hex::encode(Sha256::digest(manifest_bytes))),
    )
    .context("write runtime completion marker")?;
    for name in ["manifest.json", "manifest.json.minisig", ".complete"] {
        set_runtime_artifact_permissions(&staging.join(name), RuntimeArtifactKindV1::Rootfs)?;
    }
    Ok(())
}

async fn fetch_small(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    let response = tokio::time::timeout(Duration::from_secs(30), client.get(url).send())
        .await
        .context("runtime metadata request timed out")??
        .error_for_status()
        .with_context(|| format!("download runtime metadata {url}"))?;
    if response
        .content_length()
        .is_some_and(|length| length == 0 || length > limit as u64)
    {
        anyhow::bail!("runtime metadata has an invalid size");
    }
    let bytes = tokio::time::timeout(Duration::from_secs(30), response.bytes())
        .await
        .context("runtime metadata body timed out")??;
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= limit,
        "runtime metadata has an invalid size"
    );
    Ok(bytes.to_vec())
}

fn verify_signature(bytes: &[u8], signature: &[u8]) -> Result<()> {
    let public_key = PublicKey::decode(RUNTIME_PUBLIC_KEY).context("decode runtime public key")?;
    let signature = std::str::from_utf8(signature).context("runtime signature must be UTF-8")?;
    let signature = Signature::decode(signature).context("decode runtime signature")?;
    public_key
        .verify(bytes, &signature, false)
        .context("verify signed runtime manifest")
}

async fn download_cached_verified(
    client: &reqwest::Client,
    url: &str,
    cache_root: &Path,
    destination: &Path,
    expected_size: u64,
    expected_digest: &str,
) -> Result<()> {
    let digest = expected_digest
        .strip_prefix("sha256:")
        .context("runtime download digest is invalid")?;
    anyhow::ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "runtime download digest is invalid"
    );
    let cache = cache_root.join("downloads");
    ensure_runtime_child(cache_root, &cache)?;
    create_private_directory(&cache)?;
    let partial = cache.join(format!("{digest}.part"));
    let completed = cache.join(format!("{digest}.blob"));
    if resumable_file_size(&completed, expected_size, expected_digest)? != expected_size {
        download_verified_resumable(client, url, &partial, expected_size, expected_digest).await?;
        promote_completed_download(&partial, &completed, expected_size, expected_digest)?;
    }
    anyhow::ensure!(
        !destination.exists(),
        "runtime download destination already exists"
    );
    fs::hard_link(&completed, destination).with_context(|| {
        format!(
            "materialize verified runtime download {}",
            destination.display()
        )
    })?;
    Ok(())
}

fn promote_completed_download(
    partial: &Path,
    completed: &Path,
    expected_size: u64,
    expected_digest: &str,
) -> Result<()> {
    anyhow::ensure!(
        resumable_file_size(partial, expected_size, expected_digest)? == expected_size,
        "runtime artifact is not complete"
    );
    anyhow::ensure!(
        !completed.exists(),
        "verified runtime cache destination already exists"
    );
    fs::rename(partial, completed)
        .with_context(|| format!("promote verified runtime download {}", completed.display()))?;
    Ok(())
}

async fn download_verified_resumable(
    client: &reqwest::Client,
    url: &str,
    partial: &Path,
    expected_size: u64,
    expected_digest: &str,
) -> Result<()> {
    anyhow::ensure!(
        url.starts_with("https://"),
        "runtime artifact URL must use HTTPS"
    );
    let mut offset = resumable_file_size(partial, expected_size, expected_digest)?;
    if offset == expected_size {
        return Ok(());
    }
    let mut request = client.get(url);
    if offset > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    let response = tokio::time::timeout(DOWNLOAD_IDLE_TIMEOUT, request.send())
        .await
        .context("runtime artifact response headers timed out")?
        .with_context(|| format!("download runtime artifact {url}"))?;
    if offset > 0 && response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        validate_content_range(&response, offset, expected_size)?;
    } else if offset > 0 && response.status().is_success() {
        offset = 0;
    }
    let response = response
        .error_for_status()
        .with_context(|| format!("download runtime artifact {url}"))?;
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length == expected_size.saturating_sub(offset),
            "runtime artifact size changed"
        );
    }
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(offset == 0)
        .append(offset > 0)
        .open(partial)
        .await
        .with_context(|| format!("open partial runtime artifact {}", partial.display()))?;
    let transfer = async {
        let mut stream = response.bytes_stream();
        let mut hasher = hash_partial_prefix(partial, offset)?;
        let mut size = offset;
        loop {
            let next = tokio::time::timeout(DOWNLOAD_IDLE_TIMEOUT, stream.next())
                .await
                .context("runtime artifact download stalled")?;
            let Some(chunk) = next else { break };
            let chunk = chunk.context("read runtime artifact chunk")?;
            size = size
                .checked_add(chunk.len() as u64)
                .context("runtime artifact size overflow")?;
            anyhow::ensure!(
                size <= expected_size,
                "runtime artifact exceeds signed size"
            );
            hasher.update(&chunk);
            output
                .write_all(&chunk)
                .await
                .context("write runtime artifact")?;
        }
        output.sync_all().await.context("sync runtime artifact")?;
        anyhow::ensure!(size == expected_size, "runtime artifact is truncated");
        let digest = format!("sha256:{}", hex::encode(hasher.finalize()));
        anyhow::ensure!(
            digest == expected_digest,
            "runtime artifact SHA-256 mismatch"
        );
        Result::<()>::Ok(())
    };
    tokio::time::timeout(DOWNLOAD_TOTAL_TIMEOUT, transfer)
        .await
        .context("runtime artifact download exceeded 30 minutes")??;
    Ok(())
}

fn resumable_file_size(path: &Path, expected_size: u64, expected_digest: &str) -> Result<u64> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).context("inspect partial runtime artifact"),
    };
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "partial runtime artifact must be a regular non-symlink file"
    );
    anyhow::ensure!(
        metadata.len() <= expected_size,
        "partial runtime artifact exceeds signed size"
    );
    if metadata.len() == expected_size {
        if hash_regular_file(path, expected_size)? == expected_digest {
            return Ok(expected_size);
        }
        fs::remove_file(path).context("remove corrupt completed runtime download")?;
        return Ok(0);
    }
    Ok(metadata.len())
}

fn hash_partial_prefix(path: &Path, expected_size: u64) -> Result<Sha256> {
    if expected_size == 0 {
        return Ok(Sha256::new());
    }
    let mut file = fs::File::open(path).context("open partial runtime artifact")?;
    let metadata = file
        .metadata()
        .context("inspect partial runtime artifact")?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() == expected_size,
        "partial runtime artifact changed before resume"
    );
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .context("hash partial runtime artifact")?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("partial runtime artifact size overflow")?;
        anyhow::ensure!(size <= expected_size, "partial runtime artifact grew");
        hasher.update(&buffer[..read]);
    }
    anyhow::ensure!(
        size == expected_size,
        "partial runtime artifact was truncated"
    );
    Ok(hasher)
}

fn validate_content_range(
    response: &reqwest::Response,
    offset: u64,
    expected_size: u64,
) -> Result<()> {
    let value = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .context("resumed runtime download omitted Content-Range")?
        .to_str()
        .context("runtime Content-Range is not ASCII")?;
    validate_content_range_value(value, offset, expected_size)
}

fn validate_content_range_value(value: &str, offset: u64, expected_size: u64) -> Result<()> {
    let value = value
        .strip_prefix("bytes ")
        .context("runtime Content-Range has an invalid unit")?;
    let (range, total) = value
        .split_once('/')
        .context("runtime Content-Range is malformed")?;
    let (start, end) = range
        .split_once('-')
        .context("runtime Content-Range is malformed")?;
    let start = start
        .parse::<u64>()
        .context("runtime Content-Range start is invalid")?;
    let end = end
        .parse::<u64>()
        .context("runtime Content-Range end is invalid")?;
    let total = total
        .parse::<u64>()
        .context("runtime Content-Range total is invalid")?;
    anyhow::ensure!(
        start == offset && total == expected_size && end == expected_size.saturating_sub(1),
        "runtime download returned an unexpected Content-Range"
    );
    Ok(())
}

fn installed_bundle_complete(root: &Path, installation: &RuntimeInstallationV1) -> bool {
    bundle_directory(root, installation.sequence, &installation.version)
        .join(".complete")
        .is_file()
}

fn verify_installed_bundle_at(
    root: &Path,
    installation: &RuntimeInstallationV1,
) -> Result<RuntimeBundleManifestV1> {
    verify_runtime_bundle_at(root, installation, true)
}

fn verify_runtime_bundle_at(
    root: &Path,
    installation: &RuntimeInstallationV1,
    require_artifact_permissions: bool,
) -> Result<RuntimeBundleManifestV1> {
    installation.validate().map_err(anyhow::Error::from)?;
    let bundle = bundle_directory(root, installation.sequence, &installation.version);
    ensure_runtime_child(root, &bundle)?;
    let metadata = fs::symlink_metadata(&bundle).context("inspect installed runtime bundle")?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "runtime bundle must be a non-symlink directory"
    );
    let manifest_bytes = read_bounded_regular(
        &bundle.join("manifest.json"),
        MAX_MANIFEST_BYTES as u64,
        "runtime manifest",
    )?;
    let signature_bytes = read_bounded_regular(
        &bundle.join("manifest.json.minisig"),
        MAX_SIGNATURE_BYTES as u64,
        "runtime signature",
    )?;
    verify_signature(&manifest_bytes, &signature_bytes)?;
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(&manifest_bytes)));
    anyhow::ensure!(
        digest == installation.bundle_sha256,
        "installed runtime manifest digest changed"
    );
    let manifest: RuntimeBundleManifestV1 =
        serde_json::from_slice(&manifest_bytes).context("parse installed runtime manifest")?;
    manifest.validate(Utc::now()).map_err(anyhow::Error::from)?;
    host_version::ensure_runtime_supported(&manifest)?;
    anyhow::ensure!(
        manifest.sequence == installation.sequence
            && manifest.version == installation.version
            && manifest.target == installation.target,
        "installed runtime manifest does not match current state"
    );
    let mut expected = std::collections::HashSet::from([
        "manifest.json".to_owned(),
        "manifest.json.minisig".to_owned(),
        ".complete".to_owned(),
    ]);
    for artifact in &manifest.artifacts {
        expected.insert(artifact.file_name.clone());
        let path = bundle.join(&artifact.file_name);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect runtime artifact {}", artifact.file_name))?;
        anyhow::ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() == artifact.size,
            "runtime artifact size or type changed: {}",
            artifact.file_name
        );
        if require_artifact_permissions {
            verify_runtime_artifact_permissions(&metadata, artifact.kind)?;
        }
        let actual = hash_regular_file(&path, artifact.size)?;
        anyhow::ensure!(
            actual == artifact.sha256,
            "runtime artifact digest changed: {}",
            artifact.file_name
        );
    }
    let completion =
        read_bounded_regular(&bundle.join(".complete"), 256, "runtime completion marker")?;
    anyhow::ensure!(
        completion == format!("{}\n", installation.bundle_sha256).as_bytes(),
        "runtime completion marker changed"
    );
    for entry in fs::read_dir(&bundle).context("list installed runtime bundle")? {
        let entry = entry.context("read installed runtime entry")?;
        let name = entry.file_name().into_string().map_err(|_| {
            RuntimeError::AssetVerificationFailed(
                "runtime bundle contains a non-Unicode file name".into(),
            )
        })?;
        anyhow::ensure!(
            expected.remove(&name),
            "runtime bundle contains an undeclared file: {name}"
        );
    }
    anyhow::ensure!(
        expected.is_empty(),
        "runtime bundle is missing declared files"
    );
    Ok(manifest)
}

fn packaged_seed_path() -> Result<Option<PathBuf>> {
    if let Some(value) = std::env::var_os("REPORCH_RUNTIME_SEED") {
        let path = PathBuf::from(value);
        anyhow::ensure!(path.is_absolute(), "REPORCH_RUNTIME_SEED must be absolute");
        return Ok(Some(path));
    }
    let Some(target) = HostTarget::current() else {
        return Ok(None);
    };
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        let adjacent = directory.join("runtime").join(target_name(target));
        if adjacent.join("current.json").is_file() {
            return Ok(Some(adjacent));
        }
    }
    #[cfg(target_os = "macos")]
    return Ok(Some(
        PathBuf::from("/Library/Application Support/Reporch/RuntimeSeed").join(target_name(target)),
    ));
    #[cfg(not(target_os = "macos"))]
    {
        let _ = target;
        Ok(None)
    }
}

fn import_packaged_seed_at(seed: &Path, root: &Path) -> Result<bool> {
    ensure_runtime_root_is_narrow(root)?;
    if read_installation(root)?.is_some() {
        return Ok(false);
    }
    let seed_metadata = fs::symlink_metadata(seed).context("inspect packaged runtime seed")?;
    anyhow::ensure!(
        seed.is_absolute() && seed_metadata.is_dir() && !seed_metadata.file_type().is_symlink(),
        "packaged runtime seed must be an absolute non-symlink directory"
    );
    let installation =
        read_installation(seed)?.context("packaged runtime seed has no installation record")?;
    let manifest = verify_runtime_bundle_at(seed, &installation, false)
        .context("verify packaged runtime seed")?;
    let current_target = HostTarget::current().context("unsupported runtime host target")?;
    anyhow::ensure!(
        installation.target == current_target,
        "packaged runtime seed target does not match this host"
    );

    create_private_directory(root)?;
    if read_installation(root)?.is_some() {
        return Ok(false);
    }
    let bundles = root.join("bundles");
    create_private_directory(&bundles)?;
    let destination = bundle_directory(root, installation.sequence, &installation.version);
    if destination.exists() {
        return Ok(false);
    }
    let staging = root.join(format!(".seed-import-{}", Uuid::now_v7()));
    create_private_directory(&staging)?;
    let source = bundle_directory(seed, installation.sequence, &installation.version);
    let import = (|| -> Result<()> {
        for name in ["manifest.json", "manifest.json.minisig", ".complete"] {
            let path = staging.join(name);
            copy_seed_regular(&source.join(name), &path)?;
            set_runtime_artifact_permissions(&path, RuntimeArtifactKindV1::Kernel)?;
        }
        for artifact in &manifest.artifacts {
            let path = staging.join(&artifact.file_name);
            copy_seed_regular(&source.join(&artifact.file_name), &path)?;
            set_runtime_artifact_permissions(&path, artifact.kind)?;
        }
        fs::rename(&staging, &destination).context("atomically install packaged runtime bundle")?;
        if let Err(error) = write_json_atomic(&root.join("current.json"), &installation) {
            let _ = fs::remove_dir_all(&destination);
            return Err(error).context("commit packaged runtime installation state");
        }
        if let Err(error) = verify_installed_bundle_at(root, &installation) {
            let _ = fs::remove_file(root.join("current.json"));
            let _ = fs::remove_dir_all(&destination);
            return Err(error).context("verify imported packaged runtime seed");
        }
        Ok(())
    })();
    if import.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    import?;
    Ok(true)
}

fn copy_seed_regular(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("inspect packaged runtime file {}", source.display()))?;
    anyhow::ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() > 0
            && metadata.len() <= 4 * 1_073_741_824,
        "packaged runtime file must be a bounded regular non-symlink file"
    );
    let mut input = fs::File::open(source).context("open packaged runtime file")?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .context("create imported runtime file")?;
    let copied = std::io::copy(&mut input, &mut output).context("copy packaged runtime file")?;
    anyhow::ensure!(
        copied == metadata.len(),
        "packaged runtime file changed while copying"
    );
    output.sync_all().context("sync imported runtime file")?;
    Ok(())
}

fn read_bounded_regular(path: &Path, limit: u64, label: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() > 0
            && metadata.len() <= limit,
        "{label} must be a bounded regular non-symlink file"
    );
    let bytes = fs::read(path).with_context(|| format!("read {label} {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() as u64 == metadata.len(),
        "{label} changed while being read"
    );
    Ok(bytes)
}

fn hash_regular_file(path: &Path, expected_size: u64) -> Result<String> {
    let file = fs::File::open(path)
        .with_context(|| format!("open runtime artifact {}", path.display()))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut size = 0_u64;
    loop {
        let read = reader.read(&mut buffer).context("hash runtime artifact")?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("runtime artifact size overflow")?;
        anyhow::ensure!(
            size <= expected_size,
            "runtime artifact grew while being verified"
        );
        hasher.update(&buffer[..read]);
    }
    anyhow::ensure!(
        size == expected_size,
        "runtime artifact changed while being verified"
    );
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn verify_spool_object(path: &Path, expected_size: u64, expected_digest: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("inspect runtime spool object")?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() == expected_size,
        "runtime spool object size or type changed"
    );
    let actual = hash_regular_file(path, expected_size)?;
    anyhow::ensure!(
        actual == expected_digest,
        "runtime spool object digest changed"
    );
    Ok(())
}

fn verify_project_input(
    project_root: &Path,
    input: &reporch_runtime_protocol::ContentObjectV1,
) -> Result<()> {
    let mut source = open_project_input(project_root, &input.path)?;
    let metadata = source
        .metadata()
        .context("inspect opened runtime project input")?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() == input.size,
        "runtime project input size or type changed"
    );
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .context("hash runtime project input")?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("runtime project input size overflow")?;
        anyhow::ensure!(size <= input.size, "runtime project input grew");
        hasher.update(&buffer[..read]);
    }
    anyhow::ensure!(size == input.size, "runtime project input was truncated");
    let actual = format!("sha256:{}", hex::encode(hasher.finalize()));
    anyhow::ensure!(
        actual == input.sha256,
        "runtime project input digest changed"
    );
    Ok(())
}

fn set_private_read_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o400))
            .context("restrict runtime spool object")?;
    }
    #[cfg(windows)]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn repair_toolchain_read_access(toolchain: &VerifiedToolchainBundleV2) -> Result<()> {
    let directory = toolchain
        .path
        .parent()
        .context("installed toolchain image has no bundle directory")?;
    set_toolchain_read_only(&toolchain.path)?;
    set_toolchain_read_only(&directory.join("index.json"))?;
    set_toolchain_read_only(&directory.join("index.json.minisig"))?;
    Ok(())
}

fn set_toolchain_read_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let metadata = fs::symlink_metadata(path).context("inspect installed toolchain file")?;
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "installed toolchain file must be a regular non-symlink file"
        );
        #[cfg(target_os = "linux")]
        let system_service = rustix::process::getuid().is_root();
        #[cfg(not(target_os = "linux"))]
        let system_service = false;
        if system_service {
            anyhow::ensure!(
                metadata.uid() == 0,
                "system toolchain file must remain root-owned"
            );
            #[cfg(target_os = "linux")]
            {
                let service_gid = rustix::process::getgid();
                if metadata.gid() != service_gid.as_raw() {
                    rustix::fs::chown(path, None, Some(service_gid))
                        .context("assign toolchain file to the broker group")?;
                }
            }
        }
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(toolchain_read_only_mode(system_service)),
        )
        .context("set installed toolchain read permissions")?;
    }
    #[cfg(windows)]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(unix)]
const fn toolchain_read_only_mode(system_service: bool) -> u32 {
    if system_service { 0o440 } else { 0o400 }
}

fn set_runtime_artifact_permissions(path: &Path, kind: RuntimeArtifactKindV1) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let executable = matches!(
            kind,
            RuntimeArtifactKindV1::GuestAgent
                | RuntimeArtifactKindV1::HostService
                | RuntimeArtifactKindV1::VirtualMachineMonitor
                | RuntimeArtifactKindV1::Jailer
        );
        let mode = if executable { 0o555 } else { 0o444 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .context("set runtime artifact permissions")?;
    }
    #[cfg(windows)]
    {
        let _ = kind;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn verify_runtime_artifact_permissions(
    metadata: &fs::Metadata,
    kind: RuntimeArtifactKindV1,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = metadata.permissions().mode();
        let executable = matches!(
            kind,
            RuntimeArtifactKindV1::GuestAgent
                | RuntimeArtifactKindV1::HostService
                | RuntimeArtifactKindV1::VirtualMachineMonitor
                | RuntimeArtifactKindV1::Jailer
        );
        anyhow::ensure!(mode & 0o222 == 0, "runtime artifact became writable");
        anyhow::ensure!(
            executable == (mode & 0o111 != 0),
            "runtime artifact executable mode changed"
        );
    }
    #[cfg(windows)]
    {
        let _ = (metadata, kind);
    }
    Ok(())
}

fn bundle_directory(root: &Path, sequence: u64, version: &str) -> PathBuf {
    let safe_version: String = version
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    root.join("bundles")
        .join(format!("{sequence}-{safe_version}"))
}

fn ensure_runtime_root_is_narrow(root: &Path) -> Result<()> {
    anyhow::ensure!(root.is_absolute(), "runtime root must be absolute");
    anyhow::ensure!(
        root.components().count() >= 4,
        "runtime root is unexpectedly broad"
    );
    if root.exists() {
        let metadata = fs::symlink_metadata(root).context("inspect runtime root")?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "runtime root must be a non-symlink directory"
        );
    }
    Ok(())
}

fn ensure_runtime_child(root: &Path, child: &Path) -> Result<()> {
    anyhow::ensure!(
        child.starts_with(root) && child != root,
        "runtime child path escaped its root"
    );
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("create runtime directory {}", path.display()))?;
    let metadata = fs::symlink_metadata(path).context("inspect runtime directory")?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "runtime directory must not be a symlink"
    );
    #[cfg(target_os = "linux")]
    if rustix::process::getuid().is_root() {
        use std::os::unix::fs::MetadataExt as _;
        anyhow::ensure!(
            metadata.uid() == 0,
            "system runtime directory must remain root-owned"
        );
        let service_gid = rustix::process::getgid();
        if !service_gid.is_root() && metadata.gid() != service_gid.as_raw() {
            rustix::fs::chown(path, None, Some(service_gid))
                .context("assign runtime directory to the broker group")?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        #[cfg(target_os = "linux")]
        let mode = if rustix::process::getuid().is_root() {
            0o750
        } else {
            0o700
        };
        #[cfg(not(target_os = "linux"))]
        let mode = 0o700;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .context("restrict runtime directory permissions")?;
    }
    Ok(())
}

fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("runtime state has no parent")?;
    create_private_directory(parent)?;
    let temporary = parent.join(format!(".state-{}.tmp", Uuid::now_v7()));
    let backup = parent.join(format!(".state-{}.old", Uuid::now_v7()));
    let bytes = serde_json::to_vec_pretty(value).context("serialize runtime state")?;
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .context("create temporary runtime state")?;
        use std::io::Write as _;
        file.write_all(&bytes)
            .context("write temporary runtime state")?;
        file.sync_all().context("sync temporary runtime state")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            #[cfg(target_os = "linux")]
            let mode = if rustix::process::getuid().is_root() {
                0o640
            } else {
                0o600
            };
            #[cfg(not(target_os = "linux"))]
            let mode = 0o600;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))
                .context("restrict runtime state permissions")?;
        }
        let had_previous = path.exists();
        if had_previous {
            fs::rename(path, &backup).context("move previous runtime state aside")?;
        }
        if let Err(error) = fs::rename(&temporary, path) {
            if had_previous {
                let _ = fs::rename(&backup, path);
            }
            return Err(error).context("atomically install runtime state");
        }
        if had_previous {
            let _ = fs::remove_file(&backup);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub const fn target_name(target: HostTarget) -> &'static str {
    match target {
        HostTarget::DarwinArm64 => "darwin-arm64",
        HostTarget::DarwinX64 => "darwin-x64",
        HostTarget::LinuxArm64Gnu => "linux-arm64-gnu",
        HostTarget::LinuxX64Gnu => "linux-x64-gnu",
        HostTarget::WindowsX64Msvc => "windows-x64-msvc",
    }
}

fn required_env(name: &str) -> Result<OsString> {
    std::env::var_os(name).with_context(|| {
        format!("{name} is unavailable; set REPORCH_RUNTIME_HOME to an absolute path")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use reporch_runtime_protocol::{
        ContentObjectV1, GuestHandshakeV1, GuestOperationV1, GuestOutputV1, GuestResultV1,
        HANDSHAKE_SCHEMA, JOB_SCHEMA, PROTOCOL_VERSION, RESULT_SCHEMA, ResourceLimitsV1,
    };
    #[cfg(unix)]
    use std::collections::BTreeMap;

    const SIGNED_FIXTURE: &[u8] =
        include_bytes!("../../../artifacts/runtime-signature-fixture.json");
    const SIGNED_FIXTURE_SIGNATURE: &[u8] =
        include_bytes!("../../../artifacts/runtime-signature-fixture.json.minisig");

    #[tokio::test]
    async fn missing_installation_is_reported_without_creating_files() {
        let root = tempfile::tempdir().unwrap();
        let status = status_at(root.path()).await.unwrap();
        assert_eq!(status.availability, RuntimeAvailability::NotInstalled);
        assert!(status.installed_version.is_none());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn runtime_trust_root_rejects_any_manifest_mutation() {
        verify_signature(SIGNED_FIXTURE, SIGNED_FIXTURE_SIGNATURE).unwrap();
        let mut changed = SIGNED_FIXTURE.to_vec();
        changed[0] ^= 1;
        assert!(verify_signature(&changed, SIGNED_FIXTURE_SIGNATURE).is_err());
    }

    #[test]
    fn runtime_and_toolchain_channels_are_distinct_immutable_releases() {
        assert_eq!(
            parse_channel_url(RUNTIME_CHANNEL_BASE, "runtime")
                .unwrap()
                .as_str(),
            "https://github.com/Reporch/cli/releases/download/reporch-runtime-v1-seq24/"
        );
        assert_eq!(
            parse_channel_url(TOOLCHAIN_CHANNEL_BASE, "toolchain")
                .unwrap()
                .as_str(),
            "https://github.com/Reporch/cli/releases/download/reporch-toolchains-v2-seq8/"
        );
    }

    #[test]
    fn channel_urls_reject_credentials_and_insecure_transports() {
        for value in [
            "http://github.com/Reporch/cli/releases/download/test",
            "https://token@github.com/Reporch/cli/releases/download/test",
            "https://github.com/Reporch/cli/releases/download/test?asset=1",
            "https://github.com/Reporch/cli/releases/download/test#fragment",
        ] {
            assert!(parse_channel_url(value, "toolchain").is_err(), "{value}");
        }
    }

    #[test]
    fn toolchain_archive_expansion_is_bounded_and_digest_verified() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("toolchain.ext4.zst");
        let mut payload = vec![0_u8; 4 * 1024 * 1024];
        payload[..16].copy_from_slice(b"reporch-fixture!");
        assert_eq!(
            payload
                .chunks(64 * 1024)
                .filter(|block| block.iter().all(|byte| *byte == 0))
                .count(),
            63
        );
        let output = root.path().join("toolchain.ext4");
        let mut encoder =
            zstd::stream::write::Encoder::new(fs::File::create(&archive).unwrap(), 9).unwrap();
        encoder.include_checksum(true).unwrap();
        encoder.write_all(&payload).unwrap();
        encoder.finish().unwrap().sync_all().unwrap();
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(&payload)));
        expand_toolchain_archive(
            &archive,
            &output,
            ToolchainCompressionV2::Zstd,
            payload.len() as u64,
            &digest,
        )
        .unwrap();
        assert_eq!(fs::read(&output).unwrap(), payload);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = fs::metadata(&output).unwrap();
            assert!(
                metadata.blocks() * 512 < metadata.len() / 2,
                "sparse image allocated {} of {} bytes",
                metadata.blocks() * 512,
                metadata.len()
            );
        }

        let rejected = root.path().join("rejected.ext4");
        assert!(
            expand_toolchain_archive(
                &archive,
                &rejected,
                ToolchainCompressionV2::Zstd,
                1024,
                &digest,
            )
            .is_err()
        );
        assert!(!rejected.exists());
    }

    #[cfg(unix)]
    #[test]
    fn sparse_writer_never_allocates_zero_extents() {
        use std::os::unix::fs::MetadataExt as _;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sparse.img");
        let mut file = fs::File::create(&path).unwrap();
        let mut payload = vec![0_u8; 4 * 1024 * 1024];
        payload[..16].copy_from_slice(b"reporch-fixture!");
        write_sparse(&mut file, &payload).unwrap();
        let before = file.metadata().unwrap();
        assert_eq!(before.len(), 64 * 1024);
        assert!(
            before.blocks() * 512 < 1024 * 1024,
            "sparse writer preallocated {} bytes before truncate",
            before.blocks() * 512
        );
        file.sync_data().unwrap();
        drop(file);
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        set_sparse_len(&file, payload.len() as u64).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let metadata = fs::metadata(path).unwrap();
        assert!(
            metadata.blocks() * 512 < metadata.len() / 2,
            "direct sparse writer allocated {} of {} bytes",
            metadata.blocks() * 512,
            metadata.len()
        );
    }

    #[test]
    fn runtime_state_replacement_is_atomic_and_bounded_to_its_parent() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("current.json");
        write_json_atomic(&path, &serde_json::json!({ "version": 1 })).unwrap();
        write_json_atomic(&path, &serde_json::json!({ "version": 2 })).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["version"], 2);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn installed_toolchains_are_read_only_for_the_owner_and_system_broker_group() {
        assert_eq!(toolchain_read_only_mode(false), 0o400);
        assert_eq!(toolchain_read_only_mode(true), 0o440);
    }

    #[tokio::test]
    async fn runtime_status_never_reports_an_unverified_bundle_ready() {
        let root = tempfile::tempdir().unwrap();
        let installation = RuntimeInstallationV1 {
            schema: INSTALLATION_SCHEMA.into(),
            sequence: 8,
            version: "1.0.0-rc.8".into(),
            target: HostTarget::current().unwrap(),
            bundle_sha256: format!("sha256:{}", "a".repeat(64)),
            installed_at: Utc::now(),
        };
        let bundle = bundle_directory(root.path(), installation.sequence, &installation.version);
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join(".complete"), b"complete\n").unwrap();
        write_json_atomic(&root.path().join("current.json"), &installation).unwrap();

        let status = status_at(root.path()).await.unwrap();

        assert_eq!(status.availability, RuntimeAvailability::Broken);
        assert!(!status.service_available);
        assert!(
            status
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("Runtime asset verification failed")),
            "{:?}",
            status.reason
        );
    }

    #[test]
    fn installation_lock_serializes_processes_and_survives_a_persistent_lock_file() {
        let root = tempfile::tempdir().unwrap();
        let lock_path = root.path().join("runtime-v1.lock");
        let first = acquire_installation_lock_blocking(&lock_path).unwrap();
        let second_file = open_installation_lock(&lock_path).unwrap();
        let error = fs2::FileExt::try_lock_exclusive(&second_file).unwrap_err();
        assert!(installation_lock_is_contended(&error), "{error:?}");
        drop(first);
        fs2::FileExt::try_lock_exclusive(&second_file).unwrap();
        fs2::FileExt::unlock(&second_file).unwrap();
        assert!(lock_path.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn installation_lock_never_follows_a_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::write(&target, b"unrelated").unwrap();
        let lock_path = root.path().join("runtime-v1.lock");
        symlink(&target, &lock_path).unwrap();
        assert!(open_installation_lock(&lock_path).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"unrelated");
    }

    #[test]
    fn resumable_download_state_keeps_partial_bytes_and_rejects_wrong_complete_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("artifact.part");
        let complete = b"complete-runtime-asset";
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(complete)));

        fs::write(&path, &complete[..8]).unwrap();
        assert_eq!(
            resumable_file_size(&path, complete.len() as u64, &digest).unwrap(),
            8
        );

        fs::write(&path, vec![b'x'; complete.len()]).unwrap();
        assert_eq!(
            resumable_file_size(&path, complete.len() as u64, &digest).unwrap(),
            0
        );
        assert!(!path.exists());

        fs::write(&path, complete).unwrap();
        assert_eq!(
            resumable_file_size(&path, complete.len() as u64, &digest).unwrap(),
            complete.len() as u64
        );
    }

    #[test]
    fn resumed_download_requires_the_exact_remaining_content_range() {
        validate_content_range_value("bytes 8-20/21", 8, 21).unwrap();
        for value in [
            "bytes 7-20/21",
            "bytes 8-19/21",
            "bytes 8-20/22",
            "items 8-20/21",
        ] {
            assert!(validate_content_range_value(value, 8, 21).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn resumable_download_never_accepts_a_symlink_partial() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::write(&target, b"secret").unwrap();
        let path = root.path().join("artifact.part");
        symlink(&target, &path).unwrap();
        assert!(resumable_file_size(&path, 6, &format!("sha256:{}", "a".repeat(64))).is_err());
        assert_eq!(fs::read(target).unwrap(), b"secret");
    }

    #[test]
    fn failed_boot_rollback_restores_state_and_the_replaced_bundle() {
        let root = tempfile::tempdir().unwrap();
        let failed = root.path().join("bundles/1-current");
        let replaced = root.path().join("replaced");
        fs::create_dir_all(&failed).unwrap();
        fs::write(failed.join("new"), b"new").unwrap();
        fs::create_dir(&replaced).unwrap();
        fs::write(replaced.join("old"), b"old").unwrap();
        let previous = RuntimeInstallationV1 {
            schema: INSTALLATION_SCHEMA.into(),
            sequence: 1,
            version: "previous".into(),
            target: HostTarget::current().unwrap(),
            bundle_sha256: format!("sha256:{}", "a".repeat(64)),
            installed_at: Utc::now(),
        };
        write_json_atomic(
            &root.path().join("current.json"),
            &serde_json::json!({
                "failed": true
            }),
        )
        .unwrap();

        rollback_failed_install(root.path(), Some(&previous), &failed, Some(&replaced)).unwrap();
        let restored: RuntimeInstallationV1 =
            serde_json::from_slice(&fs::read(root.path().join("current.json")).unwrap()).unwrap();
        assert_eq!(restored, previous);
        assert_eq!(fs::read(failed.join("old")).unwrap(), b"old");
        assert!(!failed.join("new").exists());
        assert!(!replaced.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn guest_exchange_binds_handshake_and_streams_verified_inputs() {
        let root = tempfile::tempdir().unwrap();
        let contents = b"answer\n";
        fs::create_dir(root.path().join("tests")).unwrap();
        fs::write(root.path().join("tests/01.in"), contents).unwrap();
        let bundle_digest = format!("sha256:{}", "b".repeat(64));
        let job = GuestJobV1 {
            schema: JOB_SCHEMA.into(),
            protocol_version: PROTOCOL_VERSION,
            id: Uuid::now_v7(),
            nonce: "nonce-0123456789abcdef".into(),
            operation: GuestOperationV1::Program,
            toolchain_id: "python-3.14".into(),
            toolchain_index_sequence: Some(1),
            toolchain_bundle_sha256: Some(format!("sha256:{}", "c".repeat(64))),
            toolchain_lock_sha256: Some(format!("sha256:{}", "d".repeat(64))),
            command: vec!["python3".into(), "solution.py".into()],
            environment: BTreeMap::new(),
            inputs: vec![ContentObjectV1 {
                path: "tests/01.in".into(),
                sha256: format!("sha256:{}", hex::encode(Sha256::digest(contents))),
                size: contents.len() as u64,
            }],
            limits: ResourceLimitsV1 {
                timeout_ms: 1_000,
                memory_mib: 64,
                cpu_millis: 1_000,
                pids: 16,
                stdout_bytes: 1_024,
                stderr_bytes: 1_024,
                artifact_bytes: 1_024,
            },
        };
        let guest_job = job.clone();
        let guest_bundle = bundle_digest.clone();
        let (mut host_stream, mut guest_stream) = tokio::io::duplex(64 * 1024);
        let guest = tokio::spawn(async move {
            let handshake = GuestHandshakeV1 {
                schema: HANDSHAKE_SCHEMA.into(),
                protocol_version: PROTOCOL_VERSION,
                guest_version: "test".into(),
                runtime_bundle_digest: guest_bundle,
                nonce: guest_job.nonce.clone(),
            };
            write_wire_message(&mut guest_stream, &WireMessageV1::Handshake(handshake))
                .await
                .unwrap();
            assert!(matches!(
                read_wire_message(&mut guest_stream).await.unwrap(),
                WireMessageV1::Job(_)
            ));
            let mut received = Vec::new();
            loop {
                let WireMessageV1::InputChunk(chunk) =
                    read_wire_message(&mut guest_stream).await.unwrap()
                else {
                    panic!("expected input chunk");
                };
                received.extend_from_slice(&chunk.bytes);
                if chunk.eof {
                    break;
                }
            }
            assert_eq!(received, contents);
            let result = GuestResultV1 {
                schema: RESULT_SCHEMA.into(),
                protocol_version: PROTOCOL_VERSION,
                job_id: guest_job.id,
                nonce: guest_job.nonce,
                exit_code: 0,
                termination: reporch_runtime_protocol::GuestTerminationV2::Exited,
                duration_ms: 1,
                stdout: GuestOutputV1::from_bytes(b"ok", false),
                stderr: GuestOutputV1::from_bytes(b"", false),
                artifacts: Vec::new(),
            };
            write_wire_message(&mut guest_stream, &WireMessageV1::Result(result))
                .await
                .unwrap();
        });
        let result = exchange_with_guest(&mut host_stream, root.path(), &job, &bundle_digest)
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        guest.await.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn challenged_sync_exchange_authenticates_before_sending_a_job() {
        let root = tempfile::tempdir().unwrap();
        let bundle_digest = format!("sha256:{}", "b".repeat(64));
        let job = GuestJobV1 {
            schema: JOB_SCHEMA.into(),
            protocol_version: PROTOCOL_VERSION,
            id: Uuid::now_v7(),
            nonce: "nonce-0123456789abcdef".into(),
            operation: GuestOperationV1::Program,
            toolchain_id: "runtime-self-test".into(),
            toolchain_index_sequence: None,
            toolchain_bundle_sha256: None,
            toolchain_lock_sha256: None,
            command: vec!["/sbin/reporch-guestd".into(), "--self-test-workload".into()],
            environment: BTreeMap::new(),
            inputs: Vec::new(),
            limits: ResourceLimitsV1 {
                timeout_ms: 1_000,
                memory_mib: 64,
                cpu_millis: 1_000,
                pids: 16,
                stdout_bytes: 1_024,
                stderr_bytes: 1_024,
                artifact_bytes: 1_024,
            },
        };
        let guest_job = job.clone();
        let guest_bundle = bundle_digest.clone();
        let (mut host_stream, mut guest_stream) = std::os::unix::net::UnixStream::pair().unwrap();
        let guest = std::thread::spawn(move || {
            let challenge = match read_wire_message_sync(&mut guest_stream).unwrap() {
                WireMessageV1::HostChallenge(challenge) => challenge,
                message => panic!("expected host challenge, got {message:?}"),
            };
            challenge.validate().unwrap();
            assert_eq!(challenge.nonce, guest_job.nonce);
            assert_eq!(challenge.runtime_bundle_digest, guest_bundle);
            let handshake = GuestHandshakeV1 {
                schema: HANDSHAKE_SCHEMA.into(),
                protocol_version: PROTOCOL_VERSION,
                guest_version: "test".into(),
                runtime_bundle_digest: challenge.runtime_bundle_digest,
                nonce: challenge.nonce,
            };
            write_wire_message_sync(&mut guest_stream, &WireMessageV1::Handshake(handshake))
                .unwrap();
            assert!(matches!(
                read_wire_message_sync(&mut guest_stream).unwrap(),
                WireMessageV1::Job(_)
            ));
            let result = GuestResultV1 {
                schema: RESULT_SCHEMA.into(),
                protocol_version: PROTOCOL_VERSION,
                job_id: guest_job.id,
                nonce: guest_job.nonce,
                exit_code: 0,
                termination: reporch_runtime_protocol::GuestTerminationV2::Exited,
                duration_ms: 1,
                stdout: GuestOutputV1::from_bytes(b"ok", false),
                stderr: GuestOutputV1::from_bytes(b"", false),
                artifacts: Vec::new(),
            };
            write_wire_message_sync(&mut guest_stream, &WireMessageV1::Result(result)).unwrap();
        });

        establish_guest_session_sync_challenged(&mut host_stream, &job, &bundle_digest).unwrap();
        let result = exchange_job_with_guest_sync(&mut host_stream, root.path(), &job).unwrap();
        assert_eq!(result.exit_code, 0);
        guest.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn job_spool_is_content_addressed_reusable_and_symlink_safe() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let spool = tempfile::tempdir().unwrap();
        let contents = b"case\n";
        fs::create_dir(project.path().join("tests")).unwrap();
        fs::write(project.path().join("tests/01.in"), contents).unwrap();
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(contents)));
        let mut job = GuestJobV1 {
            schema: JOB_SCHEMA.into(),
            protocol_version: PROTOCOL_VERSION,
            id: Uuid::now_v7(),
            nonce: "nonce-0123456789abcdef".into(),
            operation: GuestOperationV1::Program,
            toolchain_id: "python-3.14".into(),
            toolchain_index_sequence: Some(1),
            toolchain_bundle_sha256: Some(format!("sha256:{}", "c".repeat(64))),
            toolchain_lock_sha256: Some(format!("sha256:{}", "d".repeat(64))),
            command: vec!["python3".into(), "solution.py".into()],
            environment: BTreeMap::new(),
            inputs: vec![ContentObjectV1 {
                path: "tests/01.in".into(),
                sha256: digest.clone(),
                size: contents.len() as u64,
            }],
            limits: ResourceLimitsV1 {
                timeout_ms: 1_000,
                memory_mib: 64,
                cpu_millis: 1_000,
                pids: 16,
                stdout_bytes: 1_024,
                stderr_bytes: 1_024,
                artifact_bytes: 1_024,
            },
        };
        let first = stage_job_inputs_at(project.path(), &job, spool.path()).unwrap();
        assert_eq!(first.object_count, 1);
        assert_eq!(first.reused_objects, 0);
        let second = stage_job_inputs_at(project.path(), &job, spool.path()).unwrap();
        assert_eq!(second.reused_objects, 1);

        fs::write(project.path().join("outside"), contents).unwrap();
        symlink(
            project.path().join("outside"),
            project.path().join("tests/link.in"),
        )
        .unwrap();
        job.inputs[0].path = "tests/link.in".into();
        assert!(stage_job_inputs_at(project.path(), &job, spool.path()).is_err());
    }
}

#[cfg(test)]
mod download_cache_regression;
