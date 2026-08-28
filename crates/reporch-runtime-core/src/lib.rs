#![forbid(unsafe_code)]

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use reporch_runtime_protocol::{
    ContentObjectV1, GuestHandshakeV1, GuestJobV1, GuestOperationV1, GuestOutputEncodingV1,
    GuestOutputV1, GuestResultV1, JOB_SCHEMA, PROTOCOL_VERSION, ResourceLimitsV1,
};

pub const BUNDLE_MANIFEST_SCHEMA: &str = "reporch.runtime-bundle-manifest.v1";
pub const INSTALLATION_SCHEMA: &str = "reporch.runtime-installation.v1";
pub const STATUS_SCHEMA: &str = "reporch.runtime-status.v1";
pub const DOCTOR_SCHEMA: &str = "reporch.runtime-doctor.v1";
pub const TOOLCHAIN_INDEX_SCHEMA: &str = "reporch.toolchain-index.v2";
pub const TOOLCHAIN_INSTALLATION_SCHEMA: &str = "reporch.toolchain-installation.v2";
pub const RUNTIME_SIGNING_KEY_ID: &str = "FF2F931B66DAA966";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostTarget {
    DarwinArm64,
    DarwinX64,
    LinuxArm64Gnu,
    LinuxX64Gnu,
    WindowsX64Msvc,
}

impl HostTarget {
    pub const fn current() -> Option<Self> {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Some(Self::DarwinArm64)
        } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            Some(Self::DarwinX64)
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            Some(Self::LinuxArm64Gnu)
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some(Self::LinuxX64Gnu)
        } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            Some(Self::WindowsX64Msvc)
        } else {
            None
        }
    }

    pub const fn native_backend(self) -> RuntimeBackend {
        match self {
            Self::DarwinArm64 | Self::DarwinX64 => RuntimeBackend::AppleVirtualization,
            Self::LinuxArm64Gnu | Self::LinuxX64Gnu => RuntimeBackend::Firecracker,
            Self::WindowsX64Msvc => RuntimeBackend::HyperVHcs,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeBackend {
    AppleVirtualization,
    Firecracker,
    HyperVHcs,
    RemoteOnly,
    LegacyPodman,
    LegacyDocker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAvailability {
    NotInstalled,
    Ready,
    RemoteOnly,
    Broken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeArtifactV1 {
    pub kind: RuntimeArtifactKindV1,
    pub file_name: String,
    pub sha256: String,
    pub size: u64,
    pub source_url: String,
    pub sbom_url: String,
    pub provenance_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeArtifactKindV1 {
    Kernel,
    Rootfs,
    GuestAgent,
    HostService,
    VirtualMachineMonitor,
    Jailer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBundleManifestV1 {
    pub schema: String,
    pub sequence: u64,
    pub version: String,
    pub target: HostTarget,
    pub backend: RuntimeBackend,
    pub minimum_os_version: String,
    pub protocol_min: u16,
    pub protocol_max: u16,
    pub generated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub signing_key_id: String,
    pub artifacts: Vec<RuntimeArtifactV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolchainFilesystemV2 {
    Ext4,
    Vhdx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolchainCompressionV2 {
    Zstd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolchainBundleV2 {
    pub target: HostTarget,
    pub filesystem: ToolchainFilesystemV2,
    pub file_name: String,
    pub sha256: String,
    pub size: u64,
    pub archive_file_name: String,
    pub archive_sha256: String,
    pub archive_size: u64,
    pub compression: ToolchainCompressionV2,
    pub source_url: String,
    pub sbom_url: String,
    pub provenance_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolchainEntryV2 {
    pub id: String,
    pub language: String,
    /// Identity of the common source/toolchain lock used for both Studio OCI
    /// and local VM bundles.
    pub toolchain_lock_sha256: String,
    pub studio_oci_image: String,
    pub bundles: Vec<ToolchainBundleV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolchainIndexV2 {
    pub schema: String,
    pub sequence: u64,
    pub generated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub signing_key_id: String,
    pub entries: Vec<ToolchainEntryV2>,
}

impl ToolchainIndexV2 {
    pub fn validate(&self, now: DateTime<Utc>) -> Result<(), RuntimeError> {
        if self.schema != TOOLCHAIN_INDEX_SCHEMA
            || self.sequence == 0
            || self.generated_at > now
            || self.expires_at <= now
            || self.generated_at >= self.expires_at
            || self.signing_key_id != RUNTIME_SIGNING_KEY_ID
        {
            return Err(RuntimeError::AssetVerificationFailed(
                "invalid or expired toolchain index identity".into(),
            ));
        }
        if self.entries.is_empty() || self.entries.len() > 256 {
            return Err(RuntimeError::AssetVerificationFailed(
                "invalid toolchain index entry count".into(),
            ));
        }
        let mut ids = HashSet::new();
        for entry in &self.entries {
            if !ids.insert(entry.id.as_str())
                || !valid_identifier(&entry.id, 64)
                || !valid_identifier(&entry.language, 64)
            {
                return Err(RuntimeError::AssetVerificationFailed(
                    "invalid or duplicate toolchain identity".into(),
                ));
            }
            validate_sha256(&entry.toolchain_lock_sha256)?;
            validate_oci_image(&entry.studio_oci_image)?;
            if entry.bundles.len() != 5 {
                return Err(RuntimeError::AssetVerificationFailed(format!(
                    "toolchain {} does not cover every supported target",
                    entry.id
                )));
            }
            let mut targets = HashSet::new();
            for (index, bundle) in entry.bundles.iter().enumerate() {
                if !targets.insert(bundle.target) {
                    return Err(RuntimeError::AssetVerificationFailed(
                        "duplicate toolchain target".into(),
                    ));
                }
                for previous in &entry.bundles[..index] {
                    if previous.file_name == bundle.file_name
                        && (previous.filesystem != bundle.filesystem
                            || previous.sha256 != bundle.sha256
                            || previous.size != bundle.size
                            || previous.archive_file_name != bundle.archive_file_name
                            || previous.archive_sha256 != bundle.archive_sha256
                            || previous.archive_size != bundle.archive_size
                            || previous.compression != bundle.compression
                            || previous.source_url != bundle.source_url
                            || previous.sbom_url != bundle.sbom_url
                            || previous.provenance_url != bundle.provenance_url)
                    {
                        return Err(RuntimeError::AssetVerificationFailed(
                            "shared toolchain artifact identity changed across targets".into(),
                        ));
                    }
                    if previous.archive_file_name == bundle.archive_file_name
                        && previous.file_name != bundle.file_name
                    {
                        return Err(RuntimeError::AssetVerificationFailed(
                            "toolchain archive name aliases a different image".into(),
                        ));
                    }
                }
                let expected_filesystem = if bundle.target == HostTarget::WindowsX64Msvc {
                    ToolchainFilesystemV2::Vhdx
                } else {
                    ToolchainFilesystemV2::Ext4
                };
                if bundle.filesystem != expected_filesystem {
                    return Err(RuntimeError::AssetVerificationFailed(
                        "toolchain filesystem does not match its host target".into(),
                    ));
                }
                validate_file_name(&bundle.file_name)?;
                validate_sha256(&bundle.sha256)?;
                validate_file_name(&bundle.archive_file_name)?;
                validate_sha256(&bundle.archive_sha256)?;
                if bundle.size == 0 || bundle.size > 8 * 1_073_741_824 {
                    return Err(RuntimeError::AssetVerificationFailed(
                        "invalid toolchain bundle size".into(),
                    ));
                }
                if bundle.archive_size == 0
                    || bundle.archive_size > 2 * 1_073_741_824
                    || bundle.archive_size >= bundle.size
                {
                    return Err(RuntimeError::AssetVerificationFailed(
                        "invalid toolchain archive size".into(),
                    ));
                }
                for url in [&bundle.source_url, &bundle.sbom_url, &bundle.provenance_url] {
                    validate_https_url(url)?;
                }
            }
        }
        Ok(())
    }

    pub fn entry_for_target(
        &self,
        id: &str,
        target: HostTarget,
    ) -> Result<(&ToolchainEntryV2, &ToolchainBundleV2), RuntimeError> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| RuntimeError::AssetVerificationFailed("unknown toolchain ID".into()))?;
        let bundle = entry
            .bundles
            .iter()
            .find(|bundle| bundle.target == target)
            .ok_or_else(|| {
                RuntimeError::AssetVerificationFailed("toolchain target missing".into())
            })?;
        Ok((entry, bundle))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolchainInstallationV2 {
    pub schema: String,
    pub index_sequence: u64,
    pub id: String,
    pub target: HostTarget,
    pub toolchain_lock_sha256: String,
    pub bundle_sha256: String,
    pub file_name: String,
    pub installed_at: DateTime<Utc>,
}

impl ToolchainInstallationV2 {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.schema != TOOLCHAIN_INSTALLATION_SCHEMA
            || self.index_sequence == 0
            || !valid_identifier(&self.id, 64)
        {
            return Err(RuntimeError::AssetVerificationFailed(
                "invalid toolchain installation state".into(),
            ));
        }
        validate_sha256(&self.toolchain_lock_sha256)?;
        validate_sha256(&self.bundle_sha256)?;
        validate_file_name(&self.file_name)
    }
}

impl RuntimeBundleManifestV1 {
    pub fn validate(&self, now: DateTime<Utc>) -> Result<(), RuntimeError> {
        if self.schema != BUNDLE_MANIFEST_SCHEMA {
            return Err(RuntimeError::ProtocolIncompatible);
        }
        if self.sequence == 0 || !valid_identifier(&self.version, 128) {
            return Err(RuntimeError::AssetVerificationFailed(
                "invalid bundle identity".into(),
            ));
        }
        if !valid_identifier(&self.minimum_os_version, 64) {
            return Err(RuntimeError::AssetVerificationFailed(
                "invalid minimum OS version".into(),
            ));
        }
        if self.backend != self.target.native_backend() {
            return Err(RuntimeError::AssetVerificationFailed(
                "backend does not match target".into(),
            ));
        }
        if self.protocol_min == 0
            || self.protocol_min > self.protocol_max
            || !(self.protocol_min..=self.protocol_max).contains(&PROTOCOL_VERSION)
        {
            return Err(RuntimeError::ProtocolIncompatible);
        }
        if self.generated_at > now || self.expires_at <= now || self.generated_at >= self.expires_at
        {
            return Err(RuntimeError::AssetVerificationFailed(
                "runtime index is expired or not yet valid".into(),
            ));
        }
        if self.signing_key_id != RUNTIME_SIGNING_KEY_ID {
            return Err(RuntimeError::AssetVerificationFailed(
                "runtime manifest does not name the embedded signing key".into(),
            ));
        }
        if self.artifacts.is_empty() || self.artifacts.len() > 16 {
            return Err(RuntimeError::AssetVerificationFailed(
                "invalid runtime artifact count".into(),
            ));
        }
        let mut kinds = HashSet::new();
        let mut names = HashSet::new();
        for artifact in &self.artifacts {
            if !kinds.insert(artifact.kind) || !names.insert(artifact.file_name.as_str()) {
                return Err(RuntimeError::AssetVerificationFailed(
                    "duplicate runtime artifact".into(),
                ));
            }
            validate_file_name(&artifact.file_name)?;
            validate_sha256(&artifact.sha256)?;
            if artifact.size == 0 || artifact.size > 4 * 1_073_741_824 {
                return Err(RuntimeError::AssetVerificationFailed(
                    "invalid runtime artifact size".into(),
                ));
            }
            for url in [
                &artifact.source_url,
                &artifact.sbom_url,
                &artifact.provenance_url,
            ] {
                validate_https_url(url)?;
            }
        }
        for required in [
            RuntimeArtifactKindV1::Kernel,
            RuntimeArtifactKindV1::Rootfs,
            RuntimeArtifactKindV1::GuestAgent,
        ] {
            if !kinds.contains(&required) {
                return Err(RuntimeError::AssetVerificationFailed(
                    "runtime bundle is incomplete".into(),
                ));
            }
        }
        for required in match self.backend {
            RuntimeBackend::Firecracker => vec![
                RuntimeArtifactKindV1::VirtualMachineMonitor,
                RuntimeArtifactKindV1::Jailer,
                RuntimeArtifactKindV1::HostService,
            ],
            RuntimeBackend::HyperVHcs => vec![RuntimeArtifactKindV1::HostService],
            RuntimeBackend::AppleVirtualization => Vec::new(),
            RuntimeBackend::RemoteOnly
            | RuntimeBackend::LegacyPodman
            | RuntimeBackend::LegacyDocker => {
                return Err(RuntimeError::AssetVerificationFailed(
                    "native runtime bundle cannot select a legacy or remote backend".into(),
                ));
            }
        } {
            if !kinds.contains(&required) {
                return Err(RuntimeError::AssetVerificationFailed(
                    "runtime bundle is missing its platform backend".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInstallationV1 {
    pub schema: String,
    pub sequence: u64,
    pub version: String,
    pub target: HostTarget,
    pub bundle_sha256: String,
    pub installed_at: DateTime<Utc>,
}

impl RuntimeInstallationV1 {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.schema != INSTALLATION_SCHEMA
            || self.sequence == 0
            || !valid_identifier(&self.version, 128)
        {
            return Err(RuntimeError::BootstrapIncomplete);
        }
        validate_sha256(&self.bundle_sha256)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeStatusV1 {
    pub schema: String,
    pub target: Option<HostTarget>,
    pub backend: RuntimeBackend,
    pub availability: RuntimeAvailability,
    pub installed_version: Option<String>,
    pub installed_sequence: Option<u64>,
    pub protocol_version: u16,
    pub virtualization_available: bool,
    pub service_available: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeDoctorCheckV1 {
    pub id: String,
    pub passed: bool,
    pub repairable: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeDoctorV1 {
    pub schema: String,
    pub status: RuntimeStatusV1,
    pub checks: Vec<RuntimeDoctorCheckV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeUpdateV1 {
    pub schema: String,
    pub previous_version: Option<String>,
    pub installed_version: String,
    pub sequence: u64,
    pub target: HostTarget,
    pub repaired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    #[error("Reporch Runtime is not installed completely")]
    BootstrapIncomplete,
    #[error("hardware virtualization is unavailable: {0}")]
    VirtualizationUnavailable(String),
    #[error("runtime asset verification failed: {0}")]
    AssetVerificationFailed(String),
    #[error("runtime service is unavailable: {0}")]
    ServiceUnavailable(String),
    #[error("runtime guest failed to boot: {0}")]
    GuestBootFailed(String),
    #[error("runtime guest stopped responding")]
    GuestUnresponsive,
    #[error("runtime protocol is incompatible")]
    ProtocolIncompatible,
    #[error("runtime cleanup failed: {0}")]
    CleanupFailed(String),
    #[error("Studio remote fallback was not allowed")]
    RemoteFallbackNotAllowed,
    #[error("Studio remote fallback quota was exceeded")]
    RemoteQuotaExceeded,
}

impl RuntimeError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::BootstrapIncomplete => "runtime.bootstrap_incomplete",
            Self::VirtualizationUnavailable(_) => "runtime.virtualization_unavailable",
            Self::AssetVerificationFailed(_) => "runtime.asset_verification_failed",
            Self::ServiceUnavailable(_) => "runtime.service_unavailable",
            Self::GuestBootFailed(_) => "runtime.guest_boot_failed",
            Self::GuestUnresponsive => "runtime.guest_unresponsive",
            Self::ProtocolIncompatible => "runtime.protocol_incompatible",
            Self::CleanupFailed(_) => "runtime.cleanup_failed",
            Self::RemoteFallbackNotAllowed => "runtime.remote_fallback_not_allowed",
            Self::RemoteQuotaExceeded => "runtime.remote_quota_exceeded",
        }
    }

    pub const fn retryable(&self) -> bool {
        matches!(
            self,
            Self::ServiceUnavailable(_)
                | Self::GuestBootFailed(_)
                | Self::GuestUnresponsive
                | Self::CleanupFailed(_)
        )
    }
}

fn validate_file_name(value: &str) -> Result<(), RuntimeError> {
    if value.is_empty()
        || value.len() > 255
        || value.starts_with('.')
        || value.contains('/')
        || value.contains('\\')
        || value.contains('\0')
    {
        return Err(RuntimeError::AssetVerificationFailed(
            "invalid runtime artifact file name".into(),
        ));
    }
    Ok(())
}

fn valid_identifier(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn validate_sha256(value: &str) -> Result<(), RuntimeError> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(RuntimeError::AssetVerificationFailed(
            "invalid SHA-256".into(),
        ));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(RuntimeError::AssetVerificationFailed(
            "invalid SHA-256".into(),
        ));
    }
    Ok(())
}

fn validate_oci_image(value: &str) -> Result<(), RuntimeError> {
    let Some((name, digest)) = value.rsplit_once('@') else {
        return Err(RuntimeError::AssetVerificationFailed(
            "Studio toolchain image is not digest-pinned".into(),
        ));
    };
    if name.is_empty()
        || name.len() > 512
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'/' | b':' | b'_' | b'-')
        })
    {
        return Err(RuntimeError::AssetVerificationFailed(
            "Studio toolchain image name is invalid".into(),
        ));
    }
    validate_sha256(digest)
}

fn validate_https_url(value: &str) -> Result<(), RuntimeError> {
    if value.len() > 2_048 {
        return Err(RuntimeError::AssetVerificationFailed(
            "runtime artifact URL is too long".into(),
        ));
    }
    let parsed = url::Url::parse(value).map_err(|_| {
        RuntimeError::AssetVerificationFailed("runtime artifact URL is invalid".into())
    })?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(RuntimeError::AssetVerificationFailed(
            "runtime artifact URL must be credential-free HTTPS without query or fragment".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn manifest(now: DateTime<Utc>) -> RuntimeBundleManifestV1 {
        let artifact = |kind, name: &str| RuntimeArtifactV1 {
            kind,
            file_name: name.into(),
            sha256: format!("sha256:{}", "a".repeat(64)),
            size: 1,
            source_url: format!("https://downloads.reporch.com/{name}"),
            sbom_url: format!("https://downloads.reporch.com/{name}.spdx.json"),
            provenance_url: format!("https://downloads.reporch.com/{name}.intoto.jsonl"),
        };
        let target = HostTarget::current().unwrap_or(HostTarget::LinuxX64Gnu);
        let mut artifacts = vec![
            artifact(RuntimeArtifactKindV1::Kernel, "vmlinux"),
            artifact(RuntimeArtifactKindV1::Rootfs, "rootfs.img"),
            artifact(RuntimeArtifactKindV1::GuestAgent, "reporch-guestd"),
        ];
        match target.native_backend() {
            RuntimeBackend::Firecracker => {
                artifacts.push(artifact(
                    RuntimeArtifactKindV1::VirtualMachineMonitor,
                    "firecracker",
                ));
                artifacts.push(artifact(RuntimeArtifactKindV1::Jailer, "jailer"));
                artifacts.push(artifact(
                    RuntimeArtifactKindV1::HostService,
                    "reporch-runtime-service",
                ));
            }
            RuntimeBackend::HyperVHcs => artifacts.push(artifact(
                RuntimeArtifactKindV1::HostService,
                "reporch-runtime-service.exe",
            )),
            RuntimeBackend::AppleVirtualization => {}
            _ => unreachable!(),
        }
        RuntimeBundleManifestV1 {
            schema: BUNDLE_MANIFEST_SCHEMA.into(),
            sequence: 1,
            version: "2026.08.28.1".into(),
            target,
            backend: target.native_backend(),
            minimum_os_version: "1".into(),
            protocol_min: 1,
            protocol_max: 1,
            generated_at: now - Duration::minutes(1),
            expires_at: now + Duration::days(30),
            signing_key_id: RUNTIME_SIGNING_KEY_ID.into(),
            artifacts,
        }
    }

    #[test]
    fn manifest_is_target_bound_complete_and_expiring() {
        let now = Utc::now();
        manifest(now).validate(now).unwrap();
        let mut expired = manifest(now);
        expired.expires_at = now;
        assert_eq!(
            expired.validate(now).unwrap_err().code(),
            "runtime.asset_verification_failed"
        );
        let mut incomplete = manifest(now);
        incomplete.artifacts.pop();
        assert!(incomplete.validate(now).is_err());
        let mut wrong_key = manifest(now);
        wrong_key.signing_key_id = "0000000000000000".into();
        assert!(wrong_key.validate(now).is_err());
        let mut query_url = manifest(now);
        query_url.artifacts[0].source_url.push_str("?token=secret");
        assert!(query_url.validate(now).is_err());
    }

    #[test]
    fn toolchain_v2_binds_one_common_lock_to_every_target() {
        let now = Utc::now();
        let bundle = |target: HostTarget, suffix: &str| ToolchainBundleV2 {
            target,
            filesystem: if target == HostTarget::WindowsX64Msvc {
                ToolchainFilesystemV2::Vhdx
            } else {
                ToolchainFilesystemV2::Ext4
            },
            file_name: format!("python-{suffix}.img"),
            sha256: format!("sha256:{}", "b".repeat(64)),
            size: 2048,
            archive_file_name: format!("python-{suffix}.img.zst"),
            archive_sha256: format!("sha256:{}", "d".repeat(64)),
            archive_size: 1024,
            compression: ToolchainCompressionV2::Zstd,
            source_url: format!("https://downloads.reporch.com/python-{suffix}.img.zst"),
            sbom_url: format!("https://downloads.reporch.com/python-{suffix}.spdx.json"),
            provenance_url: format!("https://downloads.reporch.com/python-{suffix}.intoto.jsonl"),
        };
        let mut index = ToolchainIndexV2 {
            schema: TOOLCHAIN_INDEX_SCHEMA.into(),
            sequence: 1,
            generated_at: now - Duration::minutes(1),
            expires_at: now + Duration::days(30),
            signing_key_id: RUNTIME_SIGNING_KEY_ID.into(),
            entries: vec![ToolchainEntryV2 {
                id: "python-3.14".into(),
                language: "python".into(),
                toolchain_lock_sha256: format!("sha256:{}", "a".repeat(64)),
                studio_oci_image: format!(
                    "registry.reporch.com/toolchains/python@sha256:{}",
                    "c".repeat(64)
                ),
                bundles: vec![
                    bundle(HostTarget::DarwinArm64, "darwin-arm64"),
                    bundle(HostTarget::DarwinX64, "darwin-x64"),
                    bundle(HostTarget::LinuxArm64Gnu, "linux-arm64"),
                    bundle(HostTarget::LinuxX64Gnu, "linux-x64"),
                    bundle(HostTarget::WindowsX64Msvc, "windows-x64"),
                ],
            }],
        };
        index.validate(now).unwrap();
        let mut shared_arm64 = index.entries[0].bundles[0].clone();
        shared_arm64.target = HostTarget::LinuxArm64Gnu;
        index.entries[0].bundles[2] = shared_arm64;
        index.validate(now).unwrap();
        index.entries[0].bundles[2].source_url.push_str("-changed");
        assert!(index.validate(now).is_err());
        index.entries[0].bundles[2].source_url = index.entries[0].bundles[0].source_url.clone();
        index.entries[0].studio_oci_image = "python:latest".into();
        assert!(index.validate(now).is_err());
    }
}
