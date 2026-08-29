#[cfg(target_os = "macos")]
use anyhow::Context as _;
use anyhow::{Result, ensure};
use reporch_runtime_core::RuntimeBundleManifestV1;

pub(crate) fn ensure_runtime_supported(manifest: &RuntimeBundleManifestV1) -> Result<()> {
    let actual = current_os_version()?;
    ensure!(
        version_at_least(&actual, &manifest.minimum_os_version)?,
        "runtime requires operating system {} or newer, but this host is {}",
        manifest.minimum_os_version,
        actual
    );
    Ok(())
}

fn version_at_least(actual: &str, required: &str) -> Result<bool> {
    let actual = version_components(actual, true)?;
    let required = version_components(required, false)?;
    let width = actual.len().max(required.len());
    for index in 0..width {
        let actual = actual.get(index).copied().unwrap_or(0);
        let required = required.get(index).copied().unwrap_or(0);
        match actual.cmp(&required) {
            std::cmp::Ordering::Greater => return Ok(true),
            std::cmp::Ordering::Less => return Ok(false),
            std::cmp::Ordering::Equal => {}
        }
    }
    Ok(true)
}

fn version_components(value: &str, allow_release_suffix: bool) -> Result<Vec<u64>> {
    ensure!(
        !value.is_empty() && value.len() <= 128,
        "operating system version is invalid"
    );
    let parts = value.split('.').collect::<Vec<_>>();
    ensure!(
        !parts.is_empty() && parts.len() <= 8,
        "operating system version has too many components"
    );
    let mut components = Vec::with_capacity(parts.len());
    for (index, part) in parts.iter().enumerate() {
        let digits = part
            .as_bytes()
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        ensure!(
            digits > 0
                && (allow_release_suffix || digits == part.len())
                && (index + 1 == parts.len() || digits == part.len()),
            "operating system version component is invalid"
        );
        components.push(part[..digits].parse::<u64>()?);
        if digits != part.len() {
            break;
        }
    }
    Ok(components)
}

#[cfg(target_os = "linux")]
fn current_os_version() -> Result<String> {
    let release = rustix::system::uname()
        .release()
        .to_string_lossy()
        .into_owned();
    ensure!(!release.is_empty(), "Linux kernel version is unavailable");
    Ok(release)
}

#[cfg(target_os = "macos")]
fn current_os_version() -> Result<String> {
    let output = std::process::Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .context("read macOS product version")?;
    ensure!(
        output.status.success() && output.stdout.len() <= 128,
        "macOS product version is unavailable"
    );
    let value = String::from_utf8(output.stdout)?.trim().to_owned();
    ensure!(!value.is_empty(), "macOS product version is unavailable");
    Ok(value)
}

#[cfg(windows)]
fn current_os_version() -> Result<String> {
    super::windows_identity::current_os_version()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn current_os_version() -> Result<String> {
    anyhow::bail!("this operating system has no Reporch runtime version probe")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_versions_are_compared_without_lexical_errors() {
        assert!(version_at_least("15.2", "13.0").unwrap());
        assert!(version_at_least("10.0.19045.0", "10.0.19041.0").unwrap());
        assert!(version_at_least("6.8.0-101", "5.10").unwrap());
        assert!(!version_at_least("5.4.0", "5.10").unwrap());
        assert!(!version_at_least("10.0.18362.0", "10.0.19041.0").unwrap());
    }

    #[test]
    fn signed_minimum_version_must_be_strictly_numeric() {
        assert!(version_at_least("15.0", "13-beta").is_err());
        assert!(version_at_least("15.0", "13..0").is_err());
    }
}
