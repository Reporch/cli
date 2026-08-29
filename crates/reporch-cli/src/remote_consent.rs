use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use studio_native_auth::{KeyringTokenStore, NativeAuthConfig};
use tempfile::NamedTempFile;

const CONSENT_SCHEMA: &str = "reporch.remote-fallback-consents.v1";
const MAX_CONSENT_BYTES: u64 = 64 * 1024;
const DEFAULT_ISSUER: &str = "https://reporch.com/oauth";
const DEFAULT_CLIENT_ID: &str = "reporch-studio-cli";

#[derive(Clone, Debug, Default)]
struct RemoteFallbackPolicy {
    explicitly_allowed: bool,
    no_input: bool,
    profile: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsentStoreV1 {
    schema: String,
    #[serde(default)]
    entries: BTreeMap<String, ConsentEntryV1>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsentEntryV1 {
    api_url: String,
    issuer: String,
    client_id: String,
    profile: String,
    credential_fingerprint: String,
    consented_at: DateTime<Utc>,
}

fn policy() -> &'static Mutex<RemoteFallbackPolicy> {
    static POLICY: OnceLock<Mutex<RemoteFallbackPolicy>> = OnceLock::new();
    POLICY.get_or_init(|| Mutex::new(RemoteFallbackPolicy::default()))
}

pub fn configure(explicitly_allowed: bool, no_input: bool, profile: Option<&str>) {
    let mut policy = policy().lock().expect("remote fallback policy lock");
    policy.explicitly_allowed = explicitly_allowed;
    policy.no_input = no_input;
    policy.profile = profile.unwrap_or("default").to_owned();
}

pub async fn authorize(project_directory: &Path) -> Result<()> {
    let policy = policy()
        .lock()
        .expect("remote fallback policy lock")
        .clone();
    if policy.explicitly_allowed {
        return Ok(());
    }
    if policy.no_input || !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(reporch_runtime_core::RuntimeError::RemoteFallbackNotAllowed.into());
    }

    let state = crate::local_project::read_local_state(project_directory)?;
    let remote = state
        .remote
        .context("Studio remote fallback requires a linked project")?;
    let issuer = configured_value("REPORCH_STUDIO_OIDC_ISSUER", DEFAULT_ISSUER);
    let client_id = configured_value("REPORCH_STUDIO_CLI_CLIENT_ID", DEFAULT_CLIENT_ID);
    let credential_fingerprint = credential_fingerprint(&issuer, &client_id).await?;
    let entry_key = consent_key(&remote.api_url, &issuer, &client_id, &policy.profile);
    let path = consent_path()?;
    let store = read_store(&path)?;
    if store.entries.get(&entry_key).is_some_and(|entry| {
        entry.api_url == remote.api_url
            && entry.issuer == issuer
            && entry.client_id == client_id
            && entry.profile == policy.profile
            && entry.credential_fingerprint == credential_fingerprint
    }) {
        return Ok(());
    }

    eprintln!("This computer cannot run the local Reporch VM.");
    eprintln!("Reporch can run this preview in Studio instead:");
    eprintln!("  Upload target: {}", remote.api_url);
    eprintln!("  Uploaded data: the current AuthoringSpec snapshot and missing CAS files only");
    eprintln!("  Quota: the execution is charged to your Studio preview quota");
    eprintln!("  Retention: preview inputs and output expire after 24 hours");
    eprint!("Allow remote fallback for this account and profile? [y/N] ");
    std::io::stderr().flush().context("flush consent prompt")?;
    let accepted = tokio::task::spawn_blocking(|| {
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        Ok::<_, std::io::Error>(matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    })
    .await
    .context("join consent prompt")??;
    if !accepted {
        return Err(reporch_runtime_core::RuntimeError::RemoteFallbackNotAllowed.into());
    }

    let mut store = read_store(&path)?;
    store.entries.insert(
        entry_key,
        ConsentEntryV1 {
            api_url: remote.api_url,
            issuer,
            client_id,
            profile: policy.profile,
            credential_fingerprint,
            consented_at: Utc::now(),
        },
    );
    write_store(&path, &store)
}

pub fn clear_for_auth(issuer: &str, client_id: &str, profile: Option<&str>) -> Result<()> {
    let path = consent_path()?;
    if !path.exists() {
        return Ok(());
    }
    let profile = profile.unwrap_or("default");
    let mut store = read_store(&path)?;
    store.entries.retain(|_, entry| {
        entry.issuer != issuer || entry.client_id != client_id || entry.profile != profile
    });
    write_store(&path, &store)
}

async fn credential_fingerprint(issuer: &str, client_id: &str) -> Result<String> {
    if let Ok(subject) = std::env::var("REPORCH_STUDIO_DEV_SUBJECT") {
        ensure!(
            !subject.trim().is_empty() && subject.len() <= 255,
            "development subject is invalid"
        );
        return Ok(digest_fields(&[
            "development",
            issuer,
            client_id,
            subject.trim(),
        ]));
    }
    let allow_insecure_http = configured_bool("REPORCH_STUDIO_ALLOW_INSECURE_HTTP")?;
    let config = NativeAuthConfig::device(
        issuer,
        client_id,
        vec![
            "openid".into(),
            "profile".into(),
            "offline_access".into(),
            "studio:entitlements".into(),
        ],
        allow_insecure_http,
    )?;
    config
        .local_credential_fingerprint(&KeyringTokenStore)
        .await?
        .context("sign in to Reporch before allowing Studio remote fallback")
}

fn configured_value(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn configured_bool(name: &str) -> Result<bool> {
    match std::env::var(name) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => Ok(true),
            "0" | "false" | "no" => Ok(false),
            _ => bail!("{name} must be true or false"),
        },
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn consent_key(api_url: &str, issuer: &str, client_id: &str, profile: &str) -> String {
    digest_fields(&[api_url.trim_end_matches('/'), issuer, client_id, profile])
}

fn digest_fields(fields: &[&str]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"reporch.remote-fallback-consent-key.v1\0");
    for field in fields {
        digest.update(field.as_bytes());
        digest.update(b"\0");
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn consent_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("REPORCH_CONFIG_HOME") {
        ensure!(!path.is_empty(), "REPORCH_CONFIG_HOME cannot be empty");
        return Ok(PathBuf::from(path).join("runtime-consents.json"));
    }
    #[cfg(target_os = "windows")]
    {
        let root = std::env::var_os("APPDATA").context("APPDATA is not set")?;
        Ok(PathBuf::from(root)
            .join("Reporch")
            .join("runtime-consents.json"))
    }
    #[cfg(target_os = "macos")]
    {
        let root = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(root)
            .join("Library")
            .join("Application Support")
            .join("Reporch")
            .join("runtime-consents.json"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(root)
                .join("reporch")
                .join("runtime-consents.json"));
        }
        let root = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(root)
            .join(".config")
            .join("reporch")
            .join("runtime-consents.json"))
    }
}

fn read_store(path: &Path) -> Result<ConsentStoreV1> {
    if !path.exists() {
        return Ok(ConsentStoreV1 {
            schema: CONSENT_SCHEMA.into(),
            entries: BTreeMap::new(),
        });
    }
    let path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect consent file {}", path.display()))?;
    ensure!(
        path_metadata.is_file() && !path_metadata.file_type().is_symlink(),
        "consent file must be a regular, non-symlink file"
    );
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.len() <= MAX_CONSENT_BYTES,
        "consent file is too large"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        ensure!(
            path_metadata.dev() == metadata.dev() && path_metadata.ino() == metadata.ino(),
            "consent file changed while being opened"
        );
        ensure!(
            metadata.mode() & 0o077 == 0,
            "consent file must be private to the current user"
        );
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take(MAX_CONSENT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 == metadata.len(), "consent file changed");
    let store: ConsentStoreV1 = serde_json::from_slice(&bytes).context("parse consent file")?;
    ensure!(
        store.schema == CONSENT_SCHEMA && store.entries.len() <= 128,
        "unsupported or oversized consent file"
    );
    Ok(store)
}

fn write_store(path: &Path, store: &ConsentStoreV1) -> Result<()> {
    ensure!(
        store.schema == CONSENT_SCHEMA && store.entries.len() <= 128,
        "invalid consent store"
    );
    let parent = path.parent().context("consent path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    ensure!(
        parent_metadata.is_dir() && !parent_metadata.file_type().is_symlink(),
        "consent directory must be a real directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let bytes = serde_json::to_vec_pretty(store)?;
    ensure!(
        bytes.len() as u64 <= MAX_CONSENT_BYTES,
        "consent file is too large"
    );
    let mut temporary = NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(&bytes)?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace {}", path.display()))?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_store_is_account_profile_and_origin_bound() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("runtime-consents.json");
        let key = consent_key(
            "https://studio.reporch.com",
            DEFAULT_ISSUER,
            DEFAULT_CLIENT_ID,
            "default",
        );
        let mut store = ConsentStoreV1 {
            schema: CONSENT_SCHEMA.into(),
            entries: BTreeMap::new(),
        };
        store.entries.insert(
            key.clone(),
            ConsentEntryV1 {
                api_url: "https://studio.reporch.com".into(),
                issuer: DEFAULT_ISSUER.into(),
                client_id: DEFAULT_CLIENT_ID.into(),
                profile: "default".into(),
                credential_fingerprint: "a".repeat(64),
                consented_at: Utc::now(),
            },
        );
        write_store(&path, &store).unwrap();
        let loaded = read_store(&path).unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[&key].credential_fingerprint, "a".repeat(64));
        store.entries.get_mut(&key).unwrap().credential_fingerprint = "b".repeat(64);
        write_store(&path, &store).unwrap();
        let replaced = read_store(&path).unwrap();
        assert_eq!(
            replaced.entries[&key].credential_fingerprint,
            "b".repeat(64)
        );
        assert_ne!(
            key,
            consent_key(
                "https://preview.reporch.com",
                DEFAULT_ISSUER,
                DEFAULT_CLIENT_ID,
                "default"
            )
        );
        assert_ne!(
            key,
            consent_key(
                "https://studio.reporch.com",
                DEFAULT_ISSUER,
                DEFAULT_CLIENT_ID,
                "ci"
            )
        );
    }
}
