#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use reporch_runtime_protocol::{
    GuestHandshakeV1, GuestJobV1, GuestOutputV1, GuestResultV1, HANDSHAKE_SCHEMA, PROTOCOL_VERSION,
    ProtocolFailureV1, RESULT_SCHEMA, WireMessageV1, read_wire_message, write_wire_message,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const WORKSPACE: &str = "/workspace";
#[cfg(target_os = "linux")]
const GUEST_VSOCK_PORT: u32 = 7000;

fn main() {
    let mode = std::env::args_os().nth(1);
    if mode.as_deref() == Some(std::ffi::OsStr::new("--self-test-workload")) {
        println!("reporch-runtime-self-test-ok");
        return;
    }
    if mode.as_deref() == Some(std::ffi::OsStr::new("--internal-exec")) {
        if let Err(error) = internal_exec() {
            eprintln!("reporch-guestd internal exec: {error:#}");
            std::process::exit(126);
        }
        return;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("reporch-guestd: create async runtime: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = runtime.block_on(run()) {
        eprintln!("reporch-guestd: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
fn internal_exec() -> Result<()> {
    use rustix::process::{Resource, Rlimit, setrlimit};
    use rustix::thread::{set_thread_groups, set_thread_res_gid, set_thread_res_uid};
    use std::os::unix::process::CommandExt as _;

    let limit = |name: &str| -> Result<u64> {
        let value = std::env::var(name).with_context(|| format!("missing {name}"))?;
        value
            .parse::<u64>()
            .with_context(|| format!("invalid {name}"))
    };
    let bounded = |value| Rlimit {
        current: Some(value),
        maximum: Some(value),
    };
    setrlimit(
        Resource::As,
        bounded(limit("REPORCH_INTERNAL_MEMORY_BYTES")?),
    )
    .context("set workload address-space limit")?;
    setrlimit(
        Resource::Cpu,
        bounded(limit("REPORCH_INTERNAL_CPU_SECONDS")?),
    )
    .context("set workload CPU limit")?;
    setrlimit(Resource::Nproc, bounded(limit("REPORCH_INTERNAL_PIDS")?))
        .context("set workload process limit")?;
    setrlimit(
        Resource::Fsize,
        bounded(limit("REPORCH_INTERNAL_FILE_BYTES")?),
    )
    .context("set workload file-size limit")?;
    setrlimit(Resource::Core, bounded(0)).context("disable workload core dumps")?;
    setrlimit(Resource::Nofile, bounded(256)).context("set workload file-descriptor limit")?;

    if std::env::var("REPORCH_INTERNAL_USE_TOOLCHAIN").as_deref() == Ok("1") {
        rustix::process::chroot("/toolchain").context("enter read-only toolchain root")?;
        std::env::set_current_dir("/workspace").context("enter toolchain workspace")?;
    }
    let uid = rustix::process::Uid::from_raw(65_534);
    let gid = rustix::process::Gid::from_raw(65_534);
    set_thread_groups(&[]).context("clear workload supplementary groups")?;
    set_thread_res_gid(gid, gid, gid).context("drop workload group privileges")?;
    set_thread_res_uid(uid, uid, uid).context("drop workload user privileges")?;

    let arguments = std::env::args_os().skip(2).collect::<Vec<_>>();
    let (program, arguments) = arguments
        .split_first()
        .context("internal workload command is empty")?;
    let mut command = std::process::Command::new(program);
    command.args(arguments);
    for name in [
        "REPORCH_INTERNAL_MEMORY_BYTES",
        "REPORCH_INTERNAL_CPU_SECONDS",
        "REPORCH_INTERNAL_PIDS",
        "REPORCH_INTERNAL_FILE_BYTES",
        "REPORCH_INTERNAL_USE_TOOLCHAIN",
    ] {
        command.env_remove(name);
    }
    Err(command.exec()).context("exec limited guest workload")
}

#[cfg(not(target_os = "linux"))]
fn internal_exec() -> Result<()> {
    bail!("the guest workload launcher is Linux-only")
}

async fn run() -> Result<()> {
    #[cfg(target_os = "linux")]
    let is_pid_one = rustix::process::getpid().as_raw_nonzero().get() == 1;
    #[cfg(target_os = "linux")]
    let boot = if is_pid_one {
        Some(initialize_linux_guest()?)
    } else {
        None
    };
    #[cfg(not(target_os = "linux"))]
    let boot: Option<GuestBootV1> = None;
    let nonce = boot
        .as_ref()
        .map(|boot| boot.nonce.clone())
        .map_or_else(|| required_env("REPORCH_RUNTIME_NONCE"), Ok)?;
    let bundle_digest = boot
        .as_ref()
        .map(|boot| boot.bundle_digest.clone())
        .map_or_else(|| required_env("REPORCH_RUNTIME_BUNDLE_DIGEST"), Ok)?;
    #[cfg(target_os = "linux")]
    let transport =
        if boot.is_some() || std::env::var("REPORCH_RUNTIME_TRANSPORT").as_deref() == Ok("vsock") {
            "vsock"
        } else {
            "stdio"
        };
    #[cfg(target_os = "linux")]
    if transport == "vsock" {
        run_vsock(
            nonce,
            bundle_digest,
            boot.as_ref().is_some_and(|boot| boot.host_challenge),
        )
        .await?;
        if is_pid_one {
            // The host owns VM teardown. PID 1 must remain alive after the
            // one-shot result is flushed or Linux will panic and may reboot
            // before the host's stop request reaches the hypervisor.
            std::future::pending::<()>().await;
        }
        return Ok(());
    }
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    run_session(&mut stdin, &mut stdout, nonce, bundle_digest).await
}

#[derive(Debug)]
struct GuestBootV1 {
    nonce: String,
    bundle_digest: String,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    host_challenge: bool,
}

#[cfg(target_os = "linux")]
fn initialize_linux_guest() -> Result<GuestBootV1> {
    use rustix::mount::{MountFlags, mount};
    use rustix::process::{Gid, Uid};
    use std::os::unix::fs::PermissionsExt as _;

    mount(
        "devtmpfs",
        "/dev",
        "devtmpfs",
        MountFlags::NOSUID | MountFlags::NOEXEC,
        c"mode=0755",
    )
    .context("mount guest devtmpfs")?;
    mount(
        "proc",
        "/proc",
        "proc",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
        None,
    )
    .context("mount guest procfs")?;
    mount(
        "sysfs",
        "/sys",
        "sysfs",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC | MountFlags::RDONLY,
        None,
    )
    .context("mount guest sysfs")?;
    mount(
        "tmpfs",
        "/run/reporch",
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        c"mode=0755",
    )
    .context("mount guest execution tmpfs")?;
    mount(
        "tmpfs",
        "/workspace",
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        c"mode=0700",
    )
    .context("mount guest input tmpfs")?;
    mount(
        "tmpfs",
        "/tmp",
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
        c"mode=1777,size=67108864",
    )
    .context("mount guest temporary tmpfs")?;
    for path in ["/run/reporch/home", "/run/reporch/tmp"] {
        std::fs::create_dir(path).with_context(|| format!("create guest directory {path}"))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        rustix::fs::chown(
            path,
            Some(Uid::from_raw(65_534)),
            Some(Gid::from_raw(65_534)),
        )
        .with_context(|| format!("set guest directory ownership {path}"))?;
    }
    rustix::fs::chown(
        "/run/reporch",
        Some(Uid::from_raw(65_534)),
        Some(Gid::from_raw(65_534)),
    )
    .context("set guest execution directory ownership")?;
    make_workload_directory(Path::new("/workspace"))?;
    load_optional_vsock_modules()?;
    mount_optional_toolchain()?;
    read_kernel_boot_identity()
}

#[cfg(target_os = "linux")]
fn mount_optional_toolchain() -> Result<()> {
    use rustix::mount::{MountFlags, mount, mount_bind};
    use std::os::unix::fs::FileTypeExt as _;

    let device = ["/dev/vda", "/dev/sda1", "/dev/sda", "/dev/sdb1", "/dev/sdb"]
        .into_iter()
        .find(|path| {
            std::fs::symlink_metadata(path).is_ok_and(|metadata| {
                metadata.file_type().is_block_device() && !metadata.file_type().is_symlink()
            })
        });
    let Some(device) = device else {
        return Ok(());
    };
    mount(
        device,
        "/toolchain",
        "ext4",
        MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOATIME,
        c"noload",
    )
    .context("mount read-only toolchain image")?;
    for (source, target) in [
        ("/workspace", "/toolchain/workspace"),
        ("/run/reporch", "/toolchain/run/reporch"),
        ("/run/reporch/tmp", "/toolchain/tmp"),
        ("/proc", "/toolchain/proc"),
        ("/dev", "/toolchain/dev"),
    ] {
        let metadata = std::fs::symlink_metadata(target)
            .with_context(|| format!("inspect toolchain mount point {target}"))?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "toolchain image is missing safe mount point {target}"
        );
        mount_bind(source, target)
            .with_context(|| format!("bind guest resource into toolchain {target}"))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn load_optional_vsock_modules() -> Result<()> {
    const MODULE_ROOT: &str = "/lib/modules/reporch";
    const MODULES: [&str; 3] = [
        "vsock.ko",
        "vmw_vsock_virtio_transport_common.ko",
        "vmw_vsock_virtio_transport.ko",
    ];
    let root = Path::new(MODULE_ROOT);
    if !root.exists() {
        return Ok(());
    }
    ensure!(root.is_dir(), "guest kernel module root is not a directory");
    for name in MODULES {
        let path = root.join(name);
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("inspect guest kernel module {name}"))?;
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && (1..=32 * 1024 * 1024).contains(&metadata.len()),
            "invalid guest kernel module {name}"
        );
        let bytes =
            std::fs::read(&path).with_context(|| format!("read guest kernel module {name}"))?;
        match rustix::system::init_module(&bytes, c"") {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => {
                return Err(error).with_context(|| format!("load guest kernel module {name}"));
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_kernel_boot_identity() -> Result<GuestBootV1> {
    let command_line = std::fs::read_to_string("/proc/cmdline").context("read guest cmdline")?;
    parse_kernel_boot_identity(&command_line)
}

#[cfg(target_os = "linux")]
fn parse_kernel_boot_identity(command_line: &str) -> Result<GuestBootV1> {
    ensure!(
        command_line.len() <= 16 * 1024,
        "guest kernel command line is too large"
    );
    let unique = |name: &str| -> Result<String> {
        let prefix = format!("{name}=");
        let values = command_line
            .split_ascii_whitespace()
            .filter_map(|value| value.strip_prefix(&prefix))
            .collect::<Vec<_>>();
        ensure!(values.len() == 1, "guest cmdline must contain one {name}");
        let value = values[0];
        ensure!(
            !value.is_empty() && value.len() <= 256 && value.is_ascii(),
            "invalid guest cmdline value for {name}"
        );
        Ok(value.to_owned())
    };
    ensure!(
        unique("reporch.transport")? == "vsock",
        "unsupported guest transport"
    );
    let challenge_values = command_line
        .split_ascii_whitespace()
        .filter_map(|value| value.strip_prefix("reporch.host_challenge="))
        .collect::<Vec<_>>();
    ensure!(
        challenge_values.len() <= 1,
        "guest cmdline contains duplicate host challenge mode"
    );
    let host_challenge = challenge_values.first().is_some_and(|value| *value == "1");
    ensure!(
        challenge_values.is_empty() || host_challenge,
        "invalid guest host challenge mode"
    );
    let (nonce, bundle_digest) = if host_challenge {
        (
            "host-challenge-pending".into(),
            format!("sha256:{}", "0".repeat(64)),
        )
    } else {
        (unique("reporch.nonce")?, unique("reporch.bundle")?)
    };
    Ok(GuestBootV1 {
        nonce,
        bundle_digest,
        host_challenge,
    })
}

#[cfg(target_os = "linux")]
async fn run_vsock(nonce: String, bundle_digest: String, host_challenge: bool) -> Result<()> {
    use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};

    let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, GUEST_VSOCK_PORT))
        .context("bind guest vsock listener")?;
    let (stream, _) = listener
        .accept()
        .await
        .context("accept host vsock session")?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (nonce, bundle_digest) = if host_challenge {
        let challenge = match read_wire_message(&mut reader).await? {
            WireMessageV1::HostChallenge(challenge) => challenge,
            _ => bail!("guest expected a host challenge before its handshake"),
        };
        challenge.validate()?;
        (challenge.nonce, challenge.runtime_bundle_digest)
    } else {
        (nonce, bundle_digest)
    };
    run_session(&mut reader, &mut writer, nonce, bundle_digest).await
}

async fn run_session(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    nonce: String,
    bundle_digest: String,
) -> Result<()> {
    let handshake = GuestHandshakeV1 {
        schema: HANDSHAKE_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        guest_version: env!("CARGO_PKG_VERSION").into(),
        runtime_bundle_digest: bundle_digest,
        nonce: nonce.clone(),
    };
    handshake.validate(&nonce, &handshake.runtime_bundle_digest)?;

    write_wire_message(writer, &WireMessageV1::Handshake(handshake)).await?;
    let job = match read_wire_message(reader).await? {
        WireMessageV1::Job(job) => job,
        _ => bail!("guest expected a job after its handshake"),
    };
    job.validate()?;
    ensure!(
        job.nonce == nonce,
        "guest job nonce does not match this VM session"
    );
    let session = async {
        receive_inputs(Path::new(WORKSPACE), &job, reader).await?;
        verify_inputs(Path::new(WORKSPACE), &job).await?;
        let result = execute_job(&job).await?;
        result.validate_for(&job)?;
        Result::<GuestResultV1>::Ok(result)
    }
    .await;
    match session {
        Ok(result) => write_wire_message(writer, &WireMessageV1::Result(result)).await?,
        Err(error) => {
            let failure = ProtocolFailureV1::bounded("runtime.guest_failure", format!("{error:#}"));
            let _ = write_wire_message(writer, &WireMessageV1::ProtocolError(failure)).await;
            return Err(error);
        }
    }
    Ok(())
}

async fn receive_inputs(
    root: &Path,
    job: &GuestJobV1,
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<()> {
    prepare_workspace(root)?;
    for (index, input) in job.inputs.iter().enumerate() {
        let index = u32::try_from(index).context("too many guest input objects")?;
        let path = root.join(&input.path);
        create_safe_parent_directories(root, Path::new(&input.path))?;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .with_context(|| format!("create guest input {}", input.path))?;
        let mut hasher = Sha256::new();
        let mut written = 0_u64;
        loop {
            let chunk = match read_wire_message(reader).await? {
                WireMessageV1::InputChunk(chunk) => chunk,
                _ => bail!("guest expected an input chunk"),
            };
            chunk.validate()?;
            ensure!(
                chunk.object_index == index && chunk.offset == written,
                "guest input chunks are out of order"
            );
            let next = written
                .checked_add(chunk.bytes.len() as u64)
                .context("guest input size overflow")?;
            ensure!(next <= input.size, "guest input exceeds its declared size");
            if !chunk.bytes.is_empty() {
                file.write_all(&chunk.bytes)
                    .await
                    .context("write guest input chunk")?;
                hasher.update(&chunk.bytes);
            }
            written = next;
            if chunk.eof {
                break;
            }
        }
        ensure!(written == input.size, "guest input is truncated");
        let digest = format!("sha256:{}", hex::encode(hasher.finalize()));
        ensure!(digest == input.sha256, "guest input SHA-256 mismatch");
        file.sync_all().await.context("sync guest input")?;
        drop(file);
        set_input_read_only(&path)?;
    }
    Ok(())
}

fn prepare_workspace(root: &Path) -> Result<()> {
    if !root.exists() {
        std::fs::create_dir_all(root).context("create guest workspace")?;
    }
    let metadata = std::fs::symlink_metadata(root).context("inspect guest workspace")?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "guest workspace must be a non-symlink directory"
    );
    ensure!(
        std::fs::read_dir(root)
            .context("list guest workspace")?
            .next()
            .is_none(),
        "guest workspace is not disposable and empty"
    );
    Ok(())
}

fn create_safe_parent_directories(root: &Path, relative: &Path) -> Result<()> {
    let mut cursor = root.to_owned();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            cursor.push(component);
            match std::fs::create_dir(&cursor) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).context("create guest input directory"),
            }
            let metadata = std::fs::symlink_metadata(&cursor)
                .with_context(|| format!("inspect guest input directory {}", cursor.display()))?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "guest input path contains a symlink or non-directory"
            );
            make_workload_directory(&cursor)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn make_workload_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restrict guest workload directory {}", path.display()))?;
    #[cfg(target_os = "linux")]
    if rustix::process::getuid().is_root() {
        rustix::fs::chown(
            path,
            Some(rustix::process::Uid::from_raw(65_534)),
            Some(rustix::process::Gid::from_raw(65_534)),
        )
        .with_context(|| format!("assign guest workload directory {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_workload_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn set_input_read_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444))
            .context("make guest input read-only")?;
    }
    #[cfg(not(unix))]
    {
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

async fn execute_job(job: &GuestJobV1) -> Result<GuestResultV1> {
    let started = Instant::now();
    let memory_bytes = job
        .limits
        .memory_mib
        .checked_mul(1_048_576)
        .context("guest memory limit overflow")?;
    let cpu_seconds = u64::from(job.limits.cpu_millis).div_ceil(1_000).max(1);
    let mut child = Command::new("/sbin/reporch-guestd");
    child
        .arg("--internal-exec")
        .args(&job.command)
        .current_dir(WORKSPACE)
        .env_clear()
        .envs(&job.environment)
        .env("HOME", "/run/reporch/home")
        .env("TMPDIR", "/run/reporch/tmp")
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .env("REPORCH_INTERNAL_MEMORY_BYTES", memory_bytes.to_string())
        .env("REPORCH_INTERNAL_CPU_SECONDS", cpu_seconds.to_string())
        .env("REPORCH_INTERNAL_PIDS", job.limits.pids.to_string())
        .env(
            "REPORCH_INTERNAL_FILE_BYTES",
            job.limits.artifact_bytes.to_string(),
        )
        .env(
            "REPORCH_INTERNAL_USE_TOOLCHAIN",
            if job.toolchain_id == "runtime-self-test" {
                "0"
            } else {
                "1"
            },
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        child.process_group(0);
    }
    let mut child = child.spawn().context("start guest workload")?;
    let stdout = child.stdout.take().context("capture guest stdout")?;
    let stderr = child.stderr.take().context("capture guest stderr")?;
    let stdout_task = tokio::spawn(read_bounded(stdout, job.limits.stdout_bytes));
    let stderr_task = tokio::spawn(read_bounded(stderr, job.limits.stderr_bytes));
    let status = match tokio::time::timeout(
        std::time::Duration::from_millis(job.limits.timeout_ms),
        child.wait(),
    )
    .await
    {
        Ok(status) => status.context("wait for guest workload")?,
        Err(_) => {
            kill_process_tree(&mut child).await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            bail!("guest workload exceeded {} ms", job.limits.timeout_ms);
        }
    };
    let (stdout, stdout_truncated) = stdout_task.await.context("join guest stdout")??;
    let (stderr, stderr_truncated) = stderr_task.await.context("join guest stderr")??;
    Ok(GuestResultV1 {
        schema: RESULT_SCHEMA.into(),
        protocol_version: PROTOCOL_VERSION,
        job_id: job.id,
        nonce: job.nonce.clone(),
        exit_code: status.code().unwrap_or(128),
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        stdout: GuestOutputV1::from_bytes(&stdout, stdout_truncated),
        stderr: GuestOutputV1::from_bytes(&stderr, stderr_truncated),
        artifacts: Vec::new(),
    })
}

async fn kill_process_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(id) = child.id()
        && let Ok(raw) = i32::try_from(id)
        && let Some(pid) = rustix::process::Pid::from_raw(raw)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill().await;
}

async fn verify_inputs(root: &Path, job: &GuestJobV1) -> Result<()> {
    let root = root.canonicalize().context("resolve guest workspace")?;
    for input in &job.inputs {
        let path = root.join(&input.path);
        ensure_no_symlink_ancestors(&root, Path::new(&input.path))?;
        let metadata = tokio::fs::symlink_metadata(&path)
            .await
            .with_context(|| format!("inspect guest input {}", input.path))?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "guest input must be a regular non-symlink file"
        );
        ensure!(
            metadata.len() == input.size,
            "guest input size does not match signed job"
        );
        let canonical = path.canonicalize().context("resolve guest input")?;
        ensure!(
            canonical.starts_with(&root),
            "guest input escaped the workspace"
        );
        let mut file = tokio::fs::File::open(&canonical)
            .await
            .context("open guest input")?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).await.context("hash guest input")?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let digest = format!("sha256:{}", hex::encode(hasher.finalize()));
        ensure!(
            digest == input.sha256,
            "guest input SHA-256 does not match signed job"
        );
    }
    Ok(())
}

fn ensure_no_symlink_ancestors(root: &Path, relative: &Path) -> Result<()> {
    let mut cursor = PathBuf::from(root);
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            cursor.push(component);
            let metadata = std::fs::symlink_metadata(&cursor)
                .with_context(|| format!("inspect guest path component {}", cursor.display()))?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "guest input contains a symlink or non-directory ancestor"
            );
        }
    }
    Ok(())
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin, limit: u64) -> Result<(Vec<u8>, bool)> {
    let capacity = usize::try_from(limit.min(64 * 1024)).unwrap_or(64 * 1024);
    let mut output = Vec::with_capacity(capacity);
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .context("read guest output")?;
        if read == 0 {
            break;
        }
        let retained = u64::try_from(output.len()).unwrap_or(u64::MAX);
        let remaining = limit.saturating_sub(retained) as usize;
        output.extend_from_slice(&buffer[..read.min(remaining)]);
        truncated |= read > remaining;
    }
    Ok((output, truncated))
}

fn required_env(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("missing required {name}"))?;
    ensure!(!value.is_empty() && value.len() <= 256, "invalid {name}");
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reporch_runtime_protocol::{
        ContentObjectV1, GuestOperationV1, InputChunkV1, JOB_SCHEMA, ResourceLimitsV1,
    };
    use serde_bytes::ByteBuf;
    use std::collections::BTreeMap;
    use uuid::Uuid;

    #[tokio::test]
    async fn streamed_inputs_are_ordered_hashed_and_read_only() {
        let root = tempfile::tempdir().unwrap();
        let contents = b"print('hello')\n";
        let job = GuestJobV1 {
            schema: JOB_SCHEMA.into(),
            protocol_version: PROTOCOL_VERSION,
            id: Uuid::now_v7(),
            nonce: "nonce-0123456789abcdef".into(),
            operation: GuestOperationV1::Program,
            toolchain_id: "python-3.14".into(),
            toolchain_index_sequence: Some(1),
            toolchain_bundle_sha256: Some(format!("sha256:{}", "b".repeat(64))),
            toolchain_lock_sha256: Some(format!("sha256:{}", "c".repeat(64))),
            command: vec!["python3".into(), "src/main.py".into()],
            environment: BTreeMap::new(),
            inputs: vec![ContentObjectV1 {
                path: "src/main.py".into(),
                sha256: format!("sha256:{}", hex::encode(Sha256::digest(contents))),
                size: contents.len() as u64,
            }],
            limits: ResourceLimitsV1 {
                timeout_ms: 1_000,
                memory_mib: 64,
                cpu_millis: 1_000,
                pids: 16,
                stdout_bytes: 1_024,
                stderr_bytes: 1_024,
                artifact_bytes: 1_024,
            },
        };
        let mut wire = Vec::new();
        write_wire_message(
            &mut wire,
            &WireMessageV1::InputChunk(InputChunkV1 {
                object_index: 0,
                offset: 0,
                bytes: ByteBuf::from(contents.to_vec()),
                eof: true,
            }),
        )
        .await
        .unwrap();
        receive_inputs(root.path(), &job, &mut wire.as_slice())
            .await
            .unwrap();
        verify_inputs(root.path(), &job).await.unwrap();
        assert_eq!(
            std::fs::read(root.path().join("src/main.py")).unwrap(),
            contents
        );
        assert!(
            std::fs::metadata(root.path().join("src/main.py"))
                .unwrap()
                .permissions()
                .readonly()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_boot_identity_is_unique_bounded_and_vsock_only() {
        let parsed = parse_kernel_boot_identity(
            "console=hvc0 reporch.nonce=nonce-0123456789abcdef \
             reporch.bundle=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
             reporch.transport=vsock",
        )
        .unwrap();
        assert_eq!(parsed.nonce, "nonce-0123456789abcdef");
        assert!(!parsed.host_challenge);
        let challenged =
            parse_kernel_boot_identity("reporch.transport=vsock reporch.host_challenge=1").unwrap();
        assert!(challenged.host_challenge);
        assert!(parse_kernel_boot_identity("reporch.transport=vsock").is_err());
        assert!(
            parse_kernel_boot_identity(
                "reporch.nonce=a reporch.nonce=b reporch.bundle=c reporch.transport=vsock"
            )
            .is_err()
        );
        assert!(
            parse_kernel_boot_identity("reporch.nonce=a reporch.bundle=c reporch.transport=serial")
                .is_err()
        );
    }
}
