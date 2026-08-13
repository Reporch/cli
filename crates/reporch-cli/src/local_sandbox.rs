use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OciRuntime {
    Auto,
    Podman,
    Docker,
}

#[derive(Debug, Clone)]
pub struct LocalSandboxOptions {
    pub runtime: OciRuntime,
    pub image: String,
    pub project_directory: PathBuf,
    pub command: Vec<String>,
    pub timeout: Duration,
    pub memory_mib: u64,
    pub cpus: f64,
    pub output_kib: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalSandboxPlan {
    schema: &'static str,
    runtime: String,
    container_name: String,
    image: String,
    project_directory: PathBuf,
    arguments: Vec<String>,
    timeout_seconds: u64,
    output_limit_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalSandboxResult {
    pub schema: &'static str,
    pub runtime: String,
    pub image: String,
    pub exit_code: i32,
    pub duration_ms: u128,
    pub stdout: String,
    pub stderr: String,
}

pub async fn plan(options: &LocalSandboxOptions) -> Result<LocalSandboxPlan> {
    validate_options(options)?;
    let runtime = resolve_secure_runtime(options.runtime).await?;
    let project_directory = canonical_project_directory(&options.project_directory)?;
    let container_name = format!("reporch-studio-local-{}", Uuid::now_v7().simple());
    let arguments = container_arguments(options, &project_directory, &container_name)?;
    Ok(LocalSandboxPlan {
        schema: "reporch.local-sandbox-plan.v1",
        runtime,
        container_name,
        image: options.image.clone(),
        project_directory,
        arguments,
        timeout_seconds: options.timeout.as_secs(),
        output_limit_bytes: options.output_kib * 1024,
    })
}

pub async fn execute(plan: &LocalSandboxPlan) -> Result<LocalSandboxResult> {
    if plan.schema != "reporch.local-sandbox-plan.v1" {
        bail!("unsupported local sandbox plan");
    }
    require_rootless(&plan.runtime).await?;
    let started = Instant::now();
    let mut child = Command::new(&plan.runtime)
        .args(&plan.arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("start rootless {} sandbox", plan.runtime))?;
    let stdout = child.stdout.take().context("capture sandbox stdout")?;
    let stderr = child.stderr.take().context("capture sandbox stderr")?;
    let output_limit =
        usize::try_from(plan.output_limit_bytes).context("output limit too large")?;
    let stdout_task = tokio::spawn(read_bounded(stdout, output_limit));
    let stderr_task = tokio::spawn(read_bounded(stderr, output_limit));
    let status =
        match tokio::time::timeout(Duration::from_secs(plan.timeout_seconds), child.wait()).await {
            Ok(result) => result.context("wait for local sandbox")?,
            Err(_) => {
                let _ = child.kill().await;
                remove_exact_container(&plan.runtime, &plan.container_name).await;
                bail!("local sandbox exceeded {} seconds", plan.timeout_seconds);
            }
        };
    let (stdout, stdout_truncated) = stdout_task.await.context("join stdout reader")??;
    let (stderr, stderr_truncated) = stderr_task.await.context("join stderr reader")??;
    if stdout_truncated || stderr_truncated {
        bail!(
            "local sandbox output exceeded {} bytes per stream",
            output_limit
        );
    }
    Ok(LocalSandboxResult {
        schema: "reporch.local-sandbox-result.v1",
        runtime: plan.runtime.clone(),
        image: plan.image.clone(),
        exit_code: status.code().unwrap_or(128),
        duration_ms: started.elapsed().as_millis(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

fn validate_options(options: &LocalSandboxOptions) -> Result<()> {
    validate_image(&options.image)?;
    if options.command.is_empty()
        || options.command.len() > 256
        || options.command.iter().any(|argument| {
            argument.is_empty() || argument.len() > 4_096 || argument.contains('\0')
        })
    {
        bail!("sandbox command must contain 1-256 bounded arguments");
    }
    if !(Duration::from_secs(1)..=Duration::from_secs(600)).contains(&options.timeout) {
        bail!("sandbox timeout must be between 1 and 600 seconds");
    }
    if !(16..=8_192).contains(&options.memory_mib) {
        bail!("sandbox memory must be between 16 and 8192 MiB");
    }
    if !options.cpus.is_finite() || !(0.1..=16.0).contains(&options.cpus) {
        bail!("sandbox CPUs must be between 0.1 and 16");
    }
    if !(1..=1_048_576).contains(&options.output_kib) {
        bail!("sandbox output must be between 1 KiB and 1 GiB");
    }
    Ok(())
}

pub(crate) fn validate_image(image: &str) -> Result<()> {
    let Some((name, digest)) = image.rsplit_once("@sha256:") else {
        bail!("sandbox image must be pinned as name@sha256:<64 lowercase hex>");
    };
    if name.is_empty()
        || name.len() > 512
        || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'/' | b':' | b'_' | b'-')
        })
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("sandbox image must be pinned as name@sha256:<64 lowercase hex>");
    }
    Ok(())
}

fn canonical_project_directory(path: &Path) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .context("resolve sandbox project directory")?;
    if !path.is_dir() || path.to_string_lossy().contains(',') {
        bail!("sandbox project directory must be a directory without commas");
    }
    Ok(path)
}

fn container_arguments(
    options: &LocalSandboxOptions,
    project_directory: &Path,
    container_name: &str,
) -> Result<Vec<String>> {
    let mount = format!(
        "type=bind,src={},dst=/workspace,readonly",
        project_directory
            .to_str()
            .context("sandbox project path must be UTF-8")?
    );
    let mut arguments = vec![
        "run".into(),
        "--rm".into(),
        "--pull=never".into(),
        "--name".into(),
        container_name.into(),
        "--label".into(),
        "com.reporch.studio.local-sandbox=true".into(),
        "--network=none".into(),
        "--read-only".into(),
        "--cap-drop=ALL".into(),
        "--security-opt=no-new-privileges".into(),
        "--pids-limit=64".into(),
        "--memory".into(),
        format!("{}m", options.memory_mib),
        "--cpus".into(),
        options.cpus.to_string(),
        "--user=65534:65534".into(),
        "--workdir=/workspace".into(),
        "--mount".into(),
        mount,
        "--tmpfs".into(),
        "/tmp:rw,noexec,nosuid,nodev,size=67108864".into(),
        options.image.clone(),
    ];
    arguments.extend(options.command.iter().cloned());
    Ok(arguments)
}

async fn resolve_runtime(runtime: OciRuntime) -> Result<String> {
    match runtime {
        OciRuntime::Podman => runtime_available("podman").await.then(|| "podman".into()),
        OciRuntime::Docker => runtime_available("docker").await.then(|| "docker".into()),
        OciRuntime::Auto => {
            if runtime_available("podman").await {
                Some("podman".into())
            } else if runtime_available("docker").await {
                Some("docker".into())
            } else {
                None
            }
        }
    }
    .context("no requested OCI runtime is available")
}

pub async fn resolve_secure_runtime(runtime: OciRuntime) -> Result<String> {
    let runtime = resolve_runtime(runtime).await?;
    require_rootless(&runtime).await?;
    Ok(runtime)
}

async fn runtime_available(runtime: &str) -> bool {
    Command::new(runtime)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

async fn require_rootless(runtime: &str) -> Result<()> {
    let output = if runtime == "podman" {
        Command::new(runtime)
            .args(["info", "--format", "json"])
            .output()
            .await
            .context("inspect Podman security mode")?
    } else if runtime == "docker" {
        Command::new(runtime)
            .args(["info", "--format", "{{json .SecurityOptions}}"])
            .output()
            .await
            .context("inspect Docker security mode")?
    } else {
        bail!("unsupported OCI runtime");
    };
    if !output.status.success() {
        bail!("OCI runtime security inspection failed");
    }
    let rootless = if runtime == "podman" {
        let value: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("parse Podman info")?;
        value
            .pointer("/host/security/rootless")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    } else {
        let options: Vec<String> =
            serde_json::from_slice(&output.stdout).context("parse Docker security options")?;
        options.iter().any(|option| option.contains("rootless"))
    };
    if !rootless {
        bail!("local sandbox requires a rootless Podman or Docker daemon");
    }
    Ok(())
}

pub(crate) async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<(Vec<u8>, bool), std::io::Error> {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
        truncated |= read > remaining;
    }
    Ok((output, truncated))
}

async fn remove_exact_container(runtime: &str, container_name: &str) {
    let _ = Command::new(runtime)
        .args(["rm", "--force", container_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(directory: PathBuf) -> LocalSandboxOptions {
        LocalSandboxOptions {
            runtime: OciRuntime::Podman,
            image: format!("registry.test/toolchain@sha256:{}", "a".repeat(64)),
            project_directory: directory,
            command: vec!["python3".into(), "solutions/accepted.py".into()],
            timeout: Duration::from_secs(10),
            memory_mib: 256,
            cpus: 1.0,
            output_kib: 1_024,
        }
    }

    #[test]
    fn image_reference_is_immutable_and_lowercase() {
        assert!(validate_image(&format!("repo/image@sha256:{}", "f".repeat(64))).is_ok());
        for image in [
            "repo/image:latest",
            "repo/image@sha256:abc",
            &format!("repo/image@sha256:{}", "F".repeat(64)),
        ] {
            assert!(validate_image(image).is_err(), "accepted {image}");
        }
    }

    #[test]
    fn command_spec_is_networkless_read_only_and_least_privilege() {
        let directory = tempfile::tempdir().expect("temp directory");
        let options = options(directory.path().to_owned());
        validate_options(&options).expect("valid options");
        let name = "reporch-studio-local-test";
        let arguments = container_arguments(
            &options,
            &directory
                .path()
                .canonicalize()
                .expect("canonical directory"),
            name,
        )
        .expect("build arguments");
        for required in [
            "--pull=never",
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--user=65534:65534",
        ] {
            assert!(arguments.iter().any(|argument| argument == required));
        }
        let image_index = arguments
            .iter()
            .position(|argument| argument == &options.image)
            .expect("image argument");
        assert_eq!(arguments[image_index + 1], "python3");
        assert!(
            arguments
                .iter()
                .any(|argument| argument.ends_with(",readonly"))
        );
    }

    #[tokio::test]
    async fn bounded_reader_drains_but_never_retains_excess_output() {
        let input = vec![b'x'; 32];
        let (output, truncated) = read_bounded(input.as_slice(), 8).await.expect("read");
        assert_eq!(output, vec![b'x'; 8]);
        assert!(truncated);
    }
}
