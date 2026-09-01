#![allow(unsafe_code)]

use std::fs;
use std::future::Future;
use std::io::{BufReader, Read as _, Write as _};
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use reporch_runtime_core::RuntimeArtifactKindV1;
use reporch_runtime_hcs::{
    HYPERV_VSOCK_PORT, HcsCancellationHandle, HcsVirtualMachine, HcsVmConfigV1,
};
use reporch_runtime_protocol::{
    PROTOCOL_VERSION, ProtocolFailureV1, RuntimeServiceCommandV1, RuntimeServiceRequestV1,
    RuntimeServiceResponseV1, RuntimeServiceResultV1, SERVICE_RESPONSE_SCHEMA,
    read_service_request, write_service_response,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, RevertToSelf, SECURITY_ATTRIBUTES, SetFileSecurityW, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};
use windows::core::{BOOL, HSTRING, PWSTR};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const HCS_GUEST_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const HCS_GUEST_HANDSHAKE_ATTEMPTS: usize = 8;
const MAX_CONNECTIONS: usize = 16;
const MAX_RUNNING_JOBS: usize = 1;

struct HcsJobLifecycle<C = HcsCancellationHandle> {
    state: Mutex<HcsJobState<C>>,
}

enum HcsJobState<C> {
    Pending { cancel_requested: bool },
    Running { capability: C },
    Canceling,
    Finished,
}

impl<C: Clone> HcsJobLifecycle<C> {
    fn new() -> Self {
        Self {
            state: Mutex::new(HcsJobState::Pending {
                cancel_requested: false,
            }),
        }
    }

    fn cancellation_requested(&self) -> Result<bool> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("HCS lifecycle lock poisoned"))?;
        Ok(matches!(
            &*state,
            HcsJobState::Pending {
                cancel_requested: true
            } | HcsJobState::Canceling
        ))
    }

    /// Publish a creation-derived cancellation capability.
    ///
    /// Returns true when cancellation arrived while the VM was being created;
    /// the worker still owns the VM and must terminate it through that handle.
    fn publish_running(&self, capability: C) -> Result<bool> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("HCS lifecycle lock poisoned"))?;
        match &*state {
            HcsJobState::Pending {
                cancel_requested: true,
            } => {
                *state = HcsJobState::Canceling;
                Ok(true)
            }
            HcsJobState::Pending {
                cancel_requested: false,
            } => {
                *state = HcsJobState::Running { capability };
                Ok(false)
            }
            HcsJobState::Running { .. } | HcsJobState::Canceling | HcsJobState::Finished => {
                anyhow::bail!("HCS lifecycle was already published")
            }
        }
    }

    fn request_cancel_with<F>(&self, terminate: F) -> Result<bool>
    where
        F: FnOnce(&C) -> Result<()>,
    {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("HCS lifecycle lock poisoned"))?;
        match &*state {
            HcsJobState::Pending { .. } => {
                *state = HcsJobState::Pending {
                    cancel_requested: true,
                };
                Ok(false)
            }
            HcsJobState::Running { capability } => {
                let capability = capability.clone();
                *state = HcsJobState::Canceling;
                terminate(&capability)?;
                Ok(true)
            }
            HcsJobState::Canceling | HcsJobState::Finished => Ok(false),
        }
    }

    fn finish(&self) {
        if let Ok(mut state) = self.state.lock() {
            *state = HcsJobState::Finished;
        }
    }
}

impl HcsJobLifecycle<HcsCancellationHandle> {
    fn request_cancel(&self) -> Result<bool> {
        self.request_cancel_with(HcsCancellationHandle::terminate)
    }
}

struct HcsJobLifecycleGuard(Arc<HcsJobLifecycle>);

impl Drop for HcsJobLifecycleGuard {
    fn drop(&mut self) {
        self.0.finish();
    }
}

pub async fn run(mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
    let pipe_name = reporch_runtime_host::service_pipe_name()?;
    let allowed_sid = required_allowed_sid()?;
    prepare_service_spool(&allowed_sid)?;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let job_semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_RUNNING_JOBS));
    let mut first = true;
    loop {
        let descriptor =
            SecurityDescriptor::from_sddl(&format!("D:P(A;;GA;;;SY)(A;;GRGW;;;{allowed_sid})"))?;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())?,
            lpSecurityDescriptor: descriptor.0.0,
            bInheritHandle: BOOL(0),
        };
        let server = unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .access_inbound(true)
                .access_outbound(true)
                .create_with_security_attributes_raw(
                    &pipe_name,
                    (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
                )
        }
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
        let job_semaphore = job_semaphore.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = handle_connection(server, &allowed_sid, &job_semaphore).await;
        });
    }
    Ok(())
}

async fn handle_connection(
    mut stream: NamedPipeServer,
    allowed_sid: &str,
    job_semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
) -> Result<()> {
    ensure!(
        pipe_client_sid(&stream)? == allowed_sid,
        "runtime service client SID is not authorized"
    );
    let request = tokio::time::timeout(REQUEST_TIMEOUT, read_service_request(&mut stream))
        .await
        .context("runtime service request timed out")??;
    let result = match request.validate() {
        Ok(()) => {
            let lifecycle = matches!(&request.command, RuntimeServiceCommandV1::RunJob { .. })
                .then(|| Arc::new(HcsJobLifecycle::new()));
            let execution = execute_request(&request, job_semaphore, lifecycle.as_ref());
            let completed = if lifecycle.is_some() {
                await_job_or_client_disconnect(&mut stream, execution).await
            } else {
                Some(execution.await)
            };
            let Some(completed) = completed else {
                let lifecycle = lifecycle.context("missing HCS job lifecycle")?;
                let _ = tokio::time::timeout(
                    Duration::from_secs(6),
                    tokio::task::spawn_blocking(move || lifecycle.request_cancel()),
                )
                .await;
                return Ok(());
            };
            completed.unwrap_or_else(|error| {
                RuntimeServiceResultV1::Error(ProtocolFailureV1::bounded(
                    "runtime.guest_boot_failed",
                    format!("{error:#}"),
                ))
            })
        }
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

async fn await_job_or_client_disconnect<R, F, T>(stream: &mut R, operation: F) -> Option<T>
where
    R: AsyncRead + Unpin,
    F: Future<Output = T>,
{
    let mut unexpected = [0_u8; 1];
    tokio::pin!(operation);
    tokio::select! {
        result = &mut operation => Some(result),
        _ = stream.read(&mut unexpected) => None,
    }
}

async fn execute_request(
    request: &RuntimeServiceRequestV1,
    job_semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
    lifecycle: Option<&Arc<HcsJobLifecycle>>,
) -> Result<RuntimeServiceResultV1> {
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
            let lifecycle = lifecycle
                .context("HCS execution requires a connection-bound lifecycle")?
                .clone();
            ensure!(
                !lifecycle.cancellation_requested()?,
                "HCS job was canceled before execution"
            );
            let job_permit = job_semaphore
                .clone()
                .acquire_owned()
                .await
                .context("runtime service job limiter closed")?;
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
                // The blocking worker, not the client-facing async future,
                // owns the permit until its VM and job directories are gone.
                let _job_permit = job_permit;
                execute_hcs_job(&bundle, toolchain.as_ref(), &job, &lifecycle)
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
    lifecycle: &Arc<HcsJobLifecycle>,
) -> Result<reporch_runtime_protocol::GuestResultV1> {
    // Declared before the VM so normal unwinding closes the owned HCS handle
    // before publishing Finished to a racing disconnect watcher.
    let _lifecycle_guard = HcsJobLifecycleGuard(lifecycle.clone());
    job.validate()?;
    ensure!(
        !lifecycle.cancellation_requested()?,
        "HCS job was canceled before VM creation"
    );
    let spool = reporch_runtime_host::service_spool_root()?;
    let input_view = prepare_input_view(&spool, job)?;
    let _cleanup = JobDirectoryCleanup(input_view.clone());
    let kernel = bundle.artifact_path(RuntimeArtifactKindV1::Kernel)?;
    let initrd = bundle.artifact_path(RuntimeArtifactKindV1::Rootfs)?;
    let config = HcsVmConfigV1 {
        // HCS object identity is broker-private. The client job ID remains in
        // the guest protocol, but can never select a privileged host object.
        id: uuid::Uuid::now_v7(),
        kernel,
        initrd,
        toolchain_vhdx: toolchain.map(|toolchain| toolchain.path.clone()),
        memory_mib: job.limits.memory_mib.saturating_add(128).min(8_192),
        processor_count: job.limits.cpu_millis.div_ceil(1_000).clamp(1, 16),
        vsock_port: HYPERV_VSOCK_PORT,
    };
    let mut vm = HcsVirtualMachine::create(&config)?;
    let cancellation = vm.cancellation_handle()?;
    if lifecycle.publish_running(cancellation)? {
        let _ = vm.terminate();
        anyhow::bail!("HCS job was canceled during VM creation");
    }
    let io_timeout =
        Duration::from_millis(job.limits.timeout_ms).saturating_add(Duration::from_secs(5));
    let mut stream = None;
    let mut last_handshake_error = None;
    for attempt in 1..=HCS_GUEST_HANDSHAKE_ATTEMPTS {
        let mut candidate = vm.connect(HCS_GUEST_HANDSHAKE_TIMEOUT)?;
        match reporch_runtime_host::establish_guest_session_sync_challenged(
            &mut candidate,
            job,
            &bundle.installation.bundle_sha256,
        ) {
            Ok(()) => {
                candidate.set_io_timeout(io_timeout)?;
                stream = Some(candidate);
                break;
            }
            Err(error) if attempt < HCS_GUEST_HANDSHAKE_ATTEMPTS => {
                last_handshake_error = Some(format!("{error:#}"));
                drop(candidate);
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(error).context(format!(
                    "authenticate HCS guest after {HCS_GUEST_HANDSHAKE_ATTEMPTS} attempts"
                ));
            }
        }
    }
    let mut stream = stream.with_context(|| {
        format!(
            "authenticate HCS guest: {}",
            last_handshake_error.as_deref().unwrap_or("no handshake")
        )
    })?;
    let result = reporch_runtime_host::exchange_job_with_guest_sync(&mut stream, &input_view, job);
    drop(stream);
    let cleanup = vm.terminate();
    match (result, cleanup) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error).context("cleanup HCS virtual machine"),
    }
}

fn prepare_input_view(spool: &Path, job: &reporch_runtime_protocol::GuestJobV1) -> Result<PathBuf> {
    let jobs = reporch_runtime_host::runtime_root()?.join("jobs");
    fs::create_dir_all(&jobs).context("create runtime jobs directory")?;
    let root = jobs.join(job.id.simple().to_string());
    ensure!(!root.exists(), "runtime job input view already exists");
    fs::create_dir(&root).context("create runtime job input view")?;
    let object_root = jobs.join(format!(".objects-{}", job.id.simple()));
    ensure!(
        !object_root.exists(),
        "runtime job object cache already exists"
    );
    fs::create_dir(&object_root).context("create runtime job object cache")?;
    let object_cleanup = JobDirectoryCleanup(object_root.clone());
    let mut objects = std::collections::HashMap::new();
    for input in &job.inputs {
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
        let digest = input
            .sha256
            .strip_prefix("sha256:")
            .context("runtime input digest is invalid")?;
        let object_path = if let Some(path) = objects.get(digest) {
            path
        } else {
            let path = object_root.join(digest);
            copy_verified_spool_object(spool, input, &path)?;
            objects.insert(digest, path);
            objects.get(digest).context("cache runtime input object")?
        };
        fs::hard_link(object_path, &destination)
            .context("link verified runtime input into private view")?;
    }
    drop(object_cleanup);
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
    let mut verified = std::collections::HashSet::new();
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
        total = total
            .checked_add(object.size)
            .context("spool total overflow")?;
        if !verified.insert(digest) {
            continue;
        }
        let file = open_spool_object(spool, digest)?;
        let metadata = file.metadata().context("inspect runtime spool object")?;
        ensure!(
            metadata.is_file() && metadata.len() == object.size,
            "runtime spool object size or type changed"
        );
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
    }
    Ok((u32::try_from(objects.len())?, total))
}

fn open_spool_object(spool: &Path, digest: &str) -> Result<fs::File> {
    use cap_std::ambient_authority;
    use cap_std::fs::Dir;

    let directory = Dir::open_ambient_dir(spool, ambient_authority())
        .context("open capability-scoped runtime spool")?;
    let file = directory
        .open(Path::new(&digest[..2]).join(digest))
        .context("open capability-scoped runtime spool object")?;
    let metadata = file.metadata().context("inspect runtime spool object")?;
    ensure!(metadata.is_file(), "runtime spool object is not a file");
    Ok(file.into_std())
}

fn copy_verified_spool_object(
    spool: &Path,
    object: &reporch_runtime_protocol::ContentObjectV1,
    destination: &Path,
) -> Result<()> {
    let digest = object
        .sha256
        .strip_prefix("sha256:")
        .context("runtime input digest is invalid")?;
    let mut source = open_spool_object(spool, digest)?;
    ensure!(
        source.metadata()?.len() == object.size,
        "runtime spool object size changed"
    );
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .context("create private runtime job input")?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .context("read runtime spool object")?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("runtime spool size overflow")?;
        ensure!(total <= object.size, "runtime spool object grew");
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .context("write private runtime job input")?;
    }
    ensure!(total == object.size, "runtime spool object was truncated");
    ensure!(
        format!("sha256:{}", hex::encode(hasher.finalize())) == object.sha256,
        "runtime spool object digest changed"
    );
    output
        .sync_all()
        .context("sync private runtime job input")?;
    let mut permissions = output.metadata()?.permissions();
    permissions.set_readonly(true);
    output.set_permissions(permissions)?;
    Ok(())
}

fn prepare_service_spool(allowed_sid: &str) -> Result<()> {
    let spool = reporch_runtime_host::service_spool_root()?;
    fs::create_dir_all(&spool).context("create Windows runtime spool")?;
    let metadata = fs::symlink_metadata(&spool).context("inspect Windows runtime spool")?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Windows runtime spool must be a real directory"
    );
    let descriptor =
        SecurityDescriptor::from_sddl(&format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{allowed_sid})"))?;
    unsafe {
        SetFileSecurityW(
            &HSTRING::from(spool.as_os_str()),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor.0,
        )
    }
    .ok()
    .context("restrict Windows runtime spool ACL")?;
    Ok(())
}

fn required_allowed_sid() -> Result<String> {
    let value = std::env::var("REPORCH_RUNTIME_ALLOWED_SID")
        .context("REPORCH_RUNTIME_ALLOWED_SID is required for the Windows broker")?;
    validate_allowed_sid(&value)?;
    Ok(value)
}

fn validate_allowed_sid(value: &str) -> Result<()> {
    let remainder = value
        .strip_prefix("S-1-")
        .context("runtime allowed SID has an unsupported revision")?;
    ensure!(
        (5..=184).contains(&value.len())
            && !remainder.is_empty()
            && remainder.split('-').all(|component| !component.is_empty()
                && component.bytes().all(|byte| byte.is_ascii_digit())),
        "runtime allowed SID is invalid"
    );
    Ok(())
}

fn pipe_client_sid(stream: &NamedPipeServer) -> Result<String> {
    let handle = HANDLE(stream.as_raw_handle());
    unsafe { ImpersonateNamedPipeClient(handle) }.context("impersonate runtime pipe client")?;
    let _revert = ImpersonationGuard;
    let mut token = HANDLE::default();
    unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token) }
        .context("open impersonated runtime pipe client token")?;
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

struct ImpersonationGuard;

impl Drop for ImpersonationGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = RevertToSelf();
        }
    }
}

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> Result<Self> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .context("parse runtime security descriptor")?;
        ensure!(
            !descriptor.is_invalid(),
            "runtime security descriptor is invalid"
        );
        Ok(Self(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
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

#[cfg(test)]
mod cancellation_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::{HcsJobLifecycle, await_job_or_client_disconnect, validate_allowed_sid};

    #[test]
    fn installer_user_sid_is_accepted_without_relaxing_the_grammar() {
        validate_allowed_sid("S-1-5-21-1456194669-2875347699-3862154473-500").unwrap();
        for invalid in [
            "s-1-5-21-1",
            "S-2-5-21-1",
            "S-1-5--21-1",
            "S-1-5-21-user",
            "S-1-",
        ] {
            assert!(validate_allowed_sid(invalid).is_err(), "accepted {invalid}");
        }
    }

    struct DropSignal(Arc<AtomicBool>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn disconnect_drops_the_hcs_join_future() {
        let (mut service, client) = tokio::io::duplex(64);
        let dropped = Arc::new(AtomicBool::new(false));
        let signal = DropSignal(dropped.clone());
        let operation = async move {
            let _signal = signal;
            std::future::pending::<()>().await;
        };
        drop(client);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            await_job_or_client_disconnect(&mut service, operation),
        )
        .await
        .unwrap();
        assert!(result.is_none());
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn disconnect_before_creation_cannot_terminate_a_client_selected_system() {
        let lifecycle = HcsJobLifecycle::<u64>::new();
        let calls = AtomicUsize::new(0);
        let terminated = lifecycle
            .request_cancel_with(|_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .unwrap();

        assert!(!terminated);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(lifecycle.publish_running(41).unwrap());
    }

    #[test]
    fn running_capability_is_canceled_once_and_finished_is_terminal() {
        let lifecycle = HcsJobLifecycle::<u64>::new();
        assert!(!lifecycle.publish_running(73).unwrap());
        let calls = AtomicUsize::new(0);
        let terminate = |capability: &u64| {
            assert_eq!(*capability, 73);
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };

        assert!(lifecycle.request_cancel_with(terminate).unwrap());
        assert!(!lifecycle.request_cancel_with(|_| Ok(())).unwrap());
        lifecycle.finish();
        assert!(!lifecycle.request_cancel_with(|_| Ok(())).unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn normal_completion_prevents_late_cancellation() {
        let lifecycle = HcsJobLifecycle::<u64>::new();
        assert!(!lifecycle.publish_running(99).unwrap());
        lifecycle.finish();
        let calls = AtomicUsize::new(0);

        assert!(
            !lifecycle
                .request_cancel_with(|_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn detached_blocking_worker_retains_the_job_permit() {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let worker_release = release.clone();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            started_tx.send(()).unwrap();
            let (lock, wake) = &*worker_release;
            let mut ready = lock.lock().unwrap();
            while !*ready {
                ready = wake.wait(ready).unwrap();
            }
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(worker);

        assert!(semaphore.clone().try_acquire_owned().is_err());
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_one();
        let permit =
            tokio::time::timeout(Duration::from_secs(1), semaphore.clone().acquire_owned())
                .await
                .unwrap()
                .unwrap();
        drop(permit);
    }
}
