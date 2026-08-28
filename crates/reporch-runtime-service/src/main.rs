#![deny(unsafe_code)]

#[cfg(target_os = "linux")]
mod linux_backend;

#[cfg(windows)]
mod windows_service;

#[cfg(unix)]
mod unix_service {
    use std::fs;
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
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::Semaphore;

    const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
    const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
    const SPOOL_VERIFY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
    const MAX_CONNECTIONS: usize = 16;

    pub async fn run() -> Result<()> {
        let socket = service_socket_path()?;
        let spool_override = spool_root_override()?;
        prepare_private_directory(socket.parent().context("service socket has no parent")?)?;
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
        let cleanup = SocketCleanup(socket.clone());
        let semaphore = std::sync::Arc::new(Semaphore::new(MAX_CONNECTIONS));
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
                    tokio::spawn(async move {
                        let _permit = permit;
                        let _ = handle_connection(stream, spool_override.as_deref()).await;
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
            Ok(()) => execute_request(&request, &spool, peer_uid)
                .await
                .unwrap_or_else(|error| {
                    RuntimeServiceResultV1::Error(ProtocolFailureV1::bounded(
                        "runtime.asset_verification_failed",
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

    async fn execute_request(
        request: &RuntimeServiceRequestV1,
        spool: &Path,
        peer_uid: u32,
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
                validate_peer_spool_directory(spool, peer_uid)?;
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
                validate_peer_spool_directory(spool, peer_uid)?;
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
            let path = spool.join(&digest[..2]).join(digest);
            ensure!(
                path.starts_with(spool),
                "spool object path escaped its root"
            );
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("inspect runtime spool object {digest}"))?;
            ensure!(
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.len() == object.size,
                "runtime spool object size or type changed"
            );
            let file = fs::File::open(&path).context("open runtime spool object")?;
            let after = file
                .metadata()
                .context("inspect opened runtime spool object")?;
            ensure!(
                after.dev() == metadata.dev()
                    && after.ino() == metadata.ino()
                    && after.len() == metadata.len(),
                "runtime spool object changed while being opened"
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
            total = total
                .checked_add(size)
                .context("spool total size overflow")?;
        }
        Ok((u32::try_from(objects.len())?, total))
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
    }
}

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
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
        let _ = run_service();
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
                    .block_on(crate::windows_service::run(shutdown_rx))
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

    fn status(state: ServiceState, accepts_stop: bool, exit_code: u32) -> ServiceStatus {
        ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: state,
            controls_accepted: if accepts_stop {
                ServiceControlAccept::STOP
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: ServiceExitCode::Win32(exit_code),
            checkpoint: 0,
            wait_hint: Duration::ZERO,
            process_id: None,
        }
    }
}
