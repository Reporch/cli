#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use reporch_runtime_core::{HostTarget, RuntimeArtifactKindV1};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const MAX_ARTIFACT_BYTES: u64 = 4 * 1_073_741_824;
const MAX_SOURCE_RECORD_BYTES: u64 = 1024 * 1024;

#[derive(Debug)]
struct Arguments {
    target: HostTarget,
    artifacts: PathBuf,
    source_record: PathBuf,
    source_revision: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceRecord {
    schema: String,
    target: String,
    source_lock_sha256: String,
    source_date_epoch: u64,
    kernel_version: String,
    kernel_provenance: String,
    firecracker_version: Option<String>,
    firecracker_tag_commit: Option<String>,
    windows_package_version: Option<String>,
    windows_package_sha256: Option<String>,
    rust_toolchain: String,
    rust_guest_targets: Vec<String>,
    files: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SpdxDocument<'a> {
    spdx_version: &'static str,
    data_license: &'static str,
    spdx_id: &'static str,
    name: String,
    document_namespace: String,
    creation_info: SpdxCreation<'a>,
    files: Vec<SpdxFile<'a>>,
    relationships: Vec<SpdxRelationship>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SpdxCreation<'a> {
    created: &'a str,
    creators: [&'static str; 1],
    license_list_version: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SpdxFile<'a> {
    file_name: String,
    spdx_id: &'static str,
    checksums: [SpdxChecksum<'a>; 1],
    license_concluded: &'a str,
    license_info_in_files: [&'a str; 1],
    copyright_text: &'static str,
}

#[derive(Debug, Serialize)]
struct SpdxChecksum<'a> {
    algorithm: &'static str,
    checksum_value: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SpdxRelationship {
    spdx_element_id: &'static str,
    relationship_type: &'static str,
    related_spdx_element: &'static str,
}

fn main() -> Result<()> {
    let arguments = parse_arguments(std::env::args_os().skip(1))?;
    let count = build_evidence(&arguments)?;
    println!(
        "{{\"schema\":\"reporch.runtime-evidence-build.v1\",\"target\":\"{}\",\"artifacts\":{count}}}",
        target_name(arguments.target)
    );
    Ok(())
}

fn parse_arguments(mut values: impl Iterator<Item = OsString>) -> Result<Arguments> {
    let target = required_utf8(&mut values, "runtime target")?;
    let target = parse_target(&target)?;
    let artifacts = required_path(&mut values, "artifact directory")?;
    let source_record = required_path(&mut values, "source materialization record")?;
    let source_revision = required_utf8(&mut values, "source revision")?;
    ensure!(values.next().is_none(), "too many arguments");
    ensure!(
        source_revision.len() == 40
            && source_revision
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "source revision must be a full lowercase Git object ID"
    );
    Ok(Arguments {
        target,
        artifacts,
        source_record,
        source_revision,
    })
}

fn build_evidence(arguments: &Arguments) -> Result<usize> {
    let artifacts = canonical_directory(&arguments.artifacts, "artifact directory")?;
    let source_bytes = read_bounded_regular(
        &arguments.source_record,
        MAX_SOURCE_RECORD_BYTES,
        "source materialization record",
    )?;
    let source: SourceRecord =
        serde_json::from_slice(&source_bytes).context("parse source materialization record")?;
    validate_source_record(&source, arguments.target)?;
    let source_record_digest = hex::encode(Sha256::digest(&source_bytes));
    let created = DateTime::<Utc>::from_timestamp(i64::try_from(source.source_date_epoch)?, 0)
        .context("source date epoch is out of range")?
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let required = required_artifacts(arguments.target);
    for (_, file_name) in &required {
        for suffix in [".spdx.json", ".intoto.jsonl"] {
            ensure!(
                !artifacts.join(format!("{file_name}{suffix}")).exists(),
                "runtime evidence output already exists"
            );
        }
    }
    for (kind, file_name) in &required {
        let artifact = artifacts.join(file_name);
        let (digest, size) = hash_bounded_regular(&artifact)?;
        validate_executable_identity(&artifact, *kind, arguments.target)?;
        let license = artifact_license(*kind, arguments.target);
        let spdx = SpdxDocument {
            spdx_version: "SPDX-2.3",
            data_license: "CC0-1.0",
            spdx_id: "SPDXRef-DOCUMENT",
            name: format!(
                "Reporch Runtime {} {file_name}",
                target_name(arguments.target)
            ),
            document_namespace: format!(
                "https://reporch.com/spdx/runtime/{}/{digest}",
                target_name(arguments.target)
            ),
            creation_info: SpdxCreation {
                created: &created,
                creators: ["Tool: reporch-runtime-evidence-builder-1.0.0-rc.8"],
                license_list_version: "3.27.0",
            },
            files: vec![SpdxFile {
                file_name: format!("./{file_name}"),
                spdx_id: "SPDXRef-RuntimeArtifact",
                checksums: [SpdxChecksum {
                    algorithm: "SHA256",
                    checksum_value: &digest,
                }],
                license_concluded: license,
                license_info_in_files: [license],
                copyright_text: "NOASSERTION",
            }],
            relationships: vec![SpdxRelationship {
                spdx_element_id: "SPDXRef-DOCUMENT",
                relationship_type: "DESCRIBES",
                related_spdx_element: "SPDXRef-RuntimeArtifact",
            }],
        };
        let mut spdx_bytes = serde_json::to_vec_pretty(&spdx).context("serialize SPDX evidence")?;
        spdx_bytes.push(b'\n');
        write_new_atomic(
            &artifacts.join(format!("{file_name}.spdx.json")),
            &spdx_bytes,
        )?;

        let provenance = serde_json::json!({
            "_type": "https://in-toto.io/Statement/v1",
            "subject": [{
                "name": file_name,
                "digest": { "sha256": digest }
            }],
            "predicateType": "https://slsa.dev/provenance/v1",
            "predicate": {
                "buildDefinition": {
                    "buildType": "https://reporch.com/buildtypes/runtime-artifact/v1",
                    "externalParameters": {
                        "target": target_name(arguments.target),
                        "kind": kind_name(*kind),
                        "size": size
                    },
                    "internalParameters": {
                        "sourceRevision": arguments.source_revision,
                        "sourceDateEpoch": source.source_date_epoch,
                        "kernelVersion": source.kernel_version,
                        "firecrackerVersion": source.firecracker_version,
                        "rustToolchain": source.rust_toolchain
                    },
                    "resolvedDependencies": [
                        {
                            "uri": "pkg:github/Reporch/cli",
                            "digest": { "gitCommit": arguments.source_revision }
                        },
                        {
                            "uri": "https://github.com/Reporch/cli/blob/main/runtime/sources.lock.json",
                            "digest": { "sha256": source.source_lock_sha256.trim_start_matches("sha256:") }
                        },
                        {
                            "uri": "reporch:runtime-source-materialization",
                            "digest": { "sha256": source_record_digest }
                        }
                    ]
                },
                "runDetails": {
                    "builder": {
                        "id": format!(
                            "https://github.com/Reporch/cli/tree/{}/tools/runtime-evidence-builder",
                            arguments.source_revision
                        )
                    },
                    "metadata": {
                        "invocationId": format!("urn:sha256:{digest}"),
                        "startedOn": created,
                        "finishedOn": created
                    }
                }
            }
        });
        let mut provenance_bytes =
            serde_json::to_vec(&provenance).context("serialize in-toto runtime provenance")?;
        provenance_bytes.push(b'\n');
        write_new_atomic(
            &artifacts.join(format!("{file_name}.intoto.jsonl")),
            &provenance_bytes,
        )?;
    }
    Ok(required.len())
}

fn validate_source_record(source: &SourceRecord, target: HostTarget) -> Result<()> {
    let source_lock_digest = source
        .source_lock_sha256
        .strip_prefix("sha256:")
        .unwrap_or_default();
    ensure!(
        source.schema == "reporch.runtime-source-materialization.v1"
            && source.target == target_name(target)
            && source.source_date_epoch > 0
            && source_lock_digest.len() == 64
            && source_lock_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && !source.kernel_version.is_empty()
            && !source.kernel_provenance.is_empty()
            && source.rust_toolchain == "1.96.0"
            && source.rust_guest_targets.len() == 2
            && source
                .files
                .contains_key(if target == HostTarget::WindowsX64Msvc {
                    "kernel"
                } else {
                    "vmlinux"
                }),
        "invalid source materialization identity"
    );
    let needs_firecracker = matches!(target, HostTarget::LinuxArm64Gnu | HostTarget::LinuxX64Gnu);
    ensure!(
        needs_firecracker
            == (source.firecracker_version.is_some()
                && source.firecracker_tag_commit.is_some()
                && source.files.contains_key("firecracker")
                && source.files.contains_key("jailer")),
        "source materialization Firecracker identity does not match target"
    );
    let needs_windows_package = target == HostTarget::WindowsX64Msvc;
    ensure!(
        needs_windows_package
            == (source.windows_package_version.is_some()
                && source
                    .windows_package_sha256
                    .as_deref()
                    .is_some_and(valid_prefixed_digest)),
        "source materialization Windows package identity does not match target"
    );
    Ok(())
}

fn valid_prefixed_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn required_artifacts(target: HostTarget) -> Vec<(RuntimeArtifactKindV1, &'static str)> {
    use RuntimeArtifactKindV1 as Kind;
    let kernel = if target == HostTarget::WindowsX64Msvc {
        "kernel"
    } else {
        "vmlinux"
    };
    let mut artifacts = vec![
        (Kind::Kernel, kernel),
        (Kind::Rootfs, "rootfs.cpio"),
        (Kind::GuestAgent, "reporch-guestd"),
    ];
    match target {
        HostTarget::LinuxArm64Gnu | HostTarget::LinuxX64Gnu => artifacts.extend([
            (Kind::VirtualMachineMonitor, "firecracker"),
            (Kind::Jailer, "jailer"),
            (Kind::HostService, "reporch-runtime-service"),
        ]),
        HostTarget::WindowsX64Msvc => {
            artifacts.push((Kind::HostService, "reporch-runtime-service.exe"));
        }
        HostTarget::DarwinArm64 | HostTarget::DarwinX64 => {}
    }
    artifacts
}

fn artifact_license(kind: RuntimeArtifactKindV1, _target: HostTarget) -> &'static str {
    match kind {
        RuntimeArtifactKindV1::Kernel => "GPL-2.0-only",
        RuntimeArtifactKindV1::Rootfs
        | RuntimeArtifactKindV1::GuestAgent
        | RuntimeArtifactKindV1::HostService
        | RuntimeArtifactKindV1::VirtualMachineMonitor
        | RuntimeArtifactKindV1::Jailer => "Apache-2.0",
    }
}

fn validate_executable_identity(
    path: &Path,
    kind: RuntimeArtifactKindV1,
    target: HostTarget,
) -> Result<()> {
    if kind == RuntimeArtifactKindV1::Kernel {
        let bytes = read_prefix(path, 0x238)?;
        if target == HostTarget::WindowsX64Msvc {
            ensure!(
                bytes.len() >= 0x238
                    && bytes[0x1fe..0x200] == [0x55, 0xaa]
                    && &bytes[0x202..0x206] == b"HdrS"
                    && u16::from_le_bytes(bytes[0x236..0x238].try_into()?) & 1 == 1,
                "Windows runtime kernel is not a 64-bit Linux x86 boot image"
            );
        } else {
            validate_elf_identity(&bytes, target, "runtime kernel")?;
        }
        return Ok(());
    }
    if kind == RuntimeArtifactKindV1::Rootfs {
        let bytes = read_prefix(path, 6)?;
        ensure!(
            matches!(bytes.as_slice(), b"070701" | b"070702"),
            "runtime rootfs is not a newc initramfs"
        );
        return Ok(());
    }
    if !matches!(
        kind,
        RuntimeArtifactKindV1::GuestAgent
            | RuntimeArtifactKindV1::HostService
            | RuntimeArtifactKindV1::VirtualMachineMonitor
            | RuntimeArtifactKindV1::Jailer
    ) {
        return Ok(());
    }
    let bytes = read_prefix(path, 64)?;
    if kind == RuntimeArtifactKindV1::HostService && target == HostTarget::WindowsX64Msvc {
        ensure!(
            bytes.len() >= 2 && &bytes[..2] == b"MZ",
            "Windows runtime service is not PE/COFF"
        );
        return Ok(());
    }
    validate_elf_identity(&bytes, target, "runtime executable")
}

fn validate_elf_identity(bytes: &[u8], target: HostTarget, label: &str) -> Result<()> {
    ensure!(
        bytes.len() >= 20 && &bytes[..4] == b"\x7fELF",
        "{label} is not ELF"
    );
    ensure!(
        bytes[4] == 2 && bytes[5] == 1,
        "{label} must be 64-bit little-endian ELF"
    );
    let machine = u16::from_le_bytes(bytes[18..20].try_into()?);
    let expected = match target {
        HostTarget::DarwinArm64 | HostTarget::LinuxArm64Gnu => 183,
        HostTarget::DarwinX64 | HostTarget::LinuxX64Gnu | HostTarget::WindowsX64Msvc => 62,
    };
    ensure!(machine == expected, "{label} architecture mismatch");
    Ok(())
}

fn hash_bounded_regular(path: &Path) -> Result<(String, u64)> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect runtime artifact {}", path.display()))?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && (1..=MAX_ARTIFACT_BYTES).contains(&metadata.len()),
        "runtime artifact must be a bounded regular non-symlink file"
    );
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("artifact size overflow")?;
        ensure!(
            size <= metadata.len(),
            "runtime artifact grew while hashing"
        );
        hasher.update(&buffer[..read]);
    }
    ensure!(size == metadata.len(), "runtime artifact was truncated");
    Ok((hex::encode(hasher.finalize()), size))
}

fn read_prefix(path: &Path, maximum: usize) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let mut bytes = vec![0_u8; maximum];
    let read = file.read(&mut bytes)?;
    bytes.truncate(read);
    Ok(bytes)
}

fn read_bounded_regular(path: &Path, maximum: u64, label: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {label}"))?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && (1..=maximum).contains(&metadata.len()),
        "{label} must be a bounded regular non-symlink file"
    );
    let bytes = fs::read(path)?;
    ensure!(
        bytes.len() as u64 == metadata.len(),
        "{label} changed while reading"
    );
    Ok(bytes)
}

fn write_new_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("evidence output has no parent")?;
    let temporary = parent.join(format!(".runtime-evidence-{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "{label} must be absolute");
    let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {label}"))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{label} must be a real directory"
    );
    path.canonicalize()
        .with_context(|| format!("resolve {label}"))
}

fn parse_target(value: &str) -> Result<HostTarget> {
    match value {
        "darwin-arm64" => Ok(HostTarget::DarwinArm64),
        "darwin-x64" => Ok(HostTarget::DarwinX64),
        "linux-arm64-gnu" => Ok(HostTarget::LinuxArm64Gnu),
        "linux-x64-gnu" => Ok(HostTarget::LinuxX64Gnu),
        "windows-x64-msvc" => Ok(HostTarget::WindowsX64Msvc),
        _ => anyhow::bail!("unsupported runtime target"),
    }
}

fn target_name(target: HostTarget) -> &'static str {
    match target {
        HostTarget::DarwinArm64 => "darwin-arm64",
        HostTarget::DarwinX64 => "darwin-x64",
        HostTarget::LinuxArm64Gnu => "linux-arm64-gnu",
        HostTarget::LinuxX64Gnu => "linux-x64-gnu",
        HostTarget::WindowsX64Msvc => "windows-x64-msvc",
    }
}

fn kind_name(kind: RuntimeArtifactKindV1) -> &'static str {
    match kind {
        RuntimeArtifactKindV1::Kernel => "kernel",
        RuntimeArtifactKindV1::Rootfs => "rootfs",
        RuntimeArtifactKindV1::GuestAgent => "guest_agent",
        RuntimeArtifactKindV1::HostService => "host_service",
        RuntimeArtifactKindV1::VirtualMachineMonitor => "virtual_machine_monitor",
        RuntimeArtifactKindV1::Jailer => "jailer",
    }
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
    fn licenses_are_explicit_for_every_artifact() {
        for target in [
            HostTarget::DarwinArm64,
            HostTarget::DarwinX64,
            HostTarget::LinuxArm64Gnu,
            HostTarget::LinuxX64Gnu,
            HostTarget::WindowsX64Msvc,
        ] {
            for (kind, _) in required_artifacts(target) {
                assert!(!artifact_license(kind, target).is_empty());
            }
        }
    }

    #[test]
    fn executable_architecture_validation_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("guestd");
        let mut elf = vec![0_u8; 64];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[18..20].copy_from_slice(&183_u16.to_le_bytes());
        fs::write(&path, elf).unwrap();
        validate_executable_identity(
            &path,
            RuntimeArtifactKindV1::GuestAgent,
            HostTarget::DarwinArm64,
        )
        .unwrap();
        assert!(
            validate_executable_identity(
                &path,
                RuntimeArtifactKindV1::GuestAgent,
                HostTarget::DarwinX64,
            )
            .is_err()
        );
    }

    #[test]
    fn windows_kernel_and_initramfs_validation_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let kernel = root.path().join("kernel");
        let mut image = vec![0_u8; 0x238];
        image[0x1fe..0x200].copy_from_slice(&[0x55, 0xaa]);
        image[0x202..0x206].copy_from_slice(b"HdrS");
        image[0x236..0x238].copy_from_slice(&1_u16.to_le_bytes());
        fs::write(&kernel, &image).unwrap();
        validate_executable_identity(
            &kernel,
            RuntimeArtifactKindV1::Kernel,
            HostTarget::WindowsX64Msvc,
        )
        .unwrap();
        image[0x202] = 0;
        fs::write(&kernel, image).unwrap();
        assert!(
            validate_executable_identity(
                &kernel,
                RuntimeArtifactKindV1::Kernel,
                HostTarget::WindowsX64Msvc,
            )
            .is_err()
        );

        let rootfs = root.path().join("rootfs.cpio");
        fs::write(&rootfs, b"070701fixture").unwrap();
        validate_executable_identity(
            &rootfs,
            RuntimeArtifactKindV1::Rootfs,
            HostTarget::WindowsX64Msvc,
        )
        .unwrap();
        fs::write(&rootfs, b"not-cpio").unwrap();
        assert!(
            validate_executable_identity(
                &rootfs,
                RuntimeArtifactKindV1::Rootfs,
                HostTarget::WindowsX64Msvc,
            )
            .is_err()
        );
    }

    fn evidence_fixture(root: &Path, name: &str) -> Arguments {
        let artifacts = root.join(name);
        fs::create_dir(&artifacts).unwrap();
        let mut kernel = vec![0_u8; 64];
        kernel[..6].copy_from_slice(b"\x7fELF\x02\x01");
        kernel[18..20].copy_from_slice(&183_u16.to_le_bytes());
        fs::write(artifacts.join("vmlinux"), kernel).unwrap();
        fs::write(artifacts.join("rootfs.cpio"), b"070701rootfs bytes\n").unwrap();
        let mut guestd = vec![0_u8; 64];
        guestd[..6].copy_from_slice(b"\x7fELF\x02\x01");
        guestd[18..20].copy_from_slice(&183_u16.to_le_bytes());
        fs::write(artifacts.join("reporch-guestd"), guestd).unwrap();
        let source_record = root.join("sources.json");
        if !source_record.exists() {
            fs::write(
                &source_record,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "schema": "reporch.runtime-source-materialization.v1",
                    "target": "darwin-arm64",
                    "source_lock_sha256": format!("sha256:{}", "a".repeat(64)),
                    "source_date_epoch": 1787961600_u64,
                    "kernel_version": "6.1.182",
                    "kernel_provenance": "fixture",
                    "firecracker_version": null,
                    "firecracker_tag_commit": null,
                    "rust_toolchain": "1.96.0",
                    "rust_guest_targets": [
                        "aarch64-unknown-linux-musl",
                        "x86_64-unknown-linux-musl"
                    ],
                    "files": {
                        "vmlinux": format!("sha256:{}", "b".repeat(64))
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        }
        Arguments {
            target: HostTarget::DarwinArm64,
            artifacts,
            source_record,
            source_revision: "c".repeat(40),
        }
    }

    #[test]
    fn evidence_is_deterministic_and_digest_bound() {
        let root = tempfile::tempdir().unwrap();
        let first = evidence_fixture(root.path(), "first");
        let second = evidence_fixture(root.path(), "second");
        assert_eq!(build_evidence(&first).unwrap(), 3);
        assert_eq!(build_evidence(&second).unwrap(), 3);
        for file_name in ["vmlinux", "rootfs.cpio", "reporch-guestd"] {
            for suffix in [".spdx.json", ".intoto.jsonl"] {
                assert_eq!(
                    fs::read(first.artifacts.join(format!("{file_name}{suffix}"))).unwrap(),
                    fs::read(second.artifacts.join(format!("{file_name}{suffix}"))).unwrap()
                );
            }
        }
        fs::write(first.artifacts.join("vmlinux"), b"changed\n").unwrap();
        let spdx: serde_json::Value =
            serde_json::from_slice(&fs::read(second.artifacts.join("vmlinux.spdx.json")).unwrap())
                .unwrap();
        assert_ne!(
            spdx["files"][0]["checksums"][0]["checksumValue"],
            hex::encode(Sha256::digest(b"changed\n"))
        );
    }
}
