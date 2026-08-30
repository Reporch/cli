use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use minisign_verify::{PublicKey, Signature};
use reporch_runtime_core::RuntimeError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::local_sandbox::{
    OciRuntime, configure_process_group, kill_process_tree, read_bounded, resolve_secure_runtime,
    validate_image,
};

const INDEX_SCHEMA: &str = "reporch.toolchain-index.v1";
const LIST_SCHEMA: &str = "reporch.toolchain-list.v1";
const INSPECTION_SCHEMA: &str = "reporch.toolchain-inspection.v1";
const MAX_INDEX_BYTES: usize = 256 * 1024;
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const INSPECTION_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_INSPECTION_OUTPUT_BYTES: usize = 64 * 1024;
const INDEX_BYTES: &[u8] = include_bytes!("../../../artifacts/toolchains-v1.json");
const INDEX_SIGNATURE: &str = include_str!("../../../artifacts/toolchains-v1.json.minisig");
const INDEX_PUBLIC_KEY: &str = include_str!("../../../artifacts/toolchains-v1.minisign.pub");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolchainEntryV1 {
    pub id: String,
    pub language: String,
    pub image: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolchainIndexV1 {
    schema: String,
    generated_at: DateTime<Utc>,
    entries: Vec<ToolchainEntryV1>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolchainListV1 {
    pub schema: &'static str,
    pub generated_at: DateTime<Utc>,
    pub signing_key_sha256: String,
    pub entries: Vec<ToolchainEntryV1>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolchainInspectionV1 {
    pub schema: &'static str,
    pub runtime: String,
    pub entry: ToolchainEntryV1,
    pub installed: bool,
    pub installed_repo_digests: Vec<String>,
    pub installed_bundle_sha256: Option<String>,
}

pub fn list() -> Result<ToolchainListV1> {
    let mut index = verified_index()?;
    index.entries.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(ToolchainListV1 {
        schema: LIST_SCHEMA,
        generated_at: index.generated_at,
        signing_key_sha256: hex::encode(Sha256::digest(INDEX_PUBLIC_KEY.as_bytes())),
        entries: index.entries,
    })
}

pub async fn inspect(id: &str, runtime: OciRuntime) -> Result<ToolchainInspectionV1> {
    let entry = find_entry(id)?;
    if runtime == OciRuntime::Auto {
        return match reporch_runtime_host::verified_toolchain(id).await {
            Ok(installed) => Ok(ToolchainInspectionV1 {
                schema: INSPECTION_SCHEMA,
                runtime: "reporch_vm".into(),
                entry,
                installed: true,
                installed_repo_digests: Vec::new(),
                installed_bundle_sha256: Some(installed.installation.bundle_sha256),
            }),
            Err(_) => Ok(ToolchainInspectionV1 {
                schema: INSPECTION_SCHEMA,
                runtime: "reporch_vm".into(),
                entry,
                installed: false,
                installed_repo_digests: Vec::new(),
                installed_bundle_sha256: None,
            }),
        };
    }
    let runtime = resolve_secure_runtime(runtime).await?;
    inspect_entry(&runtime.name, &runtime.executable, entry).await
}

pub async fn install(id: &str, runtime: OciRuntime) -> Result<ToolchainInspectionV1> {
    let entry = find_entry(id)?;
    if runtime == OciRuntime::Auto {
        let installed = reporch_runtime_host::install_toolchain(id).await?;
        return Ok(ToolchainInspectionV1 {
            schema: INSPECTION_SCHEMA,
            runtime: "reporch_vm".into(),
            entry,
            installed: true,
            installed_repo_digests: Vec::new(),
            installed_bundle_sha256: Some(installed.installation.bundle_sha256),
        });
    }
    let runtime = resolve_secure_runtime(runtime).await?;
    let before = inspect_entry(&runtime.name, &runtime.executable, entry.clone()).await?;
    if before.installed {
        return Ok(before);
    }

    let mut command = Command::new(&runtime.executable);
    command
        .args(["pull", entry.image.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    configure_process_group(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("start {} toolchain pull", runtime.name))?;
    let status = match tokio::time::timeout(INSTALL_TIMEOUT, child.wait()).await {
        Ok(status) => status.context("wait for toolchain image pull")?,
        Err(_) => {
            kill_process_tree(&mut child).await;
            return Err(RuntimeError::ServiceUnavailable(
                "toolchain installation exceeded 30 minutes".into(),
            )
            .into());
        }
    };
    ensure!(status.success(), "toolchain image pull failed");

    let installed = inspect_entry(&runtime.name, &runtime.executable, entry).await?;
    ensure!(
        installed.installed,
        "OCI runtime did not retain the exact signed toolchain digest"
    );
    Ok(installed)
}

/// Resolve a language to one exact entry from the signed embedded catalog.
/// An explicit ID is still checked against the requested language so a project
/// cannot silently run source code with an unrelated image.
pub fn resolve_for_language(explicit_id: Option<&str>, language: &str) -> Result<ToolchainEntryV1> {
    let language = normalize_language(language)?;
    let index = verified_index()?;
    let entry = match explicit_id {
        Some(id) => index
            .entries
            .into_iter()
            .find(|entry| entry.id == id.trim())
            .with_context(|| format!("unknown signed toolchain: {}", id.trim()))?,
        None => index
            .entries
            .into_iter()
            .find(|entry| entry.language == language)
            .with_context(|| format!("no signed toolchain is available for {language}"))?,
    };
    ensure!(
        entry.language == language,
        "signed toolchain {} is for {}, not {language}",
        entry.id,
        entry.language
    );
    Ok(entry)
}

pub fn resolve_for_image(image: &str) -> Result<ToolchainEntryV1> {
    verified_index()?
        .entries
        .into_iter()
        .find(|entry| entry.image == image)
        .context("native execution requires an image from the signed Reporch toolchain catalog")
}

fn normalize_language(language: &str) -> Result<&'static str> {
    match language.trim().to_ascii_lowercase().as_str() {
        "python" | "python3" | "py" => Ok("python"),
        "pypy" | "pypy3" => Ok("pypy"),
        "c" | "c11" | "gnu11" | "gnu17" => Ok("c"),
        "cpp" | "c++" | "cpp17" | "cpp20" | "gnu++17" | "gnu++20" => Ok("cpp"),
        "java" => Ok("java"),
        "rust" | "rust2021" | "rust2024" => Ok("rust"),
        "javascript" | "js" | "node" | "nodejs" => Ok("javascript"),
        "csharp" | "c#" | "cs" | "dotnet" => Ok("csharp"),
        "php" => Ok("php"),
        "r" => Ok("r"),
        "bash" | "sh" | "shell" => Ok("bash"),
        "swift" => Ok("swift"),
        _ => bail!("unsupported local toolchain language: {language}"),
    }
}

fn verified_index() -> Result<ToolchainIndexV1> {
    ensure!(
        !INDEX_BYTES.is_empty() && INDEX_BYTES.len() <= MAX_INDEX_BYTES,
        "embedded toolchain index has an invalid size"
    );
    let public_key = PublicKey::decode(INDEX_PUBLIC_KEY).context("decode toolchain public key")?;
    let signature = Signature::decode(INDEX_SIGNATURE).context("decode toolchain signature")?;
    public_key
        .verify(INDEX_BYTES, &signature, false)
        .context("verify signed toolchain index")?;
    let index: ToolchainIndexV1 =
        serde_json::from_slice(INDEX_BYTES).context("parse signed toolchain index")?;
    validate_index(&index)?;
    Ok(index)
}

fn validate_index(index: &ToolchainIndexV1) -> Result<()> {
    ensure!(
        index.schema == INDEX_SCHEMA,
        "unsupported toolchain index schema"
    );
    ensure!(
        !index.entries.is_empty() && index.entries.len() <= 256,
        "toolchain index entry count is invalid"
    );
    let mut ids = HashSet::new();
    for entry in &index.entries {
        ensure!(
            (1..=64).contains(&entry.id.len())
                && entry.id.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'_' | b'-')
                }),
            "toolchain ID is invalid: {}",
            entry.id
        );
        ensure!(ids.insert(entry.id.as_str()), "duplicate toolchain ID");
        ensure!(
            (1..=64).contains(&entry.language.len())
                && entry.language.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'+' | b'#' | b'.' | b'_' | b'-')
                }),
            "toolchain language is invalid: {}",
            entry.language
        );
        validate_image(&entry.image)?;
    }
    Ok(())
}

fn find_entry(id: &str) -> Result<ToolchainEntryV1> {
    let id = id.trim();
    ensure!(!id.is_empty(), "toolchain ID cannot be empty");
    verified_index()?
        .entries
        .into_iter()
        .find(|entry| entry.id == id)
        .with_context(|| format!("unknown signed toolchain: {id}"))
}

async fn inspect_entry(
    runtime: &str,
    executable: &std::path::Path,
    entry: ToolchainEntryV1,
) -> Result<ToolchainInspectionV1> {
    let mut command = Command::new(executable);
    command
        .args([
            "image",
            "inspect",
            "--format",
            "{{json .RepoDigests}}",
            entry.image.as_str(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_process_group(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("inspect {} toolchain image", entry.id))?;
    let stdout = child
        .stdout
        .take()
        .context("capture OCI inspection stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("capture OCI inspection stderr")?;
    let stdout_task = tokio::spawn(read_bounded(stdout, MAX_INSPECTION_OUTPUT_BYTES));
    let stderr_task = tokio::spawn(read_bounded(stderr, MAX_INSPECTION_OUTPUT_BYTES));
    let status = match tokio::time::timeout(INSPECTION_TIMEOUT, child.wait()).await {
        Ok(status) => status.context("wait for OCI image inspection")?,
        Err(_) => {
            kill_process_tree(&mut child).await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(RuntimeError::ServiceUnavailable(
                "OCI image inspection exceeded 10 seconds".into(),
            )
            .into());
        }
    };
    let (stdout, stdout_truncated) = stdout_task.await.context("join OCI stdout reader")??;
    let (_, stderr_truncated) = stderr_task.await.context("join OCI stderr reader")??;
    ensure!(
        !stdout_truncated && !stderr_truncated,
        "OCI image inspection output is too large"
    );
    if !status.success() {
        return Ok(ToolchainInspectionV1 {
            schema: INSPECTION_SCHEMA,
            runtime: runtime.into(),
            entry,
            installed: false,
            installed_repo_digests: Vec::new(),
            installed_bundle_sha256: None,
        });
    }
    let repo_digests: Vec<String> =
        serde_json::from_slice(&stdout).context("parse OCI repository digests")?;
    let expected_digest = entry
        .image
        .rsplit_once('@')
        .map(|(_, digest)| digest)
        .context("signed toolchain image lacks a digest")?;
    let installed = repo_digests
        .iter()
        .filter_map(|digest| digest.rsplit_once('@').map(|(_, digest)| digest))
        .any(|digest| digest == expected_digest);
    Ok(ToolchainInspectionV1 {
        schema: INSPECTION_SCHEMA,
        runtime: runtime.into(),
        entry,
        installed,
        installed_repo_digests: repo_digests,
        installed_bundle_sha256: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_index_is_signed_unique_and_digest_pinned() {
        let index = verified_index().unwrap();
        assert!(index.entries.len() >= 10);
        assert!(
            index
                .entries
                .iter()
                .all(|entry| entry.image.contains("@sha256:"))
        );
    }

    #[test]
    fn index_signature_fails_after_any_mutation() {
        let public_key = PublicKey::decode(INDEX_PUBLIC_KEY).unwrap();
        let signature = Signature::decode(INDEX_SIGNATURE).unwrap();
        let mut changed = INDEX_BYTES.to_vec();
        changed[0] ^= 1;
        assert!(public_key.verify(&changed, &signature, false).is_err());
    }
}
