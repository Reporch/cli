use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use reporch_runtime_core::{
    ContentObjectV1, GuestJobV1, GuestOperationV1, GuestOutputEncodingV1, ResourceLimitsV1,
    RuntimeAvailability, RuntimeError,
};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use uuid::Uuid;

const RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const RUNTIME_SECURITY_INSPECTION_TIMEOUT: Duration = Duration::from_secs(8);
const RUNTIME_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const RUNTIME_DIAGNOSTIC_OUTPUT_LIMIT: usize = 64 * 1024;
pub fn configure_remote_fallback(allowed: bool, no_input: bool, profile: Option<&str>) {
    crate::remote_consent::configure(allowed, no_input, profile);
}

#[derive(Debug)]
pub(crate) struct BoundedCommandOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub _stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

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
    toolchain_id: Option<String>,
    inputs: Vec<ContentObjectV1>,
    timeout_seconds: u64,
    memory_mib: u64,
    cpu_millis: u32,
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
    #[serde(skip)]
    pub stdout_bytes: Vec<u8>,
}

pub async fn plan(options: &LocalSandboxOptions) -> Result<LocalSandboxPlan> {
    validate_options(options)?;
    let project_directory = canonical_project_directory(&options.project_directory)?;
    let cpu_millis = cpu_millis(options.cpus)?;
    if options.runtime == OciRuntime::Auto {
        let entry = crate::toolchain::resolve_for_image(&options.image)?;
        let inputs = inventory_native_inputs(&project_directory, &options.command)?;
        if options.output_kib > 4_096 {
            bail!("native runtime output must be at most 4096 KiB per stream");
        }
        return Ok(LocalSandboxPlan {
            schema: "reporch.local-sandbox-plan.v1",
            runtime: "reporch_vm".into(),
            container_name: String::new(),
            image: options.image.clone(),
            project_directory,
            arguments: options.command.clone(),
            toolchain_id: Some(entry.id),
            inputs,
            timeout_seconds: options.timeout.as_secs(),
            memory_mib: options.memory_mib,
            cpu_millis,
            output_limit_bytes: options.output_kib * 1024,
        });
    }

    let runtime = resolve_secure_runtime(options.runtime).await?;
    let container_name = format!("reporch-studio-local-{}", Uuid::now_v7().simple());
    let arguments = container_arguments(options, &project_directory, &container_name)?;
    Ok(LocalSandboxPlan {
        schema: "reporch.local-sandbox-plan.v1",
        runtime,
        container_name,
        image: options.image.clone(),
        project_directory,
        arguments,
        toolchain_id: None,
        inputs: Vec::new(),
        timeout_seconds: options.timeout.as_secs(),
        memory_mib: options.memory_mib,
        cpu_millis,
        output_limit_bytes: options.output_kib * 1024,
    })
}

pub async fn execute(plan: &LocalSandboxPlan) -> Result<LocalSandboxResult> {
    if plan.schema != "reporch.local-sandbox-plan.v1" {
        bail!("unsupported local sandbox plan");
    }
    if plan.runtime == "reporch_vm" {
        return execute_native(plan).await;
    }
    require_rootless(&plan.runtime).await?;
    let started = Instant::now();
    let mut command = Command::new(&plan.runtime);
    command
        .args(&plan.arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_process_group(&mut command);
    let mut child = command
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
                kill_process_tree(&mut child).await;
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
    let stdout_text = String::from_utf8_lossy(&stdout).into_owned();
    Ok(LocalSandboxResult {
        schema: "reporch.local-sandbox-result.v1",
        runtime: plan.runtime.clone(),
        image: plan.image.clone(),
        exit_code: status.code().unwrap_or(128),
        duration_ms: started.elapsed().as_millis(),
        stdout: stdout_text,
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        stdout_bytes: stdout,
    })
}

async fn execute_native(plan: &LocalSandboxPlan) -> Result<LocalSandboxResult> {
    let toolchain_id = plan
        .toolchain_id
        .as_deref()
        .context("native sandbox plan is missing its signed toolchain identity")?;
    if let Err(error) = ensure_native_runtime().await {
        if runtime_fallback_eligible(&error) {
            crate::remote_consent::authorize(&plan.project_directory).await?;
            return execute_remote_preview(plan).await;
        }
        return Err(error);
    }
    let toolchain = reporch_runtime_host::install_toolchain(toolchain_id).await?;

    let job_id = Uuid::now_v7();
    let job = GuestJobV1 {
        schema: reporch_runtime_core::JOB_SCHEMA.into(),
        protocol_version: reporch_runtime_core::PROTOCOL_VERSION,
        id: job_id,
        nonce: format!("{}-{}", job_id.simple(), Uuid::now_v7().simple()),
        operation: GuestOperationV1::Program,
        toolchain_id: toolchain_id.to_owned(),
        toolchain_index_sequence: Some(toolchain.installation.index_sequence),
        toolchain_bundle_sha256: Some(toolchain.installation.bundle_sha256),
        toolchain_lock_sha256: Some(toolchain.installation.toolchain_lock_sha256),
        command: plan.arguments.clone(),
        environment: BTreeMap::new(),
        inputs: plan.inputs.clone(),
        limits: ResourceLimitsV1 {
            timeout_ms: plan.timeout_seconds.saturating_mul(1_000),
            memory_mib: plan.memory_mib,
            cpu_millis: plan.cpu_millis,
            pids: 64,
            stdout_bytes: plan.output_limit_bytes,
            stderr_bytes: plan.output_limit_bytes,
            artifact_bytes: 256 * 1_048_576,
        },
    };
    job.validate().context("validate native runtime job")?;
    let result = match reporch_runtime_host::execute_native(&plan.project_directory, &job).await {
        Ok(result) => result,
        Err(error) if runtime_fallback_eligible(&error) => {
            crate::remote_consent::authorize(&plan.project_directory).await?;
            return execute_remote_preview(plan).await;
        }
        Err(error) => return Err(error),
    };
    if result.stdout.truncated || result.stderr.truncated {
        bail!(
            "local sandbox output exceeded {} bytes per stream",
            plan.output_limit_bytes
        );
    }
    let stdout = decode_guest_output(result.stdout.encoding, &result.stdout.data)?;
    let stderr = decode_guest_output(result.stderr.encoding, &result.stderr.data)?;
    Ok(LocalSandboxResult {
        schema: "reporch.local-sandbox-result.v1",
        runtime: plan.runtime.clone(),
        image: plan.image.clone(),
        exit_code: result.exit_code,
        duration_ms: result.duration_ms.into(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        stdout_bytes: stdout,
    })
}

fn runtime_fallback_eligible(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<RuntimeError>().is_some_and(|runtime| {
            matches!(
                runtime,
                RuntimeError::VirtualizationUnavailable(_) | RuntimeError::ServiceUnavailable(_)
            )
        })
    })
}

async fn execute_remote_preview(plan: &LocalSandboxPlan) -> Result<LocalSandboxResult> {
    let toolchain_id = plan
        .toolchain_id
        .as_deref()
        .context("remote preview plan is missing its toolchain identity")?;
    let entry = crate::toolchain::resolve_for_image(&plan.image)?;
    let (source_path, stdin_path) = remote_preview_paths(&plan.inputs, &entry.language)?;
    if plan.memory_mib > 1_024 || plan.output_limit_bytes > 512 * 1_024 {
        bail!(
            "Studio remote preview supports at most 1024 MiB memory and 512 KiB output; lower the local limits or run official `reporch verify`"
        );
    }
    let started = Instant::now();
    let result = crate::studio_remote::runtime_preview_operation(
        &crate::studio_remote::RuntimePreviewExecutionOptions {
            project_directory: plan.project_directory.clone(),
            operation: studio_contracts::RuntimePreviewOperationV1::Program,
            toolchain_id: toolchain_id.into(),
            language: entry.language,
            source_path,
            stdin_path,
            limits: studio_contracts::ToolExecutionLimitsV1 {
                timeout_ms: plan.timeout_seconds.saturating_mul(1_000),
                memory_mib: plan.memory_mib,
                output_kib: plan.output_limit_bytes.div_ceil(1_024),
            },
            timeout_seconds: plan.timeout_seconds.saturating_add(60).min(7_200),
        },
    )
    .await?;
    let succeeded = result.status == studio_contracts::RuntimePreviewStatusV1::Succeeded;
    let stdout = result.output.unwrap_or_default().into_bytes();
    let stderr = result.error_code.unwrap_or_default();
    Ok(LocalSandboxResult {
        schema: "reporch.local-sandbox-result.v1",
        runtime: "studio_remote".into(),
        image: plan.image.clone(),
        exit_code: if succeeded { 0 } else { 1 },
        duration_ms: started.elapsed().as_millis(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr,
        stdout_bytes: stdout,
    })
}

fn remote_preview_paths(
    inputs: &[ContentObjectV1],
    language: &str,
) -> Result<(String, Option<String>)> {
    let mut sources = inputs
        .iter()
        .filter(|input| source_extension_matches(&input.path, language))
        .map(|input| input.path.clone())
        .collect::<Vec<_>>();
    ensure!(
        sources.len() == 1,
        "Studio remote fallback currently requires exactly one {language} source file; use official `reporch verify` for linked, interactive, or grader executions"
    );
    let source = sources.pop().expect("one source was checked");
    let remaining = inputs
        .iter()
        .filter(|input| input.path != source)
        .map(|input| input.path.clone())
        .collect::<Vec<_>>();
    ensure!(
        remaining.len() <= 1,
        "Studio remote fallback currently accepts at most one stdin file; use official `reporch verify` for multi-file execution"
    );
    Ok((source, remaining.into_iter().next()))
}

fn source_extension_matches(path: &str, language: &str) -> bool {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match language {
        "python" | "pypy" => extension == "py",
        "c" => extension == "c",
        "cpp" => matches!(extension.as_str(), "cc" | "cpp" | "cxx" | "c++"),
        "java" => extension == "java",
        "rust" => extension == "rs",
        "javascript" => matches!(extension.as_str(), "js" | "mjs" | "cjs"),
        "csharp" => extension == "cs",
        "php" => extension == "php",
        "r" => extension == "r",
        "bash" => matches!(extension.as_str(), "sh" | "bash"),
        "swift" => extension == "swift",
        _ => false,
    }
}

async fn ensure_native_runtime() -> Result<()> {
    let mut status = reporch_runtime_host::status().await?;
    if status.availability == RuntimeAvailability::NotInstalled {
        reporch_runtime_host::update().await?;
        status = reporch_runtime_host::status().await?;
    }
    match status.availability {
        RuntimeAvailability::Ready => Ok(()),
        RuntimeAvailability::RemoteOnly => Err(RuntimeError::VirtualizationUnavailable(
            status
                .reason
                .unwrap_or_else(|| "this host cannot run the local Reporch VM".into()),
        )
        .into()),
        RuntimeAvailability::NotInstalled => Err(RuntimeError::BootstrapIncomplete.into()),
        RuntimeAvailability::Broken => Err(RuntimeError::ServiceUnavailable(
            status
                .reason
                .unwrap_or_else(|| "the native runtime is installed but not ready".into()),
        )
        .into()),
    }
}

fn decode_guest_output(encoding: GuestOutputEncodingV1, data: &str) -> Result<Vec<u8>> {
    match encoding {
        GuestOutputEncodingV1::Utf8 => Ok(data.as_bytes().to_vec()),
        GuestOutputEncodingV1::Base64 => base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("decode native runtime output"),
    }
}

fn inventory_native_inputs(root: &Path, command: &[String]) -> Result<Vec<ContentObjectV1>> {
    let mut inputs = BTreeMap::new();
    for argument in command {
        let Some(relative) = argument.strip_prefix("/workspace/") else {
            continue;
        };
        let normalized = studio_core::normalize_relative_path(relative)
            .with_context(|| format!("validate native runtime input {relative}"))?;
        if normalized != relative {
            bail!("native runtime input path is not normalized: {relative}");
        }
        let (sha256, size) = crate::local_project::hash_regular_project_file(root, &normalized)?;
        inputs.insert(
            normalized.clone(),
            ContentObjectV1 {
                path: normalized,
                sha256: format!("sha256:{sha256}"),
                size,
            },
        );
    }
    Ok(inputs.into_values().collect())
}

fn cpu_millis(cpus: f64) -> Result<u32> {
    let millis = (cpus * 1_000.0).round();
    if !(100.0..=16_000.0).contains(&millis) {
        bail!("sandbox CPUs cannot be represented by the native runtime");
    }
    Ok(millis as u32)
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
        "--tmpfs".into(),
        "/run/reporch:rw,exec,nosuid,nodev,size=268435456".into(),
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
    .context(
        "no requested OCI runtime is available; install and start rootless Podman (recommended; on macOS run `podman machine init` then `podman machine start`) or rootless Docker. Reporch intentionally never runs author code directly on the host; use `reporch verify` for official Studio verification",
    )
}

pub async fn resolve_secure_runtime(runtime: OciRuntime) -> Result<String> {
    let runtime = resolve_runtime(runtime).await?;
    require_rootless(&runtime).await?;
    Ok(runtime)
}

async fn runtime_available(runtime: &str) -> bool {
    run_bounded_command(
        runtime,
        &["--version"],
        RUNTIME_PROBE_TIMEOUT,
        RUNTIME_DIAGNOSTIC_OUTPUT_LIMIT,
    )
    .await
    .is_ok_and(|output| output.status.success())
}

async fn require_rootless(runtime: &str) -> Result<()> {
    let output = if runtime == "podman" {
        run_bounded_command(
            runtime,
            &["info", "--format", "json"],
            RUNTIME_SECURITY_INSPECTION_TIMEOUT,
            RUNTIME_DIAGNOSTIC_OUTPUT_LIMIT,
        )
        .await
        .context("inspect Podman security mode")?
    } else if runtime == "docker" {
        run_bounded_command(
            runtime,
            &["info", "--format", "{{json .SecurityOptions}}"],
            RUNTIME_SECURITY_INSPECTION_TIMEOUT,
            RUNTIME_DIAGNOSTIC_OUTPUT_LIMIT,
        )
        .await
        .context("inspect Docker security mode")?
    } else {
        bail!("unsupported OCI runtime");
    };
    if !output.status.success() {
        bail!(
            "OCI runtime security inspection failed; start the rootless {runtime} daemon and retry. Reporch intentionally never runs author code directly on the host; use `reporch verify` for official Studio verification"
        );
    }
    ensure_bounded_diagnostics(&output)?;
    let rootless = if runtime == "podman" {
        let value: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("parse Podman info")?;
        value
            .pointer("/host/security/rootless")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    } else {
        docker_security_options_are_rootless(&output.stdout)?
    };
    if !rootless {
        bail!(
            "local sandbox requires a rootless Podman or Docker daemon; configure rootless mode and restart {runtime} (on macOS, rootless Podman is recommended: run `podman machine init` then `podman machine start`). Reporch intentionally never runs author code directly on the host; use `reporch verify` for official Studio verification"
        );
    }
    Ok(())
}

fn docker_security_options_are_rootless(bytes: &[u8]) -> Result<bool> {
    // Docker Desktop can report JSON `null` while its daemon is unavailable.
    // Treat that as an inspected-but-not-rootless runtime so callers receive
    // the actionable rootless guidance instead of an incidental parse error.
    let options: Option<Vec<String>> =
        serde_json::from_slice(bytes).context("parse Docker security options")?;
    Ok(options
        .unwrap_or_default()
        .iter()
        .any(|option| option.contains("rootless")))
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
    let _ = run_bounded_command(
        runtime,
        &["rm", "--force", container_name],
        RUNTIME_CLEANUP_TIMEOUT,
        RUNTIME_DIAGNOSTIC_OUTPUT_LIMIT,
    )
    .await;
}

pub(crate) async fn run_bounded_command(
    program: &str,
    arguments: &[&str],
    timeout: Duration,
    output_limit: usize,
) -> Result<BoundedCommandOutput> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_process_group(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("start bounded {program} command"))?;
    let stdout = child
        .stdout
        .take()
        .context("capture bounded command stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("capture bounded command stderr")?;
    let stdout_task = tokio::spawn(read_bounded(stdout, output_limit));
    let stderr_task = tokio::spawn(read_bounded(stderr, output_limit));
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status.with_context(|| format!("wait for bounded {program} command"))?,
        Err(_) => {
            kill_process_tree(&mut child).await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            bail!("{program} command exceeded {} seconds", timeout.as_secs());
        }
    };
    let (stdout, stdout_truncated) = stdout_task.await.context("join bounded stdout reader")??;
    let (stderr, stderr_truncated) = stderr_task.await.context("join bounded stderr reader")??;
    Ok(BoundedCommandOutput {
        status,
        stdout,
        _stderr: stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

pub(crate) fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = command;
    }
}

pub(crate) async fn kill_process_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(id) = child.id()
        && let Ok(raw) = i32::try_from(id)
        && let Some(pid) = rustix::process::Pid::from_raw(raw)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill().await;
}

fn ensure_bounded_diagnostics(output: &BoundedCommandOutput) -> Result<()> {
    if output.stdout_truncated || output.stderr_truncated {
        bail!("OCI runtime diagnostic output exceeded 64 KiB");
    }
    Ok(())
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

    fn native_options(directory: PathBuf) -> LocalSandboxOptions {
        LocalSandboxOptions {
            runtime: OciRuntime::Auto,
            image: "python:3.14-slim@sha256:44dd04494ee8f3b538294360e7c4b3acb87c8268e4d0a4828a6500b1eff50061".into(),
            project_directory: directory,
            command: vec!["python3".into(), "/workspace/solution.py".into()],
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
    fn docker_security_options_treat_null_as_not_rootless() {
        assert!(!docker_security_options_are_rootless(b"null").unwrap());
        assert!(docker_security_options_are_rootless(br#"["name=rootless"]"#).unwrap());
        assert!(docker_security_options_are_rootless(br#"{}"#).is_err());
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
    async fn auto_plan_uses_native_vm_and_only_inventories_referenced_inputs() {
        let directory = tempfile::tempdir().expect("temp directory");
        std::fs::write(directory.path().join("solution.py"), b"print(42)\n")
            .expect("write referenced input");
        std::fs::write(directory.path().join("not-shared.txt"), b"host only\n")
            .expect("write unrelated project file");

        let plan = plan(&native_options(directory.path().to_owned()))
            .await
            .expect("plan native runtime without probing Docker");
        assert_eq!(plan.runtime, "reporch_vm");
        assert_eq!(plan.toolchain_id.as_deref(), Some("python-3.14"));
        assert_eq!(plan.inputs.len(), 1);
        assert_eq!(plan.inputs[0].path, "solution.py");
        assert_eq!(plan.inputs[0].size, 10);
        assert!(plan.inputs[0].sha256.starts_with("sha256:"));
        assert!(plan.container_name.is_empty());
    }

    #[tokio::test]
    async fn native_plan_rejects_workspace_traversal() {
        let directory = tempfile::tempdir().expect("temp directory");
        let mut options = native_options(directory.path().to_owned());
        options.command[1] = "/workspace/../outside.py".into();
        let error = plan(&options).await.expect_err("reject traversal");
        assert!(error.to_string().contains("native runtime input"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_plan_rejects_symlink_inputs() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temp directory");
        std::fs::write(directory.path().join("real.py"), b"print(42)\n").unwrap();
        symlink("real.py", directory.path().join("solution.py")).unwrap();
        let error = plan(&native_options(directory.path().to_owned()))
            .await
            .expect_err("reject symlink");
        assert!(error.to_string().contains("not a regular file"));
    }

    #[test]
    fn remote_preview_selects_one_source_and_one_stdin_without_guessing_multi_source_jobs() {
        let object = |path: &str| ContentObjectV1 {
            path: path.into(),
            sha256: format!("sha256:{}", "a".repeat(64)),
            size: 1,
        };
        let (source, stdin) = remote_preview_paths(
            &[object("solutions/main.py"), object("tests/01.in")],
            "python",
        )
        .unwrap();
        assert_eq!(source, "solutions/main.py");
        assert_eq!(stdin.as_deref(), Some("tests/01.in"));
        assert!(
            remote_preview_paths(
                &[
                    object("grader.cpp"),
                    object("solution.cpp"),
                    object("tests/01.in")
                ],
                "cpp"
            )
            .is_err()
        );
    }

    #[test]
    fn remote_fallback_is_limited_to_unavailable_native_backends() {
        assert!(runtime_fallback_eligible(&anyhow::Error::new(
            RuntimeError::VirtualizationUnavailable("disabled".into())
        )));
        assert!(runtime_fallback_eligible(&anyhow::Error::new(
            RuntimeError::ServiceUnavailable("missing".into())
        )));
        assert!(!runtime_fallback_eligible(&anyhow::anyhow!(
            "toolchain signature mismatch"
        )));
    }

    #[tokio::test]
    async fn bounded_reader_drains_but_never_retains_excess_output() {
        let input = vec![b'x'; 32];
        let (output, truncated) = read_bounded(input.as_slice(), 8).await.expect("read");
        assert_eq!(output, vec![b'x'; 8]);
        assert!(truncated);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unresponsive_runtime_probe_is_bounded() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("temp directory");
        let runtime = directory.path().join("hung-runtime");
        std::fs::write(&runtime, "#!/bin/sh\nsleep 30 &\nwait\n").expect("write fake runtime");
        let mut permissions = std::fs::metadata(&runtime).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&runtime, permissions).unwrap();
        let started = Instant::now();
        let error = run_bounded_command(
            runtime.to_str().unwrap(),
            &["info"],
            Duration::from_millis(50),
            1_024,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("exceeded"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
