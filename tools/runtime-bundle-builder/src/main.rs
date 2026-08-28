#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Duration, Utc};
use reporch_runtime_core::{
    BUNDLE_MANIFEST_SCHEMA, HostTarget, PROTOCOL_VERSION, RUNTIME_SIGNING_KEY_ID,
    RuntimeArtifactKindV1, RuntimeArtifactV1, RuntimeBundleManifestV1,
};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

const MAX_ARTIFACT_BYTES: u64 = 4 * 1_073_741_824;
const VALIDITY_DAYS: i64 = 35;

#[derive(Debug)]
struct Arguments {
    target: HostTarget,
    sequence: u64,
    version: String,
    minimum_os_version: String,
    base_url: Url,
    artifact_directory: PathBuf,
    output: PathBuf,
}

fn main() -> Result<()> {
    let arguments = parse_arguments(std::env::args_os().skip(1))?;
    let generated_at = generated_at()?;
    let manifest = build_manifest(&arguments, generated_at)?;
    write_manifest(&arguments.output, &manifest)?;
    println!(
        "{{\"schema\":\"reporch.runtime-bundle-build.v1\",\"target\":\"{}\",\"sequence\":{},\"artifacts\":{}}}",
        target_name(arguments.target),
        manifest.sequence,
        manifest.artifacts.len()
    );
    Ok(())
}

fn parse_arguments(mut values: impl Iterator<Item = OsString>) -> Result<Arguments> {
    let target = required_utf8(&mut values, "target")?;
    let target = parse_target(&target)?;
    let sequence = required_utf8(&mut values, "sequence")?
        .parse::<u64>()
        .context("sequence must be an integer")?;
    ensure!(sequence > 0, "sequence must be positive");
    let version = required_utf8(&mut values, "version")?;
    let minimum_os_version = required_utf8(&mut values, "minimum OS version")?;
    let base_url = Url::parse(&required_utf8(&mut values, "artifact base URL")?)
        .context("artifact base URL is invalid")?;
    validate_base_url(&base_url)?;
    let artifact_directory = required_path(&mut values, "artifact directory")?;
    let output = required_path(&mut values, "manifest output")?;
    ensure!(values.next().is_none(), "too many arguments");
    Ok(Arguments {
        target,
        sequence,
        version,
        minimum_os_version,
        base_url,
        artifact_directory,
        output,
    })
}

fn build_manifest(
    arguments: &Arguments,
    generated_at: DateTime<Utc>,
) -> Result<RuntimeBundleManifestV1> {
    let directory =
        canonical_directory(&arguments.artifact_directory, "runtime artifact directory")?;
    let mut artifacts = Vec::new();
    for (kind, file_name) in required_artifacts(arguments.target) {
        let path = directory.join(file_name);
        let (sha256, size) = hash_bounded_regular(&path)?;
        for suffix in [".spdx.json", ".intoto.jsonl"] {
            validate_metadata(&directory.join(format!("{file_name}{suffix}")))?;
        }
        artifacts.push(RuntimeArtifactV1 {
            kind,
            file_name: file_name.into(),
            sha256,
            size,
            source_url: arguments.base_url.join(file_name)?.to_string(),
            sbom_url: arguments
                .base_url
                .join(&format!("{file_name}.spdx.json"))?
                .to_string(),
            provenance_url: arguments
                .base_url
                .join(&format!("{file_name}.intoto.jsonl"))?
                .to_string(),
        });
    }
    let manifest = RuntimeBundleManifestV1 {
        schema: BUNDLE_MANIFEST_SCHEMA.into(),
        sequence: arguments.sequence,
        version: arguments.version.clone(),
        target: arguments.target,
        backend: arguments.target.native_backend(),
        minimum_os_version: arguments.minimum_os_version.clone(),
        protocol_min: PROTOCOL_VERSION,
        protocol_max: PROTOCOL_VERSION,
        generated_at,
        expires_at: generated_at + Duration::days(VALIDITY_DAYS),
        signing_key_id: RUNTIME_SIGNING_KEY_ID.into(),
        artifacts,
    };
    manifest
        .validate(generated_at)
        .map_err(anyhow::Error::from)?;
    Ok(manifest)
}

fn write_manifest(output: &Path, manifest: &RuntimeBundleManifestV1) -> Result<()> {
    ensure!(output.is_absolute(), "manifest output must be absolute");
    ensure!(!output.exists(), "manifest output already exists");
    let parent = canonical_directory(
        output.parent().context("manifest output has no parent")?,
        "manifest output parent",
    )?;
    let mut bytes = serde_json::to_vec_pretty(manifest).context("serialize runtime manifest")?;
    bytes.push(b'\n');
    let temporary = parent.join(format!(".runtime-manifest-{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .context("create temporary runtime manifest")?;
        file.write_all(&bytes).context("write runtime manifest")?;
        file.sync_all().context("sync runtime manifest")?;
        drop(file);
        fs::rename(&temporary, output).context("atomically install runtime manifest")?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn required_artifacts(target: HostTarget) -> Vec<(RuntimeArtifactKindV1, &'static str)> {
    use RuntimeArtifactKindV1 as Kind;
    let rootfs = if target == HostTarget::WindowsX64Msvc {
        "rootfs.vhdx"
    } else {
        "rootfs.cpio"
    };
    let mut artifacts = vec![
        (Kind::Kernel, "vmlinux"),
        (Kind::Rootfs, rootfs),
        (Kind::GuestAgent, "reporch-guestd"),
    ];
    match target.native_backend() {
        reporch_runtime_core::RuntimeBackend::Firecracker => {
            artifacts.extend([
                (Kind::VirtualMachineMonitor, "firecracker"),
                (Kind::Jailer, "jailer"),
                (Kind::HostService, "reporch-runtime-service"),
            ]);
        }
        reporch_runtime_core::RuntimeBackend::HyperVHcs => {
            artifacts.push((Kind::HostService, "reporch-runtime-service.exe"));
        }
        reporch_runtime_core::RuntimeBackend::AppleVirtualization => {}
        _ => unreachable!(),
    }
    artifacts
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
    let mut file = fs::File::open(path).context("open runtime artifact")?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).context("hash runtime artifact")?;
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
    Ok((format!("sha256:{}", hex::encode(hasher.finalize())), size))
}

fn validate_metadata(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect runtime metadata {}", path.display()))?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && (1..=16 * 1024 * 1024).contains(&metadata.len()),
        "runtime metadata must be a bounded regular non-symlink file"
    );
    Ok(())
}

fn validate_base_url(url: &Url) -> Result<()> {
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path().ends_with('/'),
        "artifact base URL must be credential-free HTTPS ending in /"
    );
    Ok(())
}

fn generated_at() -> Result<DateTime<Utc>> {
    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .context("SOURCE_DATE_EPOCH is required for reproducible manifests")?
        .parse::<i64>()
        .context("SOURCE_DATE_EPOCH must be an integer")?;
    DateTime::from_timestamp(epoch, 0).context("SOURCE_DATE_EPOCH is out of range")
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

    fn fixture(target: HostTarget) -> (tempfile::TempDir, Arguments) {
        let root = tempfile::tempdir().unwrap();
        let artifacts = root.path().join("artifacts");
        fs::create_dir(&artifacts).unwrap();
        for (_, name) in required_artifacts(target) {
            fs::write(artifacts.join(name), format!("artifact {name}\n")).unwrap();
            fs::write(artifacts.join(format!("{name}.spdx.json")), b"{}\n").unwrap();
            fs::write(artifacts.join(format!("{name}.intoto.jsonl")), b"{}\n").unwrap();
        }
        let arguments = Arguments {
            target,
            sequence: 8,
            version: "1.0.0-rc.8".into(),
            minimum_os_version: "15.0".into(),
            base_url: Url::parse(
                "https://github.com/Reporch/cli/releases/download/reporch-runtime-v1/",
            )
            .unwrap(),
            artifact_directory: artifacts,
            output: root.path().join("manifest.json"),
        };
        (root, arguments)
    }

    #[test]
    fn every_target_builds_a_complete_valid_manifest() {
        let generated = DateTime::parse_from_rfc3339("2026-08-28T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        for target in [
            HostTarget::DarwinArm64,
            HostTarget::DarwinX64,
            HostTarget::LinuxArm64Gnu,
            HostTarget::LinuxX64Gnu,
            HostTarget::WindowsX64Msvc,
        ] {
            let (_root, arguments) = fixture(target);
            let first = build_manifest(&arguments, generated).unwrap();
            let second = build_manifest(&arguments, generated).unwrap();
            assert_eq!(first, second);
            first.validate(generated).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_artifacts_and_missing_metadata_fail_closed() {
        use std::os::unix::fs::symlink;

        let (_root, arguments) = fixture(HostTarget::DarwinArm64);
        let kernel = arguments.artifact_directory.join("vmlinux");
        fs::remove_file(&kernel).unwrap();
        symlink("reporch-guestd", &kernel).unwrap();
        assert!(build_manifest(&arguments, Utc::now()).is_err());
    }
}
