#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use flate2::read::GzDecoder;
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;
use url::Url;
use uuid::Uuid;

const LOCK_BYTES: &[u8] = include_bytes!("../../../runtime/sources.lock.json");
const MAX_KERNEL_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_WINDOWS_MSI_BYTES: u64 = 320 * 1024 * 1024;
const MAX_FIRECRACKER_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FIRECRACKER_ENTRY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    DarwinArm64,
    DarwinX64,
    LinuxArm64,
    LinuxX64,
    WindowsX64,
}

impl Target {
    fn architecture(self) -> &'static str {
        match self {
            Self::DarwinArm64 | Self::LinuxArm64 => "aarch64",
            Self::DarwinX64 | Self::LinuxX64 | Self::WindowsX64 => "x86_64",
        }
    }

    fn includes_firecracker(self) -> bool {
        matches!(self, Self::LinuxArm64 | Self::LinuxX64)
    }

    fn uses_windows_kernel(self) -> bool {
        self == Self::WindowsX64
    }

    fn name(self) -> &'static str {
        match self {
            Self::DarwinArm64 => "darwin-arm64",
            Self::DarwinX64 => "darwin-x64",
            Self::LinuxArm64 => "linux-arm64-gnu",
            Self::LinuxX64 => "linux-x64-gnu",
            Self::WindowsX64 => "windows-x64-msvc",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceLock {
    schema: String,
    source_date_epoch: u64,
    guest_kernel: GuestKernel,
    windows_guest_kernel: WindowsGuestKernel,
    firecracker: Firecracker,
    rust: RustSource,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestKernel {
    version: String,
    provenance: String,
    artifacts: BTreeMap<String, KernelArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KernelArtifact {
    url: String,
    sha256: String,
    config_url: String,
    config_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowsGuestKernel {
    version: String,
    provenance: String,
    package_version: String,
    url: String,
    sha256: String,
    size: u64,
    msi_cabinet_stream: String,
    cabinet_entry: String,
    kernel_sha256: String,
    config_url: String,
    config_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Firecracker {
    version: String,
    tag_commit: String,
    artifacts: BTreeMap<String, DownloadArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DownloadArtifact {
    url: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RustSource {
    toolchain: String,
    guest_targets: Vec<String>,
}

#[derive(Debug, Serialize)]
struct MaterializationRecord<'a> {
    schema: &'static str,
    target: &'a str,
    source_lock_sha256: String,
    source_date_epoch: u64,
    kernel_version: &'a str,
    kernel_provenance: &'a str,
    firecracker_version: Option<&'a str>,
    firecracker_tag_commit: Option<&'a str>,
    windows_package_version: Option<&'a str>,
    windows_package_sha256: Option<String>,
    rust_toolchain: &'a str,
    rust_guest_targets: &'a [String],
    files: BTreeMap<String, String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let (target, output) = parse_arguments(std::env::args_os().skip(1))?;
    let lock: SourceLock =
        serde_json::from_slice(LOCK_BYTES).context("parse runtime source lock")?;
    validate_lock(&lock)?;
    materialize(target, &output, &lock).await?;
    println!(
        "{{\"schema\":\"reporch.runtime-source-fetch.v1\",\"target\":\"{}\"}}",
        target.name()
    );
    Ok(())
}

fn parse_arguments(mut values: impl Iterator<Item = OsString>) -> Result<(Target, PathBuf)> {
    let target = values
        .next()
        .context("missing runtime target")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("runtime target must be UTF-8"))?;
    let target = parse_target(&target)?;
    let output = values
        .next()
        .map(PathBuf::from)
        .context("missing new output directory")?;
    ensure!(values.next().is_none(), "too many arguments");
    ensure!(output.is_absolute(), "output directory must be absolute");
    ensure!(!output.exists(), "output directory already exists");
    Ok((target, output))
}

fn parse_target(value: &str) -> Result<Target> {
    match value {
        "darwin-arm64" => Ok(Target::DarwinArm64),
        "darwin-x64" => Ok(Target::DarwinX64),
        "linux-arm64-gnu" => Ok(Target::LinuxArm64),
        "linux-x64-gnu" => Ok(Target::LinuxX64),
        "windows-x64-msvc" => Ok(Target::WindowsX64),
        _ => anyhow::bail!("unsupported runtime target"),
    }
}

fn validate_lock(lock: &SourceLock) -> Result<()> {
    ensure!(
        lock.schema == "reporch.runtime-sources-lock.v1" && lock.source_date_epoch > 0,
        "invalid runtime source lock identity"
    );
    ensure!(
        !lock.guest_kernel.provenance.trim().is_empty(),
        "kernel provenance is missing"
    );
    let windows = &lock.windows_guest_kernel;
    ensure!(
        !windows.version.trim().is_empty()
            && !windows.provenance.trim().is_empty()
            && !windows.package_version.trim().is_empty()
            && windows.size > 0
            && windows.size <= MAX_WINDOWS_MSI_BYTES
            && windows.msi_cabinet_stream == "cab3.cab"
            && windows.cabinet_entry == "kernel",
        "Windows guest kernel identity is invalid"
    );
    for url in [&windows.url, &windows.config_url] {
        validate_source_url(url)?;
    }
    for digest in [
        &windows.sha256,
        &windows.kernel_sha256,
        &windows.config_sha256,
    ] {
        validate_digest(digest)?;
    }
    ensure!(
        lock.firecracker.tag_commit.len() == 40
            && lock
                .firecracker
                .tag_commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "invalid Firecracker tag commit"
    );
    ensure!(
        lock.rust.toolchain == "1.96.0" && lock.rust.guest_targets.len() == 2,
        "runtime Rust toolchain identity drifted"
    );
    for architecture in ["aarch64", "x86_64"] {
        let kernel = lock
            .guest_kernel
            .artifacts
            .get(architecture)
            .context("kernel architecture is missing")?;
        let firecracker = lock
            .firecracker
            .artifacts
            .get(architecture)
            .context("Firecracker architecture is missing")?;
        for url in [&kernel.url, &kernel.config_url, &firecracker.url] {
            validate_source_url(url)?;
        }
        for digest in [&kernel.sha256, &kernel.config_sha256, &firecracker.sha256] {
            validate_digest(digest)?;
        }
    }
    Ok(())
}

async fn materialize(target: Target, output: &Path, lock: &SourceLock) -> Result<()> {
    let parent = output.parent().context("output directory has no parent")?;
    let metadata = fs::symlink_metadata(parent).context("inspect output parent")?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "output parent must be a real directory"
    );
    let staging = parent.join(format!(".runtime-sources-{}", Uuid::now_v7()));
    fs::create_dir(&staging).context("create runtime source staging directory")?;
    let result = async {
        let client = reqwest::Client::builder()
            .https_only(true)
            .connect_timeout(Duration::from_secs(15))
            .user_agent("reporch-runtime-source-fetcher/1")
            .build()
            .context("build runtime source HTTP client")?;
        let mut files = BTreeMap::new();
        if target.uses_windows_kernel() {
            let kernel = &lock.windows_guest_kernel;
            let package = staging.join("windows-kernel.msi");
            fetch_verified(
                &client,
                &kernel.url,
                &kernel.sha256,
                MAX_WINDOWS_MSI_BYTES,
                &package,
            )
            .await?;
            ensure!(
                fs::metadata(&package)?.len() == kernel.size,
                "Windows kernel MSI size mismatch"
            );
            files.extend(extract_windows_kernel(&package, &staging, kernel)?);
            fs::remove_file(&package).context("remove verified Windows kernel MSI")?;
            fetch_verified(
                &client,
                &kernel.config_url,
                &kernel.config_sha256,
                MAX_CONFIG_BYTES,
                &staging.join("kernel.config"),
            )
            .await?;
            set_mode(&staging.join("kernel.config"), 0o444)?;
            files.insert(
                "kernel.config".into(),
                format!("sha256:{}", kernel.config_sha256),
            );
        } else {
            let architecture = target.architecture();
            let kernel = &lock.guest_kernel.artifacts[architecture];
            fetch_verified(
                &client,
                &kernel.url,
                &kernel.sha256,
                MAX_KERNEL_BYTES,
                &staging.join("vmlinux"),
            )
            .await?;
            set_mode(&staging.join("vmlinux"), 0o444)?;
            files.insert("vmlinux".into(), format!("sha256:{}", kernel.sha256));
            fetch_verified(
                &client,
                &kernel.config_url,
                &kernel.config_sha256,
                MAX_CONFIG_BYTES,
                &staging.join("kernel.config"),
            )
            .await?;
            set_mode(&staging.join("kernel.config"), 0o444)?;
            files.insert(
                "kernel.config".into(),
                format!("sha256:{}", kernel.config_sha256),
            );
        }
        let architecture = target.architecture();
        if target.includes_firecracker() {
            let firecracker = &lock.firecracker.artifacts[architecture];
            let archive = staging.join("firecracker.tgz");
            fetch_verified(
                &client,
                &firecracker.url,
                &firecracker.sha256,
                MAX_FIRECRACKER_ARCHIVE_BYTES,
                &archive,
            )
            .await?;
            let extracted =
                extract_firecracker(&archive, &staging, &lock.firecracker.version, architecture)?;
            fs::remove_file(&archive).context("remove verified Firecracker transport archive")?;
            files.extend(extracted);
        }
        let record = MaterializationRecord {
            schema: "reporch.runtime-source-materialization.v1",
            target: target.name(),
            source_lock_sha256: format!("sha256:{}", hex::encode(Sha256::digest(LOCK_BYTES))),
            source_date_epoch: lock.source_date_epoch,
            kernel_version: if target.uses_windows_kernel() {
                &lock.windows_guest_kernel.version
            } else {
                &lock.guest_kernel.version
            },
            kernel_provenance: if target.uses_windows_kernel() {
                &lock.windows_guest_kernel.provenance
            } else {
                &lock.guest_kernel.provenance
            },
            firecracker_version: target
                .includes_firecracker()
                .then_some(lock.firecracker.version.as_str()),
            firecracker_tag_commit: target
                .includes_firecracker()
                .then_some(lock.firecracker.tag_commit.as_str()),
            windows_package_version: target
                .uses_windows_kernel()
                .then_some(lock.windows_guest_kernel.package_version.as_str()),
            windows_package_sha256: target
                .uses_windows_kernel()
                .then(|| format!("sha256:{}", lock.windows_guest_kernel.sha256)),
            rust_toolchain: &lock.rust.toolchain,
            rust_guest_targets: &lock.rust.guest_targets,
            files,
        };
        let mut bytes = serde_json::to_vec_pretty(&record)?;
        bytes.push(b'\n');
        write_new(&staging.join("sources.json"), &bytes, 0o444)?;
        sync_directory(&staging)?;
        fs::rename(&staging, output).context("atomically publish runtime sources")?;
        sync_directory(parent)?;
        Result::<()>::Ok(())
    }
    .await;
    if result.is_err() {
        make_writable(&staging);
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn extract_windows_kernel(
    msi_path: &Path,
    output: &Path,
    source: &WindowsGuestKernel,
) -> Result<BTreeMap<String, String>> {
    let mut package = msi::open(msi_path).context("open verified Windows kernel MSI")?;
    let stream = package
        .read_stream(&source.msi_cabinet_stream)
        .context("open Windows kernel cabinet stream")?;
    let mut cabinet = cab::Cabinet::new(stream).context("parse Windows kernel cabinet")?;
    let entry_size = cabinet
        .folder_entries()
        .flat_map(|folder| folder.file_entries())
        .find(|entry| entry.name() == source.cabinet_entry)
        .map(|entry| u64::from(entry.uncompressed_size()))
        .context("Windows kernel cabinet entry is missing")?;
    ensure!(
        (1..=MAX_KERNEL_BYTES).contains(&entry_size),
        "Windows kernel cabinet entry has unsafe size"
    );
    let mut reader = cabinet
        .read_file(&source.cabinet_entry)
        .context("open Windows kernel cabinet entry")?;
    let mut bytes = Vec::with_capacity(usize::try_from(entry_size)?);
    reader
        .by_ref()
        .take(MAX_KERNEL_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("extract Windows kernel cabinet entry")?;
    ensure!(
        bytes.len() as u64 == entry_size,
        "Windows kernel cabinet entry was truncated or oversized"
    );
    validate_x86_64_bzimage(&bytes)?;
    let digest = hex::encode(Sha256::digest(&bytes));
    ensure!(
        digest == source.kernel_sha256,
        "Windows kernel SHA-256 mismatch"
    );
    write_new(&output.join("kernel"), &bytes, 0o444)?;
    Ok(BTreeMap::from([(
        "kernel".into(),
        format!("sha256:{digest}"),
    )]))
}

fn validate_x86_64_bzimage(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() >= 0x238
            && bytes[0x1fe..0x200] == [0x55, 0xaa]
            && &bytes[0x202..0x206] == b"HdrS",
        "Windows guest kernel is not a Linux x86 boot image"
    );
    let xloadflags = u16::from_le_bytes(bytes[0x236..0x238].try_into()?);
    ensure!(
        xloadflags & 1 == 1,
        "Windows guest kernel is not 64-bit capable"
    );
    Ok(())
}

async fn fetch_verified(
    client: &reqwest::Client,
    url: &str,
    expected_sha256: &str,
    maximum: u64,
    output: &Path,
) -> Result<()> {
    validate_source_url(url)?;
    validate_digest(expected_sha256)?;
    let response = tokio::time::timeout(Duration::from_secs(30), client.get(url).send())
        .await
        .context("runtime source request timed out")??
        .error_for_status()
        .with_context(|| format!("download runtime source {url}"))?;
    if response
        .content_length()
        .is_some_and(|length| length == 0 || length > maximum)
    {
        anyhow::bail!("runtime source has an invalid declared size");
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .await
        .context("create downloaded runtime source")?;
    let mut stream = response.bytes_stream();
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .context("runtime source download stalled")?
    {
        let chunk = chunk.context("read runtime source body")?;
        total = total
            .checked_add(chunk.len() as u64)
            .context("runtime source size overflow")?;
        ensure!(total <= maximum, "runtime source exceeded its size limit");
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .context("write runtime source")?;
    }
    ensure!(total > 0, "runtime source was empty");
    file.sync_all().await.context("sync runtime source")?;
    drop(file);
    let actual = hex::encode(hasher.finalize());
    ensure!(actual == expected_sha256, "runtime source SHA-256 mismatch");
    Ok(())
}

fn extract_firecracker(
    archive_path: &Path,
    output: &Path,
    version: &str,
    architecture: &str,
) -> Result<BTreeMap<String, String>> {
    let archive = fs::File::open(archive_path).context("open Firecracker archive")?;
    let mut archive = tar::Archive::new(GzDecoder::new(archive));
    let prefix = format!("release-v{version}-{architecture}");
    let selected = BTreeMap::from([
        (
            format!("{prefix}/firecracker-v{version}-{architecture}"),
            ("firecracker", true),
        ),
        (
            format!("{prefix}/jailer-v{version}-{architecture}"),
            ("jailer", true),
        ),
        (format!("{prefix}/LICENSE"), ("firecracker-LICENSE", false)),
        (format!("{prefix}/NOTICE"), ("firecracker-NOTICE", false)),
        (
            format!("{prefix}/THIRD-PARTY"),
            ("firecracker-THIRD-PARTY", false),
        ),
    ]);
    let mut found = HashSet::new();
    let mut digests = BTreeMap::new();
    for (index, entry) in archive.entries()?.enumerate() {
        ensure!(
            index < MAX_ARCHIVE_ENTRIES,
            "Firecracker archive has too many entries"
        );
        let mut entry = entry.context("read Firecracker archive entry")?;
        let path = entry.path().context("read Firecracker archive path")?;
        ensure!(
            path.components()
                .all(|component| matches!(component, Component::Normal(_))),
            "Firecracker archive contains an unsafe path"
        );
        let path = path
            .to_str()
            .context("Firecracker archive path is not UTF-8")?;
        ensure!(
            entry.header().entry_type().is_file() || entry.header().entry_type().is_dir(),
            "Firecracker archive contains a link or special entry"
        );
        let Some((name, executable)) = selected.get(path) else {
            continue;
        };
        ensure!(
            entry.header().entry_type().is_file(),
            "selected Firecracker entry is not regular"
        );
        ensure!(
            found.insert(path.to_owned()),
            "duplicate selected Firecracker entry"
        );
        let size = entry.size();
        ensure!(
            (1..=MAX_FIRECRACKER_ENTRY_BYTES).contains(&size),
            "Firecracker entry has unsafe size"
        );
        let mut bytes = Vec::with_capacity(usize::try_from(size)?);
        entry
            .read_to_end(&mut bytes)
            .context("extract Firecracker entry")?;
        ensure!(
            bytes.len() as u64 == size,
            "Firecracker entry was truncated"
        );
        if *executable {
            validate_elf(&bytes, architecture)?;
        }
        let digest = hex::encode(Sha256::digest(&bytes));
        write_new(
            &output.join(name),
            &bytes,
            if *executable { 0o555 } else { 0o444 },
        )?;
        digests.insert((*name).into(), format!("sha256:{digest}"));
    }
    ensure!(
        found.len() == selected.len(),
        "Firecracker archive is missing required entries"
    );
    Ok(digests)
}

fn validate_elf(bytes: &[u8], architecture: &str) -> Result<()> {
    ensure!(
        bytes.len() >= 20 && &bytes[..4] == b"\x7fELF",
        "Firecracker binary is not ELF"
    );
    ensure!(
        bytes[4] == 2 && bytes[5] == 1,
        "Firecracker binary is not 64-bit little-endian ELF"
    );
    let machine = u16::from_le_bytes(bytes[18..20].try_into()?);
    let expected = match architecture {
        "aarch64" => 183,
        "x86_64" => 62,
        _ => anyhow::bail!("unsupported ELF architecture"),
    };
    ensure!(machine == expected, "Firecracker ELF architecture mismatch");
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    set_mode(path, mode)
}

fn validate_source_url(value: &str) -> Result<()> {
    let url = Url::parse(value).context("parse runtime source URL")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "runtime source URL must be credential-free HTTPS"
    );
    Ok(())
}

fn validate_digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "runtime source digest is invalid"
    );
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(windows)]
    {
        let _ = mode;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions)?;
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

fn make_writable(path: &Path) {
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                make_writable(&path);
            }
            if let Ok(metadata) = fs::metadata(&path) {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = if metadata.is_dir() { 0o700 } else { 0o600 };
                    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode));
                }
                #[cfg(windows)]
                {
                    let mut permissions = metadata.permissions();
                    permissions.set_readonly(false);
                    let _ = fs::set_permissions(&path, permissions);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_and_all_public_targets_are_valid() {
        let lock: SourceLock = serde_json::from_slice(LOCK_BYTES).unwrap();
        validate_lock(&lock).unwrap();
        for target in [
            "darwin-arm64",
            "darwin-x64",
            "linux-arm64-gnu",
            "linux-x64-gnu",
            "windows-x64-msvc",
        ] {
            assert_eq!(parse_target(target).unwrap().name(), target);
        }
    }

    #[test]
    fn elf_validation_is_architecture_bound() {
        let mut elf = vec![0_u8; 20];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[18..20].copy_from_slice(&183_u16.to_le_bytes());
        validate_elf(&elf, "aarch64").unwrap();
        assert!(validate_elf(&elf, "x86_64").is_err());
    }

    #[test]
    fn windows_kernel_header_validation_is_64_bit_and_fail_closed() {
        let mut image = vec![0_u8; 0x238];
        image[0x1fe..0x200].copy_from_slice(&[0x55, 0xaa]);
        image[0x202..0x206].copy_from_slice(b"HdrS");
        image[0x236..0x238].copy_from_slice(&1_u16.to_le_bytes());
        validate_x86_64_bzimage(&image).unwrap();
        image[0x236..0x238].copy_from_slice(&0_u16.to_le_bytes());
        assert!(validate_x86_64_bzimage(&image).is_err());
    }
}
