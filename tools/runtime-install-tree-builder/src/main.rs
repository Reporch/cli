#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use minisign_verify::{PublicKey, Signature};
use reporch_runtime_core::{
    HostTarget, INSTALLATION_SCHEMA, RuntimeArtifactKindV1, RuntimeBundleManifestV1,
    RuntimeInstallationV1,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const PUBLIC_KEY: &str = include_str!("../../../artifacts/runtime-v1.minisign.pub");
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 16 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 4 * 1_073_741_824;
const COPY_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
struct Arguments {
    manifest: PathBuf,
    signature: PathBuf,
    artifacts: PathBuf,
    output: PathBuf,
}

fn main() -> Result<()> {
    let arguments = parse_arguments(std::env::args_os().skip(1))?;
    let installed_at = source_date_epoch()?;
    let manifest_bytes =
        read_bounded_regular(&arguments.manifest, MAX_MANIFEST_BYTES, "runtime manifest")?;
    let signature_bytes = read_bounded_regular(
        &arguments.signature,
        MAX_SIGNATURE_BYTES,
        "runtime manifest signature",
    )?;
    verify_signature(&manifest_bytes, &signature_bytes)?;
    let manifest: RuntimeBundleManifestV1 =
        serde_json::from_slice(&manifest_bytes).context("parse signed runtime manifest")?;
    manifest
        .validate(installed_at)
        .map_err(anyhow::Error::from)?;
    build_install_tree(
        &manifest,
        &manifest_bytes,
        &signature_bytes,
        &arguments.artifacts,
        &arguments.output,
        installed_at,
    )?;
    println!(
        "{{\"schema\":\"reporch.runtime-install-tree-build.v1\",\"target\":\"{}\",\"sequence\":{},\"version\":\"{}\"}}",
        target_name(manifest.target),
        manifest.sequence,
        manifest.version
    );
    Ok(())
}

fn parse_arguments(mut values: impl Iterator<Item = OsString>) -> Result<Arguments> {
    let manifest = required_path(&mut values, "signed manifest")?;
    let signature = required_path(&mut values, "manifest signature")?;
    let artifacts = required_path(&mut values, "artifact directory")?;
    let output = required_path(&mut values, "installation tree output")?;
    ensure!(values.next().is_none(), "too many arguments");
    for (path, label) in [
        (&manifest, "signed manifest"),
        (&signature, "manifest signature"),
        (&artifacts, "artifact directory"),
        (&output, "installation tree output"),
    ] {
        ensure!(path.is_absolute(), "{label} must be absolute");
    }
    Ok(Arguments {
        manifest,
        signature,
        artifacts,
        output,
    })
}

fn verify_signature(bytes: &[u8], signature: &[u8]) -> Result<()> {
    let public_key = PublicKey::decode(PUBLIC_KEY).context("decode compiled runtime public key")?;
    let signature = std::str::from_utf8(signature).context("signature must be UTF-8")?;
    let signature = Signature::decode(signature).context("decode runtime manifest signature")?;
    public_key
        .verify(bytes, &signature, false)
        .context("verify runtime manifest against the compiled trust root")
}

fn build_install_tree(
    manifest: &RuntimeBundleManifestV1,
    manifest_bytes: &[u8],
    signature_bytes: &[u8],
    artifact_directory: &Path,
    output: &Path,
    installed_at: DateTime<Utc>,
) -> Result<()> {
    ensure!(!output.exists(), "installation tree output already exists");
    let artifacts = canonical_directory(artifact_directory, "artifact directory")?;
    let parent = canonical_directory(
        output
            .parent()
            .context("installation tree output has no parent")?,
        "installation tree output parent",
    )?;
    let staging = parent.join(format!(".runtime-install-tree-{}", Uuid::now_v7()));
    fs::create_dir(&staging).context("create installation tree staging directory")?;
    let result = (|| -> Result<()> {
        let bundle = staging
            .join("bundles")
            .join(bundle_directory_name(manifest.sequence, &manifest.version));
        fs::create_dir_all(&bundle).context("create runtime bundle directory")?;
        for artifact in &manifest.artifacts {
            let source = artifacts.join(&artifact.file_name);
            let destination = bundle.join(&artifact.file_name);
            copy_verified_regular(&source, &destination, artifact.size, &artifact.sha256)?;
            set_artifact_permissions(&destination, artifact.kind)?;
        }
        write_new_file(&bundle.join("manifest.json"), manifest_bytes, false)?;
        write_new_file(
            &bundle.join("manifest.json.minisig"),
            signature_bytes,
            false,
        )?;
        let manifest_digest = format!("sha256:{}", hex::encode(Sha256::digest(manifest_bytes)));
        write_new_file(
            &bundle.join(".complete"),
            format!("{manifest_digest}\n").as_bytes(),
            false,
        )?;
        let installation = RuntimeInstallationV1 {
            schema: INSTALLATION_SCHEMA.into(),
            sequence: manifest.sequence,
            version: manifest.version.clone(),
            target: manifest.target,
            bundle_sha256: manifest_digest,
            installed_at,
        };
        installation.validate().map_err(anyhow::Error::from)?;
        let mut installation_bytes =
            serde_json::to_vec_pretty(&installation).context("serialize runtime installation")?;
        installation_bytes.push(b'\n');
        write_new_file(&staging.join("current.json"), &installation_bytes, false)?;
        set_directory_mode(&bundle, 0o555)?;
        set_directory_mode(&staging.join("bundles"), 0o755)?;
        sync_directory(&bundle)?;
        sync_directory(&staging.join("bundles"))?;
        sync_directory(&staging)?;
        fs::rename(&staging, output).context("atomically install runtime tree")?;
        sync_directory(&parent)?;
        set_directory_mode(output, 0o755)?;
        Ok(())
    })();
    if result.is_err() {
        make_tree_writable_for_cleanup(&staging);
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn copy_verified_regular(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<()> {
    ensure!(
        (1..=MAX_ARTIFACT_BYTES).contains(&expected_size),
        "artifact has an invalid declared size"
    );
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("inspect runtime artifact {}", source.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() == expected_size,
        "runtime artifact is not the declared bounded regular file"
    );
    let mut input = fs::File::open(source).context("open runtime artifact")?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .context("create installed runtime artifact")?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    // Windows executables have a smaller default main-thread stack than the
    // Unix targets. Keep the bounded transfer buffer on the heap so copying a
    // valid runtime bundle cannot exhaust that platform stack.
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer).context("read runtime artifact")?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("runtime artifact size overflow")?;
        ensure!(
            total <= expected_size,
            "runtime artifact grew while copying"
        );
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .context("copy runtime artifact")?;
    }
    ensure!(total == expected_size, "runtime artifact was truncated");
    let actual = format!("sha256:{}", hex::encode(hasher.finalize()));
    ensure!(actual == expected_sha256, "runtime artifact hash mismatch");
    output
        .sync_all()
        .context("sync installed runtime artifact")?;
    Ok(())
}

fn read_bounded_regular(path: &Path, maximum: u64, label: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {label}"))?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && (1..=maximum).contains(&metadata.len()),
        "{label} must be a bounded regular non-symlink file"
    );
    let bytes = fs::read(path).with_context(|| format!("read {label}"))?;
    ensure!(
        bytes.len() as u64 == metadata.len(),
        "{label} changed while reading"
    );
    Ok(bytes)
}

fn write_new_file(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", path.display()))?;
    drop(file);
    set_read_only_permissions(path, executable)
}

fn set_artifact_permissions(path: &Path, kind: RuntimeArtifactKindV1) -> Result<()> {
    let executable = matches!(
        kind,
        RuntimeArtifactKindV1::GuestAgent
            | RuntimeArtifactKindV1::HostService
            | RuntimeArtifactKindV1::VirtualMachineMonitor
            | RuntimeArtifactKindV1::Jailer
    );
    set_read_only_permissions(path, executable)
}

fn set_read_only_permissions(path: &Path, executable: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if executable { 0o555 } else { 0o444 }),
        )
        .with_context(|| format!("set read-only permissions on {}", path.display()))?;
    }
    #[cfg(windows)]
    {
        let _ = executable;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn set_directory_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(windows)]
    {
        let _ = (path, mode);
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

fn make_tree_writable_for_cleanup(path: &Path) {
    // Cleanup runs on the error path, where masking the original failure with a
    // stack overflow is particularly harmful. Runtime trees can contain many
    // files, so walk them iteratively and restore permissions in post-order.
    let mut pending = vec![path.to_path_buf()];
    let mut visited = Vec::new();
    while let Some(current) = pending.pop() {
        let Ok(metadata) = fs::symlink_metadata(&current) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir()
            && let Ok(entries) = fs::read_dir(&current)
        {
            pending.extend(entries.flatten().map(|entry| entry.path()));
        }
        visited.push((current, metadata.is_dir()));
    }

    for (current, _is_directory) in visited.into_iter().rev() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = if _is_directory { 0o700 } else { 0o600 };
            let _ = fs::set_permissions(&current, fs::Permissions::from_mode(mode));
        }
        #[cfg(windows)]
        if let Ok(metadata) = fs::symlink_metadata(&current) {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            let _ = fs::set_permissions(&current, permissions);
        }
    }
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

fn source_date_epoch() -> Result<DateTime<Utc>> {
    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .context("SOURCE_DATE_EPOCH is required for a reproducible install tree")?
        .parse::<i64>()
        .context("SOURCE_DATE_EPOCH must be an integer")?;
    DateTime::from_timestamp(epoch, 0).context("SOURCE_DATE_EPOCH is out of range")
}

fn bundle_directory_name(sequence: u64, version: &str) -> String {
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
    format!("{sequence}-{safe_version}")
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use reporch_runtime_core::{
        BUNDLE_MANIFEST_SCHEMA, PROTOCOL_VERSION, RUNTIME_SIGNING_KEY_ID, RuntimeArtifactV1,
    };

    fn fixture() -> (tempfile::TempDir, RuntimeBundleManifestV1, Vec<u8>, Vec<u8>) {
        let root = tempfile::tempdir().unwrap();
        let artifacts = root.path().join("artifacts");
        fs::create_dir(&artifacts).unwrap();
        let content = b"kernel fixture\n";
        fs::write(artifacts.join("vmlinux"), content).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-08-29T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let manifest = RuntimeBundleManifestV1 {
            schema: BUNDLE_MANIFEST_SCHEMA.into(),
            sequence: 8,
            version: "1.0.0-rc.8".into(),
            target: HostTarget::DarwinArm64,
            backend: HostTarget::DarwinArm64.native_backend(),
            minimum_os_version: "15.0".into(),
            protocol_min: PROTOCOL_VERSION,
            protocol_max: PROTOCOL_VERSION,
            generated_at: now,
            expires_at: now + Duration::days(35),
            signing_key_id: RUNTIME_SIGNING_KEY_ID.into(),
            artifacts: vec![RuntimeArtifactV1 {
                kind: RuntimeArtifactKindV1::Kernel,
                file_name: "vmlinux".into(),
                sha256: format!("sha256:{}", hex::encode(Sha256::digest(content))),
                size: content.len() as u64,
                source_url: "https://example.test/vmlinux".into(),
                sbom_url: "https://example.test/vmlinux.spdx.json".into(),
                provenance_url: "https://example.test/vmlinux.intoto.jsonl".into(),
            }],
        };
        let mut bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        bytes.push(b'\n');
        (root, manifest, bytes, b"test signature\n".to_vec())
    }

    #[test]
    fn deterministic_installation_tree_uses_declared_bytes() {
        let (root, manifest, bytes, signature) = fixture();
        let installed_at = manifest.generated_at;
        let first = root.path().join("first");
        let second = root.path().join("second");
        for output in [&first, &second] {
            build_install_tree(
                &manifest,
                &bytes,
                &signature,
                &root.path().join("artifacts"),
                output,
                installed_at,
            )
            .unwrap();
        }
        let relative = Path::new("bundles/8-1.0.0-rc.8");
        assert_eq!(
            fs::read(first.join("current.json")).unwrap(),
            fs::read(second.join("current.json")).unwrap()
        );
        assert_eq!(
            fs::read(first.join(relative).join("vmlinux")).unwrap(),
            b"kernel fixture\n"
        );
        assert_eq!(
            fs::read(first.join(relative).join(".complete")).unwrap(),
            fs::read(second.join(relative).join(".complete")).unwrap()
        );
    }

    #[test]
    fn artifact_tampering_fails_without_publishing_partial_tree() {
        let (root, manifest, bytes, signature) = fixture();
        fs::write(root.path().join("artifacts/vmlinux"), b"tampered bytes\n").unwrap();
        let output = root.path().join("output");
        assert!(
            build_install_tree(
                &manifest,
                &bytes,
                &signature,
                &root.path().join("artifacts"),
                &output,
                manifest.generated_at
            )
            .is_err()
        );
        assert!(!output.exists());
    }

    #[cfg(unix)]
    #[test]
    fn artifact_symlink_fails_closed() {
        use std::os::unix::fs::symlink;

        let (root, manifest, bytes, signature) = fixture();
        let artifact = root.path().join("artifacts/vmlinux");
        fs::remove_file(&artifact).unwrap();
        symlink("../outside", &artifact).unwrap();
        fs::write(root.path().join("outside"), b"kernel fixture\n").unwrap();
        assert!(
            build_install_tree(
                &manifest,
                &bytes,
                &signature,
                &root.path().join("artifacts"),
                &root.path().join("output"),
                manifest.generated_at
            )
            .is_err()
        );
    }

    #[test]
    fn arbitrary_signature_does_not_match_compiled_trust_root() {
        assert!(verify_signature(b"manifest", b"not a minisign signature").is_err());
    }

    #[test]
    fn cleanup_permission_walk_handles_deep_runtime_trees_without_recursion() {
        let root = tempfile::tempdir().unwrap();
        let mut directory = root.path().join("tree");
        fs::create_dir(&directory).unwrap();
        for _ in 0..128 {
            directory = directory.join("d");
            fs::create_dir(&directory).unwrap();
        }
        let artifact = directory.join("artifact");
        fs::write(&artifact, b"runtime artifact").unwrap();
        set_read_only_permissions(&artifact, false).unwrap();

        make_tree_writable_for_cleanup(&root.path().join("tree"));

        fs::remove_dir_all(root.path().join("tree")).unwrap();
    }
}
