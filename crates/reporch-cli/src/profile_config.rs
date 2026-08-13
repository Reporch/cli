use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

const CONFIG_VERSION: u32 = 1;
const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const REEXEC_MARKER: &str = "REPORCH_INTERNAL_PROFILE_APPLIED";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserConfigV1 {
    version: u32,
    profiles: BTreeMap<String, ConnectionProfileV1>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectionProfileV1 {
    studio_api_url: Option<String>,
    oidc_issuer: Option<String>,
    cli_client_id: Option<String>,
    studio_web_url: Option<String>,
    allow_insecure_http: Option<bool>,
}

pub fn bootstrap() -> Result<Option<i32>> {
    if std::env::var_os(REEXEC_MARKER).is_some() {
        return Ok(None);
    }
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    let profile_name = selected_profile(&arguments)
        .or_else(|| std::env::var("REPORCH_PROFILE").ok())
        .map(|profile| profile.trim().to_owned());
    let Some(profile_name) = profile_name else {
        return Ok(None);
    };
    validate_profile_name(&profile_name)?;

    let path = user_config_path()?;
    let config = read_user_config(&path)?;
    let profile = config.profiles.get(&profile_name).with_context(|| {
        format!(
            "profile {profile_name:?} does not exist in {}",
            path.display()
        )
    })?;

    let executable = std::env::current_exe().context("resolve current Reporch CLI executable")?;
    let mut child = Command::new(executable);
    child
        .args(&arguments)
        .env(REEXEC_MARKER, "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (name, value) in profile.environment() {
        if std::env::var_os(name).is_none() {
            child.env(name, value);
        }
    }
    let status = child.status().context("run CLI with selected profile")?;
    Ok(Some(exit_code(status)))
}

impl ConnectionProfileV1 {
    fn environment(&self) -> Vec<(&'static str, String)> {
        let mut values = Vec::new();
        if let Some(value) = &self.studio_api_url {
            values.push(("REPORCH_STUDIO_API_URL", value.clone()));
        }
        if let Some(value) = &self.oidc_issuer {
            values.push(("REPORCH_STUDIO_OIDC_ISSUER", value.clone()));
        }
        if let Some(value) = &self.cli_client_id {
            values.push(("REPORCH_STUDIO_CLI_CLIENT_ID", value.clone()));
        }
        if let Some(value) = &self.studio_web_url {
            values.push(("REPORCH_STUDIO_WEB_URL", value.clone()));
        }
        if let Some(value) = self.allow_insecure_http {
            values.push(("REPORCH_STUDIO_ALLOW_INSECURE_HTTP", value.to_string()));
        }
        values
    }
}

fn selected_profile(arguments: &[OsString]) -> Option<String> {
    let mut selected = None;
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        if argument == "--profile" {
            selected = arguments
                .next()
                .and_then(|value| value.to_str())
                .map(str::to_owned);
        } else if let Some(argument) = argument.to_str()
            && let Some(value) = argument.strip_prefix("--profile=")
        {
            selected = Some(value.to_owned());
        }
    }
    selected
}

fn validate_profile_name(profile: &str) -> Result<()> {
    ensure!(
        (1..=64).contains(&profile.len())
            && profile
                .bytes()
                .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') }),
        "profile names must contain 1-64 ASCII letters, digits, dots, underscores, or hyphens"
    );
    Ok(())
}

fn user_config_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("REPORCH_CONFIG_HOME") {
        ensure!(!path.is_empty(), "REPORCH_CONFIG_HOME cannot be empty");
        return Ok(PathBuf::from(path).join("config.toml"));
    }
    #[cfg(target_os = "windows")]
    {
        let root = std::env::var_os("APPDATA").context("APPDATA is not set")?;
        Ok(PathBuf::from(root).join("Reporch").join("config.toml"))
    }
    #[cfg(target_os = "macos")]
    {
        let root = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(root)
            .join("Library")
            .join("Application Support")
            .join("Reporch")
            .join("config.toml"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(root).join("reporch").join("config.toml"));
        }
        let root = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(root)
            .join(".config")
            .join("reporch")
            .join("config.toml"))
    }
}

fn read_user_config(path: &Path) -> Result<UserConfigV1> {
    let path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect user config {}", path.display()))?;
    ensure!(
        path_metadata.is_file() && !path_metadata.file_type().is_symlink(),
        "user config must be a regular, non-symlink file"
    );
    let parent = path
        .parent()
        .context("user config has no parent directory")?;
    let parent_metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("inspect user config directory {}", parent.display()))?;
    ensure!(
        parent_metadata.is_dir() && !parent_metadata.file_type().is_symlink(),
        "user config directory must be a real directory"
    );
    let mut file =
        fs::File::open(path).with_context(|| format!("open user config {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect open user config {}", path.display()))?;
    ensure!(
        metadata.is_file() && metadata.len() > 0 && metadata.len() <= MAX_CONFIG_BYTES,
        "user config must contain at most {MAX_CONFIG_BYTES} bytes"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        ensure!(
            path_metadata.dev() == metadata.dev() && path_metadata.ino() == metadata.ino(),
            "user config changed while being opened"
        );
        ensure!(
            metadata.mode() & 0o022 == 0 && parent_metadata.mode() & 0o022 == 0,
            "user config and its directory cannot be writable by group or other users"
        );
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read user config {}", path.display()))?;
    ensure!(
        bytes.len() as u64 == metadata.len(),
        "user config changed while being read"
    );
    let text = std::str::from_utf8(&bytes).context("user config must be UTF-8")?;
    let config: UserConfigV1 = toml::from_str(text).context("parse user config TOML")?;
    ensure!(
        config.version == CONFIG_VERSION,
        "unsupported user config version {}",
        config.version
    );
    ensure!(
        !config.profiles.is_empty() && config.profiles.len() <= 64,
        "user config must contain 1-64 profiles"
    );
    for profile in config.profiles.keys() {
        validate_profile_name(profile)?;
    }
    Ok(config)
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_explicit_profile_wins_and_equals_syntax_is_supported() {
        let arguments = [
            OsString::from("--profile"),
            OsString::from("first"),
            OsString::from("--profile=second"),
        ];
        assert_eq!(selected_profile(&arguments).as_deref(), Some("second"));
    }

    #[test]
    fn config_is_bounded_strict_and_maps_only_non_secret_connection_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            br#"
version = 1
[profiles.production]
studio_api_url = "https://studio.reporch.com"
oidc_issuer = "https://reporch.com/oauth"
cli_client_id = "reporch-studio-cli"
allow_insecure_http = false
"#,
        )
        .unwrap();
        let config = read_user_config(&path).unwrap();
        let environment = config.profiles["production"].environment();
        assert!(
            environment
                .iter()
                .any(|(name, value)| *name == "REPORCH_STUDIO_API_URL"
                    && value == "https://studio.reporch.com")
        );
        assert!(environment.iter().all(|(name, _)| !name.contains("TOKEN")));

        fs::write(
            &path,
            "version = 1\n[profiles.production]\ntoken = \"must-not-be-accepted\"\n",
        )
        .unwrap();
        assert!(read_user_config(&path).is_err());
    }

    #[test]
    fn unsafe_profile_names_are_rejected() {
        for profile in ["", "../prod", "prod test", &"x".repeat(65)] {
            assert!(validate_profile_name(profile).is_err());
        }
    }
}
