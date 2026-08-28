#![allow(unsafe_code)]

use std::fs;
use std::io::{BufReader, Read as _};
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use reporch_runtime_core::RuntimeArtifactKindV1;
use reporch_runtime_hcs::{HYPERV_VSOCK_PORT, HcsVirtualMachine, HcsVmConfigV1};
use reporch_runtime_protocol::{
    PROTOCOL_VERSION, ProtocolFailureV1, RuntimeServiceCommandV1, RuntimeServiceRequestV1,
    RuntimeServiceResponseV1, RuntimeServiceResultV1, SERVICE_RESPONSE_SCHEMA,
    read_service_request, write_service_response,
};
use sha2::{Digest, Sha256};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::core::PWSTR;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 16;

pub async fn run(mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
    let pipe_name = reporch_runtime_host::service_pipe_name()?;
    let allowed_sid = required_allowed_sid()?;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let mut first = true;
    loop {
        let server = ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .access_inbound(true)
            .access_outbound(true)
            .create(&pipe_name)
            .with_context(|| format!("create runtime service pipe {pipe_name}"))?;
        first = false;
        tokio::select! {
            changed = shutdown.changed() => {
                changed.context("runtime service shutdown channel closed")?;
                break;
            }
            connected = server.connect() => connected.context("accept runtime service pipe client")?,
        }
        let permit = semaphore.clone().acquire_owned().await?;
        let allowed_sid = allowed_sid.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = handle_connection(server, &allowed_sid).await;
        });
    }
    Ok(())
}

async fn handle_connection(mut stream: NamedPipeServer, allowed_sid: &str) -> Result<()> {
    ensure!(
        pipe_client_sid(&stream)? == allowed_sid,
        "runtime service client SID is not authorized"
    );
    let request = tokio::time::timeout(REQUEST_TIMEOUT, read_service_request(&mut stream))
        .await
        .context("runtime service request timed out")??;
    let result = match request.validate() {
        Ok(()) => execute_request(&request).await.unwrap_or_else(|error| {
            RuntimeServiceResultV1::Error(ProtocolFailureV1::bounded(
                "runtime.guest_boot_failed",
                format!("{error:#}"),
            ))
        }),
        Err(error) => RuntimeServiceResultV1::Error(ProtocolFailureV1::bounded(
            "runtime.protocol_incompatible",
            error.to_string(),
        )),
    };
    let response = RuntimeServiceResponseV1 {
        schema: SERVICE_RESPONSE_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        request_id: request.id,
        result,
    };
    tokio::time::timeout(
        RESPONSE_TIMEOUT,
        write_service_response(&mut stream, &response),
    )
    .await
    .context("runtime service response timed out")??;
    Ok(())
}

async fn execute_request(request: &RuntimeServiceRequestV1) -> Result<RuntimeServiceResultV1> {
    match &request.command {
        RuntimeServiceCommandV1::Ping => Ok(RuntimeServiceResultV1::Pong {
            service_version: env!("CARGO_PKG_VERSION").into(),
        }),
        RuntimeServiceCommandV1::UpdateRuntime { force } => {
            ensure_system_runtime_root()?;
            let updated = reporch_runtime_host::update_direct_for_service(*force).await?;
            Ok(RuntimeServiceResultV1::RuntimeUpdated {
                previous_version: updated.previous_version,
                installed_version: updated.installed_version,
                sequence: updated.sequence,
                target: reporch_runtime_host::target_name(updated.target).into(),
                repaired: updated.repaired,
            })
        }
        RuntimeServiceCommandV1::InstallToolchain { id } => {
            ensure_system_runtime_root()?;
            let installed = reporch_runtime_host::install_toolchain_direct(id).await?;
            Ok(RuntimeServiceResultV1::ToolchainInstalled {
                id: installed.installation.id,
                index_sequence: installed.installation.index_sequence,
                bundle_sha256: installed.installation.bundle_sha256,
            })
        }
        RuntimeServiceCommandV1::ValidateSpool { objects } => {
            let spool = reporch_runtime_host::service_spool_root()?;
            let objects = objects.clone();
            let (object_count, total_bytes) =
                tokio::task::spawn_blocking(move || verify_spool(&spool, &objects)).await??;
            Ok(RuntimeServiceResultV1::SpoolValid {
                object_count,
                total_bytes,
            })
        }
        RuntimeServiceCommandV1::RunJob {
            job,
            runtime_sequence,
            runtime_bundle_digest,
        } => {
            ensure_system_runtime_root()?;
            let bundle = reporch_runtime_host::verified_bundle().await?;
            ensure!(
                bundle.installation.sequence == *runtime_sequence
                    && bundle.installation.bundle_sha256 == *runtime_bundle_digest,
                "requested runtime bundle is not the installed verified bundle"
            );
            let toolchain = if job.toolchain_id == "runtime-self-test" {
                None
            } else {
                let toolchain = reporch_runtime_host::verified_toolchain(&job.toolchain_id).await?;
                ensure!(
                    job.toolchain_index_sequence == Some(toolchain.installation.index_sequence)
                        && job.toolchain_bundle_sha256.as_deref()
                            == Some(toolchain.installation.bundle_sha256.as_str())
                        && job.toolchain_lock_sha256.as_deref()
                            == Some(toolchain.installation.toolchain_lock_sha256.as_str()),
                    "requested toolchain identity is not the installed verified bundle"
                );
                Some(toolchain)
            };
            let job = (**job).clone();
            let result = tokio::task::spawn_blocking(move || {
                execute_hcs_job(&bundle, toolchain.as_ref(), &job)
            })
            .await??;
            Ok(RuntimeServiceResultV1::JobCompleted {
                result: Box::new(result),
            })
        }
    }
}

fn ensure_system_runtime_root() -> Result<()> {
    let program_data = std::env::var_os("PROGRAMDATA").context("PROGRAMDATA is required")?;
    let expected = PathBuf::from(program_data).join("Reporch").join("Runtime");
    ensure!(
        reporch_runtime_host::runtime_root()? == expected,
        "the privileged runtime broker must use the system-owned runtime root"
    );
    Ok(())
}

fn execute_hcs_job(
    bundle: &reporch_runtime_host::VerifiedRuntimeBundleV1,
    toolchain: Option<&reporch_runtime_host::VerifiedToolchainBundleV2>,
    job: &reporch_runtime_protocol::GuestJobV1,
) -> Result<reporch_runtime_protocol::GuestResultV1> {
    job.validate()?;
    let spool = reporch_runtime_host::service_spool_root()?;
    let input_view = prepare_input_view(&spool, job)?;
    let _cleanup = JobDirectoryCleanup(input_view.clone());
    let rootfs = bundle.artifact_path(RuntimeArtifactKindV1::Rootfs)?;
    let config = HcsVmConfigV1 {
        id: job.id,
        rootfs_vhdx: rootfs,
        toolchain_vhdx: toolchain.map(|toolchain| toolchain.path.clone()),
        memory_mib: job.limits.memory_mib.saturating_add(128).min(8_192),
        processor_count: job.limits.cpu_millis.div_ceil(1_000).clamp(1, 16),
        vsock_port: HYPERV_VSOCK_PORT,
    };
    let mut vm = HcsVirtualMachine::create(&config)?;
    let io_timeout =
        Duration::from_millis(job.limits.timeout_ms).saturating_add(Duration::from_secs(5));
    let mut stream = vm.connect(io_timeout)?;
    let result = reporch_runtime_host::exchange_with_guest_sync_challenged(
        &mut stream,
        &input_view,
        job,
        &bundle.installation.bundle_sha256,
    );
    drop(stream);
    let cleanup = vm.terminate();
    match (result, cleanup) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error).context("cleanup HCS virtual machine"),
    }
}

fn prepare_input_view(spool: &Path, job: &reporch_runtime_protocol::GuestJobV1) -> Result<PathBuf> {
    verify_spool(spool, &job.inputs)?;
    let jobs = reporch_runtime_host::runtime_root()?.join("jobs");
    fs::create_dir_all(&jobs).context("create runtime jobs directory")?;
    let root = jobs.join(job.id.simple().to_string());
    ensure!(!root.exists(), "runtime job input view already exists");
    fs::create_dir(&root).context("create runtime job input view")?;
    for input in &job.inputs {
        let digest = input
            .sha256
            .strip_prefix("sha256:")
            .context("runtime input digest is invalid")?;
        let source = spool.join(&digest[..2]).join(digest);
        let destination = root.join(&input.path);
        ensure!(
            destination.starts_with(&root),
            "runtime input path escaped job view"
        );
        fs::create_dir_all(
            destination
                .parent()
                .context("runtime input has no parent")?,
        )?;
        fs::hard_link(&source, &destination).context("link runtime spool object into job view")?;
        let mut permissions = fs::metadata(&destination)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&destination, permissions)?;
    }
    Ok(root)
}

fn verify_spool(
    spool: &Path,
    objects: &[reporch_runtime_protocol::ContentObjectV1],
) -> Result<(u32, u64)> {
    let metadata = fs::symlink_metadata(spool).context("inspect runtime spool")?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "runtime spool must be a real directory"
    );
    let mut total = 0_u64;
    for object in objects {
        let digest = object
            .sha256
            .strip_prefix("sha256:")
            .context("runtime spool digest is invalid")?;
        ensure!(
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "runtime spool digest is invalid"
        );
        let path = spool.join(&digest[..2]).join(digest);
        let metadata = fs::symlink_metadata(&path).context("inspect runtime spool object")?;
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() == object.size,
            "runtime spool object size or type changed"
        );
        let file = fs::File::open(&path).context("open runtime spool object")?;
        let mut reader = BufReader::new(file);
        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            size = size
                .checked_add(read as u64)
                .context("spool size overflow")?;
            ensure!(size <= object.size, "runtime spool object grew");
            hasher.update(&buffer[..read]);
        }
        ensure!(size == object.size, "runtime spool object was truncated");
        ensure!(
            hex::encode(hasher.finalize()) == digest,
            "runtime spool object digest changed"
        );
        total = total.checked_add(size).context("spool total overflow")?;
    }
    Ok((u32::try_from(objects.len())?, total))
}

fn required_allowed_sid() -> Result<String> {
    let value = std::env::var("REPORCH_RUNTIME_ALLOWED_SID")
        .context("REPORCH_RUNTIME_ALLOWED_SID is required for the Windows broker")?;
    ensure!(
        (5..=184).contains(&value.len())
            && value.starts_with("S-1-")
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-'),
        "runtime allowed SID is invalid"
    );
    Ok(value)
}

fn pipe_client_sid(stream: &NamedPipeServer) -> Result<String> {
    let handle = HANDLE(stream.as_raw_handle());
    let mut process_id = 0_u32;
    unsafe { GetNamedPipeClientProcessId(handle, &mut process_id) }
        .context("get runtime pipe client process")?;
    ensure!(process_id != 0, "runtime pipe client process ID is invalid");
    let process = OwnedHandle(unsafe {
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id)
            .context("open runtime pipe client process")?
    });
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) }
        .context("open runtime pipe client token")?;
    let token = OwnedHandle(token);
    let mut required = 0_u32;
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut required) };
    ensure!(
        (std::mem::size_of::<TOKEN_USER>() as u32..=64 * 1024).contains(&required),
        "runtime pipe client token has an invalid size"
    );
    let words = required.div_ceil(std::mem::size_of::<usize>() as u32) as usize;
    let mut buffer = vec![0_usize; words];
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            required,
            &mut required,
        )
    }
    .context("read runtime pipe client token")?;
    let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = PWSTR::null();
    unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid) }
        .context("format runtime pipe client SID")?;
    let sid = LocalString(sid);
    Ok(unsafe { sid.0.to_string() }?)
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

struct LocalString(PWSTR);

impl Drop for LocalString {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0.cast())));
        }
    }
}

struct JobDirectoryCleanup(PathBuf);

impl Drop for JobDirectoryCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
