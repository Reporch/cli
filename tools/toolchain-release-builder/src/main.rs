#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::{BufReader, Read as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Duration, Utc};
use reporch_runtime_core::{
    HostTarget, RUNTIME_SIGNING_KEY_ID, TOOLCHAIN_INDEX_SCHEMA, ToolchainBundleV2,
    ToolchainCompressionV2, ToolchainEntryV2, ToolchainFilesystemV2, ToolchainIndexV2,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const MAX_LOCK_BYTES: u64 = 64 * 1024;
const MAX_RECEIPT_BYTES: u64 = 16 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 2 * 1_073_741_824;
const MAX_IMAGE_BYTES: u64 = 8 * 1_073_741_824;

#[derive(Debug)]
struct Arguments {
    lock: PathBuf,
    artifacts: PathBuf,
    source_revision: String,
    base_url: String,
    output_index: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceLock {
    schema: String,
    sequence: u64,
    source_date_epoch: u64,
    entries: Vec<SourceEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceEntry {
    id: String,
    language: String,
    image_mib: u64,
    image: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildReceipt {
    schema: String,
    source_identity: String,
    architecture: String,
    filesystem: String,
    image_sha256: String,
    image_size: u64,
    archive_sha256: String,
    archive_size: u64,
    compression: String,
}

fn main() -> Result<()> {
    let arguments = parse_arguments(std::env::args_os().skip(1))?;
    let index = build_release(&arguments)?;
    println!(
        "{{\"schema\":\"reporch.toolchain-release-build.v2\",\"sequence\":{},\"entries\":{}}}",
        index.sequence,
        index.entries.len()
    );
    Ok(())
}

fn parse_arguments(mut values: impl Iterator<Item = OsString>) -> Result<Arguments> {
    let lock = required_path(&mut values, "toolchain source lock")?;
    let artifacts = required_path(&mut values, "toolchain artifact directory")?;
    let source_revision = required_utf8(&mut values, "source revision")?;
    let base_url = required_utf8(&mut values, "release base URL")?;
    let output_index = required_path(&mut values, "output index")?;
    ensure!(values.next().is_none(), "too many arguments");
    ensure!(
        source_revision.len() == 40
            && source_revision
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "source revision must be a full lowercase Git object ID"
    );
    let parsed = url::Url::parse(&base_url).context("parse release base URL")?;
    ensure!(
        parsed.scheme() == "https"
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "release base URL must be credential-free HTTPS"
    );
    ensure!(base_url.ends_with('/'), "release base URL must end with /");
    Ok(Arguments {
        lock,
        artifacts,
        source_revision,
        base_url,
        output_index,
    })
}

fn build_release(arguments: &Arguments) -> Result<ToolchainIndexV2> {
    let lock_bytes =
        read_bounded_regular(&arguments.lock, MAX_LOCK_BYTES, "toolchain source lock")?;
    let lock: SourceLock =
        serde_json::from_slice(&lock_bytes).context("parse toolchain source lock")?;
    validate_lock(&lock)?;
    let artifacts = canonical_directory(&arguments.artifacts, "toolchain artifact directory")?;
    validate_new_output(&arguments.output_index, &artifacts)?;
    let lock_digest = format!("sha256:{}", hex::encode(Sha256::digest(&lock_bytes)));
    let generated_at = DateTime::<Utc>::from_timestamp(i64::try_from(lock.source_date_epoch)?, 0)
        .context("toolchain source epoch is out of range")?;
    let expires_at = generated_at + Duration::days(90);
    let created = generated_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut entries = Vec::with_capacity(lock.entries.len());
    for entry in &lock.entries {
        entries.push(build_entry(
            entry,
            &artifacts,
            &arguments.base_url,
            &arguments.source_revision,
            &lock_digest,
            lock.source_date_epoch,
            &created,
        )?);
    }
    let index = ToolchainIndexV2 {
        schema: TOOLCHAIN_INDEX_SCHEMA.into(),
        sequence: lock.sequence,
        generated_at,
        expires_at,
        signing_key_id: RUNTIME_SIGNING_KEY_ID.into(),
        entries,
    };
    index
        .validate(generated_at + Duration::seconds(1))
        .map_err(anyhow::Error::from)?;
    let mut bytes = serde_json::to_vec_pretty(&index).context("serialize toolchain index")?;
    bytes.push(b'\n');
    write_new_atomic(&arguments.output_index, &bytes)?;
    Ok(index)
}

#[allow(clippy::too_many_arguments)]
fn build_entry(
    entry: &SourceEntry,
    artifacts: &Path,
    base_url: &str,
    source_revision: &str,
    lock_digest: &str,
    source_date_epoch: u64,
    created: &str,
) -> Result<ToolchainEntryV2> {
    let image_digest = entry
        .image
        .rsplit_once('@')
        .map(|(_, digest)| digest)
        .context("toolchain image is not digest pinned")?;
    let arm64 = bundle(
        entry,
        artifacts,
        base_url,
        source_revision,
        lock_digest,
        source_date_epoch,
        created,
        "linux-arm64",
        "arm64",
        ToolchainFilesystemV2::Ext4,
    )?;
    let x64 = bundle(
        entry,
        artifacts,
        base_url,
        source_revision,
        lock_digest,
        source_date_epoch,
        created,
        "linux-x64",
        "amd64",
        ToolchainFilesystemV2::Ext4,
    )?;
    let windows = bundle(
        entry,
        artifacts,
        base_url,
        source_revision,
        lock_digest,
        source_date_epoch,
        created,
        "windows-x64",
        "amd64",
        ToolchainFilesystemV2::Vhdx,
    )?;
    let retarget = |bundle: &ToolchainBundleV2, target| {
        let mut bundle = bundle.clone();
        bundle.target = target;
        bundle
    };
    Ok(ToolchainEntryV2 {
        id: entry.id.clone(),
        language: entry.language.clone(),
        toolchain_lock_sha256: image_digest.into(),
        studio_oci_image: entry.image.clone(),
        bundles: vec![
            retarget(&arm64, HostTarget::DarwinArm64),
            retarget(&x64, HostTarget::DarwinX64),
            retarget(&arm64, HostTarget::LinuxArm64Gnu),
            retarget(&x64, HostTarget::LinuxX64Gnu),
            retarget(&windows, HostTarget::WindowsX64Msvc),
        ],
    })
}

#[allow(clippy::too_many_arguments)]
fn bundle(
    entry: &SourceEntry,
    artifacts: &Path,
    base_url: &str,
    source_revision: &str,
    lock_digest: &str,
    source_date_epoch: u64,
    created: &str,
    suffix: &str,
    architecture: &str,
    filesystem: ToolchainFilesystemV2,
) -> Result<ToolchainBundleV2> {
    let extension = match filesystem {
        ToolchainFilesystemV2::Ext4 => "ext4",
        ToolchainFilesystemV2::Vhdx => "vhdx",
    };
    let file_name = format!("{}-{suffix}.{extension}", entry.id);
    let archive_file_name = format!("{file_name}.zst");
    let archive = artifacts.join(&archive_file_name);
    let receipt_path = artifacts.join(format!("{archive_file_name}.build.json"));
    let receipt_bytes = read_bounded_regular(&receipt_path, MAX_RECEIPT_BYTES, "build receipt")?;
    let receipt: BuildReceipt =
        serde_json::from_slice(&receipt_bytes).context("parse build receipt")?;
    let source_identity = entry
        .image
        .rsplit_once('@')
        .map(|(_, digest)| digest)
        .context("toolchain image is not digest pinned")?;
    validate_receipt(&receipt, source_identity, architecture, extension)?;
    let archive_digest = hash_regular(&archive, receipt.archive_size, MAX_ARCHIVE_BYTES)?;
    ensure!(
        archive_digest == receipt.archive_sha256,
        "toolchain archive digest mismatch"
    );
    verify_expanded_image(&archive, receipt.image_size, &receipt.image_sha256)?;
    let source_sbom_suffix = if architecture == "arm64" {
        "linux-arm64"
    } else {
        "linux-x64"
    };
    let source_sbom = artifacts.join(format!(
        "{}-{source_sbom_suffix}.source.spdx.json",
        entry.id
    ));
    write_evidence(
        entry,
        &archive,
        &source_sbom,
        &archive_file_name,
        &file_name,
        &receipt,
        architecture,
        source_revision,
        lock_digest,
        source_date_epoch,
        created,
    )?;
    Ok(ToolchainBundleV2 {
        target: HostTarget::DarwinArm64,
        filesystem,
        file_name,
        sha256: receipt.image_sha256,
        size: receipt.image_size,
        archive_file_name: archive_file_name.clone(),
        archive_sha256: receipt.archive_sha256,
        archive_size: receipt.archive_size,
        compression: ToolchainCompressionV2::Zstd,
        source_url: format!("{base_url}{archive_file_name}"),
        sbom_url: format!("{base_url}{archive_file_name}.spdx.json"),
        provenance_url: format!("{base_url}{archive_file_name}.intoto.jsonl"),
    })
}

fn validate_lock(lock: &SourceLock) -> Result<()> {
    ensure!(
        lock.schema == "reporch.toolchain-sources-lock.v1"
            && lock.sequence > 0
            && lock.source_date_epoch > 0
            && lock.entries.len() == 12,
        "invalid toolchain source lock identity"
    );
    let mut ids = HashSet::new();
    for entry in &lock.entries {
        ensure!(
            ids.insert(entry.id.as_str())
                && valid_identifier(&entry.id)
                && valid_identifier(&entry.language),
            "invalid or duplicate toolchain identity"
        );
        ensure!(
            (256..=8_192).contains(&entry.image_mib),
            "invalid toolchain image size"
        );
        let (name, digest) = entry
            .image
            .rsplit_once('@')
            .context("unpinned toolchain image")?;
        ensure!(!name.is_empty(), "toolchain image name is empty");
        validate_sha256(digest)?;
    }
    Ok(())
}

fn validate_receipt(
    receipt: &BuildReceipt,
    source_identity: &str,
    architecture: &str,
    filesystem: &str,
) -> Result<()> {
    ensure!(
        receipt.schema == "reporch.toolchain-bundle-build.v2"
            && receipt.architecture == architecture
            && receipt.source_identity == source_identity
            && receipt.filesystem == filesystem
            && receipt.compression == "zstd"
            && receipt.image_size > 0
            && receipt.image_size <= MAX_IMAGE_BYTES
            && receipt.archive_size > 0
            && receipt.archive_size < receipt.image_size
            && receipt.archive_size <= MAX_ARCHIVE_BYTES,
        "invalid toolchain build receipt"
    );
    validate_sha256(&receipt.image_sha256)?;
    validate_sha256(&receipt.archive_sha256)
}

fn verify_expanded_image(archive: &Path, expected_size: u64, expected_digest: &str) -> Result<()> {
    let input = fs::File::open(archive).context("open toolchain archive")?;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(BufReader::new(input))
        .context("initialize toolchain decoder")?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = decoder
            .read(&mut buffer)
            .context("decompress toolchain image")?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("expanded size overflow")?;
        ensure!(
            size <= expected_size,
            "expanded toolchain image exceeded signed size"
        );
        hasher.update(&buffer[..read]);
    }
    ensure!(
        size == expected_size,
        "expanded toolchain image size mismatch"
    );
    ensure!(
        format!("sha256:{}", hex::encode(hasher.finalize())) == expected_digest,
        "expanded toolchain image digest mismatch"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_evidence(
    entry: &SourceEntry,
    archive: &Path,
    source_sbom: &Path,
    archive_name: &str,
    image_name: &str,
    receipt: &BuildReceipt,
    architecture: &str,
    source_revision: &str,
    lock_digest: &str,
    source_date_epoch: u64,
    created: &str,
) -> Result<()> {
    let image_digest = receipt.image_sha256.trim_start_matches("sha256:");
    let archive_digest = receipt.archive_sha256.trim_start_matches("sha256:");
    let spdx_bytes = read_bounded_regular(source_sbom, 32 * 1024 * 1024, "normalized SPDX SBOM")?;
    let spdx: serde_json::Value =
        serde_json::from_slice(&spdx_bytes).context("parse normalized SPDX SBOM")?;
    let source_identity = entry
        .image
        .rsplit_once('@')
        .map(|(_, value)| value)
        .context("toolchain image is not digest pinned")?;
    ensure!(
        spdx.get("spdxVersion").and_then(serde_json::Value::as_str) == Some("SPDX-2.3")
            && spdx
                .pointer("/creationInfo/created")
                .and_then(serde_json::Value::as_str)
                == Some(created)
            && spdx
                .get("documentNamespace")
                .and_then(serde_json::Value::as_str)
                == Some(
                    format!(
                        "https://reporch.com/spdx/toolchain-source/{}/{architecture}",
                        source_identity.trim_start_matches("sha256:")
                    )
                    .as_str()
                )
            && spdx
                .get("packages")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|packages| {
                    !packages.is_empty()
                        && packages.iter().any(|package| {
                            package
                                .get("licenseDeclared")
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(|license| license != "NOASSERTION")
                        })
                }),
        "normalized SPDX SBOM identity or license inventory is invalid"
    );
    let source_sbom_digest = hex::encode(Sha256::digest(&spdx_bytes));
    write_new_atomic(
        &archive.with_file_name(format!("{archive_name}.spdx.json")),
        &spdx_bytes,
    )?;

    let provenance = serde_json::json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [
            { "name": image_name, "digest": { "sha256": image_digest } },
            { "name": archive_name, "digest": { "sha256": archive_digest } }
        ],
        "predicateType": "https://slsa.dev/provenance/v1",
        "predicate": {
            "buildDefinition": {
                "buildType": "https://reporch.com/buildtypes/toolchain-vm-bundle/v2",
                "externalParameters": {
                    "id": entry.id,
                    "language": entry.language,
                    "studioOciImage": entry.image,
                    "imageSize": receipt.image_size,
                    "archiveSize": receipt.archive_size
                },
                "internalParameters": {
                    "sourceRevision": source_revision,
                    "sourceDateEpoch": source_date_epoch
                },
                "resolvedDependencies": [
                    { "uri": "pkg:github/Reporch/cli", "digest": { "gitCommit": source_revision } },
                    { "uri": "https://github.com/Reporch/cli/blob/main/runtime/toolchains.lock.json", "digest": { "sha256": lock_digest.trim_start_matches("sha256:") } },
                    { "uri": entry.image, "digest": { "sha256": entry.image.rsplit_once("sha256:").map(|(_, value)| value).unwrap_or_default() } },
                    { "uri": format!("reporch:toolchain-sbom/{}/{architecture}", entry.id), "digest": { "sha256": source_sbom_digest } }
                ]
            },
            "runDetails": {
                "builder": { "id": format!("https://github.com/Reporch/cli/tree/{source_revision}/tools/toolchain-release-builder") },
                "metadata": {
                    "invocationId": format!("urn:sha256:{archive_digest}"),
                    "startedOn": created,
                    "finishedOn": created
                }
            }
        }
    });
    let mut provenance_bytes = serde_json::to_vec(&provenance).context("serialize provenance")?;
    provenance_bytes.push(b'\n');
    write_new_atomic(
        &archive.with_file_name(format!("{archive_name}.intoto.jsonl")),
        &provenance_bytes,
    )
}

fn hash_regular(path: &Path, expected_size: u64, maximum: u64) -> Result<String> {
    let metadata = fs::symlink_metadata(path).context("inspect toolchain artifact")?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() == expected_size
            && metadata.len() <= maximum,
        "invalid toolchain artifact type or size"
    );
    let mut file = fs::File::open(path).context("open toolchain artifact")?;
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).context("hash toolchain artifact")?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .context("artifact size overflow")?;
        ensure!(
            copied <= expected_size,
            "toolchain artifact grew while hashing"
        );
        hasher.update(&buffer[..read]);
    }
    ensure!(
        copied == expected_size,
        "toolchain artifact changed while hashing"
    );
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn validate_sha256(value: &str) -> Result<()> {
    let digest = value
        .strip_prefix("sha256:")
        .context("missing SHA-256 prefix")?;
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid SHA-256"
    );
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {label}"))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{label} must be a real directory"
    );
    path.canonicalize()
        .with_context(|| format!("resolve {label}"))
}

fn validate_new_output(path: &Path, artifacts: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() && !path.exists(),
        "output index must be a new absolute path"
    );
    let parent = canonical_directory(
        path.parent().context("output index has no parent")?,
        "output parent",
    )?;
    ensure!(
        parent == artifacts,
        "output index must be inside the artifact directory"
    );
    Ok(())
}

fn read_bounded_regular(path: &Path, maximum: u64, label: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {label}"))?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() > 0
            && metadata.len() <= maximum,
        "{label} must be a bounded regular non-symlink file"
    );
    let bytes = fs::read(path).with_context(|| format!("read {label}"))?;
    ensure!(
        bytes.len() as u64 == metadata.len(),
        "{label} changed while being read"
    );
    Ok(bytes)
}

fn write_new_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(!path.exists(), "evidence output already exists");
    let parent = path.parent().context("evidence output has no parent")?;
    let temporary = parent.join(format!(".toolchain-evidence-{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<()> {
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .context("create toolchain evidence")?;
        std::io::Write::write_all(&mut output, bytes).context("write toolchain evidence")?;
        output.sync_all().context("sync toolchain evidence")?;
        drop(output);
        fs::rename(&temporary, path).context("atomically install toolchain evidence")?;
        fs::File::open(parent)?
            .sync_all()
            .context("sync evidence parent")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn required_path(values: &mut impl Iterator<Item = OsString>, label: &str) -> Result<PathBuf> {
    values
        .next()
        .map(PathBuf::from)
        .with_context(|| format!("missing {label}"))
}

fn required_utf8(values: &mut impl Iterator<Item = OsString>, label: &str) -> Result<String> {
    values
        .next()
        .with_context(|| format!("missing {label}"))?
        .into_string()
        .map_err(|_| anyhow::anyhow!("{label} must be UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_lock_requires_all_twelve_unique_pinned_toolchains() {
        let mut entries = Vec::new();
        for index in 0..12 {
            entries.push(SourceEntry {
                id: format!("language-{index}"),
                language: format!("language-{index}"),
                image_mib: 256,
                image: format!(
                    "registry.example/toolchain-{index}@sha256:{}",
                    "a".repeat(64)
                ),
            });
        }
        let mut lock = SourceLock {
            schema: "reporch.toolchain-sources-lock.v1".into(),
            sequence: 8,
            source_date_epoch: 1,
            entries,
        };
        validate_lock(&lock).unwrap();
        lock.entries[1].id = lock.entries[0].id.clone();
        assert!(validate_lock(&lock).is_err());
    }

    #[test]
    fn build_receipts_are_target_and_format_bound() {
        let mut receipt = BuildReceipt {
            schema: "reporch.toolchain-bundle-build.v2".into(),
            source_identity: format!("sha256:{}", "c".repeat(64)),
            architecture: "arm64".into(),
            filesystem: "ext4".into(),
            image_sha256: format!("sha256:{}", "a".repeat(64)),
            image_size: 2048,
            archive_sha256: format!("sha256:{}", "b".repeat(64)),
            archive_size: 1024,
            compression: "zstd".into(),
        };
        let identity = receipt.source_identity.clone();
        validate_receipt(&receipt, &identity, "arm64", "ext4").unwrap();
        receipt.filesystem = "vhdx".into();
        assert!(validate_receipt(&receipt, &identity, "arm64", "ext4").is_err());
    }
}
