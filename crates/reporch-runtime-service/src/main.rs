#![deny(unsafe_code)]

#[cfg(target_os = "linux")]
mod linux_backend;

#[cfg(windows)]
mod windows_service;

#[cfg(unix)]
mod unix_service {
    use std::fs;
    use std::future::Future;
    use std::io::{BufReader, Read as _};
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use anyhow::{Context, Result, ensure};
    use reporch_runtime_protocol::{
        PROTOCOL_VERSION, ProtocolFailureV1, RuntimeServiceCommandV1, RuntimeServiceRequestV1,
        RuntimeServiceResponseV1, RuntimeServiceResultV1, SERVICE_RESPONSE_SCHEMA,
        read_service_request, write_service_response,
    };
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt as _;
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::Semaphore;

    const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
    const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
    const SPOOL_VERIFY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
    const MAX_CONNECTIONS: usize = 16;
    const MAX_RUNNING_JOBS: usize = 1;

    pub async fn run() -> Result<()> {
        #[cfg(target_os = "linux")]
        bootstrap_system_runtime_seed().await?;
        let socket = service_socket_path()?;
        let spool_override = spool_root_override()?;
        prepare_service_directory(socket.parent().context("service socket has no parent")?)?;
        if let Some(spool) = &spool_override {
            prepare_private_directory(spool)?;
        }
        remove_stale_socket(&socket)?;
        let listener = UnixListener::bind(&socket)
            .with_context(|| format!("bind runtime service socket {}", socket.display()))?;
        let service_uid = rustix::process::getuid().as_raw();
        let socket_mode = if service_uid == 0 { 0o660 } else { 0o600 };
        fs::set_permissions(&socket, fs::Permissions::from_mode(socket_mode))
            .context("restrict runtime service socket")?;
        let socket_metadata = fs::symlink_metadata(&socket).context("inspect runtime socket")?;
        ensure!(
            socket_metadata.uid() == service_uid
                && socket_metadata.gid() == rustix::process::getgid().as_raw(),
            "runtime service socket owner or group is invalid"
        );
        let cleanup = SocketCleanup(socket.clone());
        let semaphore = std::sync::Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let job_semaphore = std::sync::Arc::new(Semaphore::new(MAX_RUNNING_JOBS));
        loop {
            tokio::select! {
                signal = tokio::signal::ctrl_c() => {
                    signal.context("wait for runtime service shutdown signal")?;
                    break;
                }
                accepted = listener.accept() => {
                    let (stream, _) = accepted.context("accept runtime service client")?;
                    let permit = semaphore.clone().acquire_owned().await
                        .context("runtime service connection limiter closed")?;
                    let spool_override = spool_override.clone();
                    let job_semaphore = job_semaphore.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let _ = handle_connection(stream, spool_override.as_deref(), &job_semaphore).await;
                    });
                }
            }
        }
        drop(cleanup);
        Ok(())
    }

    async fn handle_connection(
        mut stream: UnixStream,
        spool_override: Option<&Path>,
        job_semaphore: &std::sync::Arc<Semaphore>,
    ) -> Result<()> {
        let credential = stream
            .peer_cred()
            .context("read runtime service peer credential")?;
        let peer_uid = credential.uid();
        let service_uid = rustix::process::getuid().as_raw();
        if service_uid != 0 {
            ensure!(
                peer_uid == service_uid,
                "runtime service peer UID is not authorized"
            );
        }
        let spool = spool_override
            .map(Path::to_owned)
            .unwrap_or_else(|| peer_spool_root(peer_uid));
        let request = tokio::time::timeout(REQUEST_TIMEOUT, read_service_request(&mut stream))
            .await
            .context("runtime service request timed out")??;
        let result = match request.validate() {
            Ok(()) => {
                let execution = execute_request(&request, &spool, peer_uid, job_semaphore);
                let completed =
                    if matches!(&request.command, RuntimeServiceCommandV1::RunJob { .. }) {
                        await_job_or_client_disconnect(&mut stream, execution).await
                    } else {
                        Some(execution.await)
                    };
                let Some(completed) = completed else {
                    // Dropping the Linux execution future drops a kill-on-drop
                    // Firecracker child and its jail cleanup guard. A client
                    // that exits on SIGINT therefore cannot leave work running
                    // in the privileged broker.
                    return Ok(());
                };
                completed.unwrap_or_else(|error| {
                    RuntimeServiceResultV1::Error(ProtocolFailureV1::bounded(
                        "runtime.asset_verification_failed",
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

    async fn await_job_or_client_disconnect<F, T>(
        stream: &mut UnixStream,
        operation: F,
    ) -> Option<T>
    where
        F: Future<Output = T>,
    {
        let mut unexpected = [0_u8; 1];
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => Some(result),
            // A well-formed client sends exactly one request. EOF, a socket
            // error, or additional bytes all revoke this job's lifetime.
            _ = stream.read(&mut unexpected) => None,
        }
    }

    async fn execute_request(
        request: &RuntimeServiceRequestV1,
        spool: &Path,
        peer_uid: u32,
        job_semaphore: &std::sync::Arc<Semaphore>,
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
                validate_peer_spool_for_objects(spool, peer_uid, objects)?;
                if objects.is_empty() {
                    return Ok(RuntimeServiceResultV1::SpoolValid {
                        object_count: 0,
                        total_bytes: 0,
                    });
                }
                let objects = objects.clone();
                let spool = spool.to_owned();
                let (count, total) = tokio::time::timeout(
                    SPOOL_VERIFY_TIMEOUT,
                    tokio::task::spawn_blocking(move || verify_spool(&spool, &objects)),
                )
                .await
                .context("runtime spool verification timed out")?
                .context("join runtime spool verification")??;
                Ok(RuntimeServiceResultV1::SpoolValid {
                    object_count: count,
                    total_bytes: total,
                })
            }
            RuntimeServiceCommandV1::RunJob {
                job,
                runtime_sequence,
                runtime_bundle_digest,
            } => {
                let _job_permit = job_semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .context("runtime service job limiter closed")?;
                validate_peer_spool_for_objects(spool, peer_uid, &job.inputs)?;
                #[cfg(target_os = "linux")]
                {
                    ensure_system_runtime_root()?;
                    ensure!(
                        rustix::process::getuid().is_root(),
                        "local Firecracker execution requires the installed root broker"
                    );
                    let bundle = reporch_runtime_host::verified_bundle().await?;
                    ensure!(
                        bundle.installation.sequence == *runtime_sequence
                            && bundle.installation.bundle_sha256 == *runtime_bundle_digest,
                        "requested runtime bundle is not the installed verified bundle"
                    );
                    let toolchain = if job.toolchain_id == "runtime-self-test" {
                        None
                    } else {
                        let toolchain =
                            reporch_runtime_host::verified_toolchain(&job.toolchain_id).await?;
                        ensure!(
                            job.toolchain_index_sequence
                                == Some(toolchain.installation.index_sequence)
                                && job.toolchain_bundle_sha256.as_deref()
                                    == Some(toolchain.installation.bundle_sha256.as_str())
                                && job.toolchain_lock_sha256.as_deref()
                                    == Some(toolchain.installation.toolchain_lock_sha256.as_str()),
                            "requested toolchain identity is not the installed verified bundle"
                        );
                        Some(toolchain)
                    };
                    let result =
                        crate::linux_backend::execute(&bundle, toolchain.as_ref(), job, spool)
                            .await?;
                    Ok(RuntimeServiceResultV1::JobCompleted {
                        result: Box::new(result),
                    })
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = (job, runtime_sequence, runtime_bundle_digest);
                    anyhow::bail!("this Unix platform does not use the Firecracker broker")
                }
            }
        }
    }

    fn verify_spool(
        spool: &Path,
        objects: &[reporch_runtime_protocol::ContentObjectV1],
    ) -> Result<(u32, u64)> {
        let mut total = 0_u64;
        let mut verified = std::collections::HashSet::new();
        for object in objects {
            let digest = object
                .sha256
                .strip_prefix("sha256:")
                .context("spool object digest is invalid")?;
            ensure!(
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "spool object digest is invalid"
            );
            total = total
                .checked_add(object.size)
                .context("spool total size overflow")?;
            if !verified.insert(digest) {
                continue;
            }
            let file = open_spool_object(spool, digest)
                .with_context(|| format!("open runtime spool object {digest}"))?;
            let metadata = file
                .metadata()
                .with_context(|| format!("inspect runtime spool object {digest}"))?;
            ensure!(
                metadata.is_file() && metadata.len() == object.size,
                "runtime spool object size or type changed"
            );
            let mut reader = BufReader::with_capacity(64 * 1024, file);
            let mut hasher = Sha256::new();
            let mut size = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = reader
                    .read(&mut buffer)
                    .context("hash runtime spool object")?;
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

    #[cfg(test)]
    mod cancellation_tests {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        use tokio::net::UnixStream;

        use super::await_job_or_client_disconnect;

        struct DropSignal(Arc<AtomicBool>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        #[tokio::test]
        async fn disconnect_drops_the_in_flight_job_future() {
            let (mut service, client) = UnixStream::pair().unwrap();
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

        #[tokio::test]
        async fn completed_job_keeps_the_client_connection() {
            let (mut service, _client) = UnixStream::pair().unwrap();
            let result = await_job_or_client_disconnect(&mut service, async { 42_u8 }).await;
            assert_eq!(result, Some(42));
        }
    }

    fn open_spool_object(spool: &Path, digest: &str) -> Result<fs::File> {
        use rustix::fs::{Mode, OFlags, open, openat};

        ensure!(spool.is_absolute(), "runtime spool path must be absolute");
        let directory = open(
            spool,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("open runtime spool directory without following its final symlink")?;
        let prefix = openat(
            &directory,
            &digest[..2],
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("open runtime spool prefix without following symlinks")?;
        let file = openat(
            &prefix,
            digest,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .context("open runtime spool object without following symlinks")?;
        Ok(file.into())
    }

    fn service_socket_path() -> Result<PathBuf> {
        if let Some(value) = std::env::var_os("REPORCH_RUNTIME_SERVICE_SOCKET") {
            let value = PathBuf::from(value);
            ensure!(
                value.is_absolute(),
                "runtime service socket override must be absolute"
            );
            return Ok(value);
        }
        if rustix::process::getuid().is_root() {
            return Ok(PathBuf::from("/run/reporch-runtime/service-v1.sock"));
        }
        let root = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| {
                PathBuf::from("/run/user").join(rustix::process::getuid().as_raw().to_string())
            });
        Ok(root.join("reporch-runtime").join("service-v1.sock"))
    }

    #[cfg(target_os = "linux")]
    fn ensure_system_runtime_root() -> Result<()> {
        let expected = Path::new("/var/lib/reporch-runtime/runtime");
        ensure!(
            reporch_runtime_host::runtime_root()? == expected,
            "the privileged runtime broker must use the system-owned runtime root"
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    async fn bootstrap_system_runtime_seed() -> Result<()> {
        if !rustix::process::getuid().is_root() {
            return Ok(());
        }
        let executable = std::env::current_exe().context("resolve runtime service executable")?;
        let target = reporch_runtime_core::HostTarget::current()
            .context("runtime service target is unsupported")?;
        let seed = executable
            .parent()
            .context("runtime service executable has no parent")?
            .join("runtime")
            .join(reporch_runtime_host::target_name(target));
        if seed.join("current.json").is_file() {
            reporch_runtime_host::bootstrap_packaged_seed_to(
                &seed,
                Path::new("/var/lib/reporch-runtime/runtime"),
            )
            .await
            .context("import installer runtime seed")?;
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn ensure_system_runtime_root() -> Result<()> {
        anyhow::bail!("privileged runtime management is available only on Linux")
    }

    fn spool_root_override() -> Result<Option<PathBuf>> {
        if let Some(value) = std::env::var_os("REPORCH_RUNTIME_SPOOL_ROOT") {
            let value = PathBuf::from(value);
            ensure!(
                value.is_absolute(),
                "runtime spool override must be absolute"
            );
            return Ok(Some(value));
        }
        Ok(None)
    }

    fn peer_spool_root(peer_uid: u32) -> PathBuf {
        PathBuf::from("/run/user")
            .join(peer_uid.to_string())
            .join("reporch-runtime")
            .join("spool")
    }

    fn validate_peer_spool_directory(path: &Path, peer_uid: u32) -> Result<()> {
        let metadata = fs::symlink_metadata(path).context("inspect runtime peer spool")?;
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == peer_uid
                && metadata.permissions().mode() & 0o022 == 0,
            "runtime peer spool must be a private peer-owned directory"
        );
        Ok(())
    }

    fn validate_peer_spool_for_objects(
        path: &Path,
        peer_uid: u32,
        objects: &[reporch_runtime_protocol::ContentObjectV1],
    ) -> Result<()> {
        if objects.is_empty() {
            return Ok(());
        }
        validate_peer_spool_directory(path, peer_uid)
    }

    fn prepare_private_directory(path: &Path) -> Result<()> {
        fs::create_dir_all(path)
            .with_context(|| format!("create private runtime directory {}", path.display()))?;
        let metadata = fs::symlink_metadata(path).context("inspect private runtime directory")?;
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == rustix::process::getuid().as_raw(),
            "runtime directory must be a current-user non-symlink directory"
        );
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .context("restrict private runtime directory")?;
        Ok(())
    }

    fn prepare_service_directory(path: &Path) -> Result<()> {
        fs::create_dir_all(path)
            .with_context(|| format!("create runtime service directory {}", path.display()))?;
        let metadata = fs::symlink_metadata(path).context("inspect runtime service directory")?;
        let service_uid = rustix::process::getuid().as_raw();
        let service_gid = rustix::process::getgid().as_raw();
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == service_uid
                && metadata.gid() == service_gid,
            "runtime service directory must be owned by the service UID and GID"
        );
        let mode = if service_uid == 0 { 0o750 } else { 0o700 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .context("restrict runtime service directory")?;
        Ok(())
    }

    fn remove_stale_socket(path: &Path) -> Result<()> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_socket(),
                    "runtime service path is not a socket"
                );
                fs::remove_file(path).context("remove stale runtime service socket")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect runtime service socket"),
        }
        Ok(())
    }

    struct SocketCleanup(PathBuf);

    impl Drop for SocketCleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use reporch_runtime_protocol::ContentObjectV1;

        #[test]
        fn spool_validation_is_digest_bound_and_rejects_tampering() {
            let root = tempfile::tempdir().unwrap();
            let bytes = b"input\n";
            let digest = hex::encode(Sha256::digest(bytes));
            fs::create_dir(root.path().join(&digest[..2])).unwrap();
            fs::write(root.path().join(&digest[..2]).join(&digest), bytes).unwrap();
            let object = ContentObjectV1 {
                path: "tests/01.in".into(),
                sha256: format!("sha256:{digest}"),
                size: bytes.len() as u64,
            };
            assert_eq!(
                verify_spool(root.path(), std::slice::from_ref(&object)).unwrap(),
                (1, 6)
            );
            fs::write(root.path().join(&digest[..2]).join(&digest), b"changed").unwrap();
            assert!(verify_spool(root.path(), &[object]).is_err());
        }

        #[test]
        fn spool_validation_rejects_symlinks() {
            use std::os::unix::fs::symlink;

            let root = tempfile::tempdir().unwrap();
            let bytes = b"input\n";
            let digest = hex::encode(Sha256::digest(bytes));
            fs::create_dir(root.path().join(&digest[..2])).unwrap();
            fs::write(root.path().join("outside"), bytes).unwrap();
            symlink(
                root.path().join("outside"),
                root.path().join(&digest[..2]).join(&digest),
            )
            .unwrap();
            let object = ContentObjectV1 {
                path: "tests/01.in".into(),
                sha256: format!("sha256:{digest}"),
                size: bytes.len() as u64,
            };
            assert!(verify_spool(root.path(), &[object]).is_err());
        }

        #[test]
        fn empty_jobs_do_not_require_a_peer_spool_directory() {
            let missing = tempfile::tempdir().unwrap().path().join("missing-spool");
            validate_peer_spool_for_objects(&missing, 1234, &[]).unwrap();
        }
    }
}

#[cfg(unix)]
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    if let Err(error) = unix_service::run().await {
        eprintln!("reporch-runtime-service: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn main() {
    if let Err(error) = windows_entry::dispatch() {
        eprintln!("reporch-runtime-service: {error}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod windows_entry {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::time::Duration;

    use ::windows_service::define_windows_service;
    use ::windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use ::windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use ::windows_service::service_dispatcher;

    const SERVICE_NAME: &str = "ReporchRuntime";
    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

    define_windows_service!(ffi_service_main, service_main);

    pub fn dispatch() -> ::windows_service::Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
    }

    fn service_main(_arguments: Vec<OsString>) {
        clear_service_failure();
        if let Err(error) = run_service() {
            persist_service_failure(&error);
        }
    }

    fn run_service() -> ::windows_service::Result<()> {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let event_handler = move |event| match event {
            ServiceControl::Stop => {
                let _ = shutdown_tx.send(true);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };
        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
        status_handle.set_service_status(status(ServiceState::Running, true, 0))?;

        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(::windows_service::Error::Winapi)
            .and_then(|runtime| {
                runtime
                    .block_on(async {
                        bootstrap_system_runtime_seed().await?;
                        crate::windows_service::run(shutdown_rx).await
                    })
                    .map_err(|error| {
                        ::windows_service::Error::Winapi(std::io::Error::other(format!(
                            "{error:#}"
                        )))
                    })
            });
        let exit_code = u32::from(result.is_err());
        status_handle.set_service_status(status(ServiceState::Stopped, false, exit_code))?;
        result
    }

    fn service_failure_path() -> Option<PathBuf> {
        std::env::var_os("PROGRAMDATA").map(|root| {
            PathBuf::from(root)
                .join("Reporch")
                .join("Runtime")
                .join("last-service-error.txt")
        })
    }

    fn clear_service_failure() {
        if let Some(path) = service_failure_path() {
            let _ = std::fs::remove_file(path);
        }
    }

    fn persist_service_failure(error: &::windows_service::Error) {
        let Some(path) = service_failure_path() else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        let _ = std::fs::create_dir_all(parent);
        let mut message: String = format!("{error:#}").chars().take(8_192).collect();
        message.push('\n');
        let _ = std::fs::write(path, message);
    }

    async fn bootstrap_system_runtime_seed() -> anyhow::Result<()> {
        use anyhow::{Context as _, ensure};

        let program_data = std::env::var_os("PROGRAMDATA").context("PROGRAMDATA is required")?;
        let root = std::path::PathBuf::from(program_data)
            .join("Reporch")
            .join("Runtime");
        let executable = std::env::current_exe().context("resolve runtime service executable")?;
        let target = reporch_runtime_core::HostTarget::current()
            .context("runtime service target is unsupported")?;
        let seed = executable
            .parent()
            .context("runtime service executable has no parent")?
            .join("runtime")
            .join(reporch_runtime_host::target_name(target));
        ensure!(
            seed.is_absolute(),
            "runtime service seed path is not absolute"
        );
        if seed.join("current.json").is_file() {
            reporch_runtime_host::bootstrap_packaged_seed_to(&seed, &root)
                .await
                .context("import installer runtime seed")?;
        }
        Ok(())
    }

    fn status(state: ServiceState, accepts_stop: bool, exit_code: u32) -> ServiceStatus {
        ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: state,
            controls_accepted: if accepts_stop {
                ServiceControlAccept::STOP
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: if exit_code == 0 {
                ServiceExitCode::Win32(0)
            } else {
                ServiceExitCode::ServiceSpecific(exit_code)
            },
            checkpoint: 0,
            wait_hint: Duration::ZERO,
            process_id: None,
        }
    }
}
