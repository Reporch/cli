use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use reporch_runtime_core::{HostTarget, RuntimeArtifactKindV1, RuntimeBackend};
use reporch_runtime_host::{
    VerifiedRuntimeBundleV1, VerifiedToolchainBundleV2, exchange_with_guest,
};
use reporch_runtime_protocol::{GuestJobV1, GuestResultV1};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

const VSOCK_PORT: u32 = 7000;
const MAX_UNIX_SOCKET_PATH_BYTES: usize = 107;
const BOOT_TIMEOUT: Duration = Duration::from_secs(5);
const VSOCK_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const BOOT_LOG_LIMIT_BYTES: u64 = 16 * 1024;

#[derive(Debug)]
pub struct LinuxVmPlan {
    pub id: String,
    pub jailer: PathBuf,
    pub firecracker: PathBuf,
    pub chroot_base: PathBuf,
    pub jail_root: PathBuf,
    pub config_path: PathBuf,
    pub input_view: PathBuf,
    pub vsock_path: PathBuf,
    pub arguments: Vec<String>,
    config: FirecrackerConfig,
    vm_uid: u32,
    vm_gid: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
struct FirecrackerConfig {
    boot_source: BootSource,
    drives: Vec<Drive>,
    machine_config: MachineConfig,
    vsock: Vsock,
}

#[derive(Debug, Serialize)]
struct BootSource {
    kernel_image_path: &'static str,
    initrd_path: &'static str,
    boot_args: String,
}

#[derive(Debug, Serialize)]
struct Drive {
    drive_id: String,
    path_on_host: String,
    is_root_device: bool,
    is_read_only: bool,
}

#[derive(Debug, Serialize)]
struct MachineConfig {
    vcpu_count: u32,
    mem_size_mib: u64,
    smt: bool,
    track_dirty_pages: bool,
}

#[derive(Debug, Serialize)]
struct Vsock {
    guest_cid: u32,
    uds_path: &'static str,
}

pub fn build_plan(
    bundle: &VerifiedRuntimeBundleV1,
    toolchain: Option<&VerifiedToolchainBundleV2>,
    job: &GuestJobV1,
    peer_spool: &Path,
) -> Result<LinuxVmPlan> {
    ensure!(
        matches!(
            bundle.installation.target,
            HostTarget::LinuxArm64Gnu | HostTarget::LinuxX64Gnu
        ) && bundle.manifest.backend == RuntimeBackend::Firecracker,
        "verified runtime bundle is not a Linux Firecracker bundle"
    );
    job.validate()?;
    let vm_uid = required_non_root_id("REPORCH_RUNTIME_VM_UID")?;
    let vm_gid = required_non_root_id("REPORCH_RUNTIME_VM_GID")?;
    let chroot_base = std::env::var_os("REPORCH_RUNTIME_JAIL_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/reporch-runtime/jailer"));
    ensure!(
        chroot_base.is_absolute(),
        "runtime jail root must be absolute"
    );
    let firecracker = bundle.artifact_path(RuntimeArtifactKindV1::VirtualMachineMonitor)?;
    let jailer = bundle.artifact_path(RuntimeArtifactKindV1::Jailer)?;
    let firecracker_name = firecracker
        .file_name()
        .and_then(|value| value.to_str())
        .context("Firecracker artifact name is not UTF-8")?;
    let id = format!("rp-{}", job.id.simple());
    let jail_root = chroot_base.join(firecracker_name).join(&id).join("root");
    let config_path = jail_root.join("vm-config.json");
    let input_view = jail_root.join("input-view");
    let vsock_path = jail_root.join("run/v.sock");
    ensure!(
        vsock_path.as_os_str().as_bytes().len() <= MAX_UNIX_SOCKET_PATH_BYTES,
        "Firecracker vsock path exceeds the host Unix socket limit"
    );
    let memory_max = job
        .limits
        .memory_mib
        .saturating_add(128)
        .saturating_mul(1_048_576);
    let cpu_quota = u64::from(job.limits.cpu_millis)
        .saturating_mul(100_000)
        .div_ceil(1_000);
    let arguments = vec![
        "--id".into(),
        id.clone(),
        "--exec-file".into(),
        firecracker.to_string_lossy().into_owned(),
        "--uid".into(),
        vm_uid.to_string(),
        "--gid".into(),
        vm_gid.to_string(),
        "--chroot-base-dir".into(),
        chroot_base.to_string_lossy().into_owned(),
        "--new-pid-ns".into(),
        "--cgroup-version".into(),
        "2".into(),
        "--cgroup".into(),
        format!("pids.max={}", job.limits.pids.saturating_add(16)),
        "--cgroup".into(),
        format!("memory.max={memory_max}"),
        "--cgroup".into(),
        format!("cpu.max={cpu_quota} 100000"),
        "--resource-limit".into(),
        "no-file=256".into(),
        "--".into(),
        "--api-sock".into(),
        "/run/firecracker.socket".into(),
        "--config-file".into(),
        "/vm-config.json".into(),
    ];
    let guest_cid = u32::from_be_bytes(job.id.as_bytes()[..4].try_into()?) | 3;
    ensure!(
        (job.toolchain_id == "runtime-self-test") == toolchain.is_none(),
        "runtime self-test and toolchain attachment do not match"
    );
    let drives = toolchain
        .map(|_| {
            vec![Drive {
                drive_id: "reporch-toolchain".into(),
                path_on_host: "/toolchain.ext4".into(),
                is_root_device: false,
                is_read_only: true,
            }]
        })
        .unwrap_or_default();
    let config = FirecrackerConfig {
        boot_source: BootSource {
            kernel_image_path: "/vmlinux",
            initrd_path: "/rootfs.cpio",
            boot_args: format!(
                "console=ttyS0 reboot=k panic=1 pci=off ipv6.disable=1 rdinit=/sbin/reporch-guestd reporch.nonce={} reporch.bundle={} reporch.transport=vsock",
                job.nonce, bundle.installation.bundle_sha256
            ),
        },
        drives,
        machine_config: MachineConfig {
            vcpu_count: job.limits.cpu_millis.div_ceil(1_000).clamp(1, 16),
            mem_size_mib: job.limits.memory_mib.saturating_add(128),
            smt: false,
            track_dirty_pages: false,
        },
        vsock: Vsock {
            guest_cid,
            uds_path: "/run/v.sock",
        },
    };
    ensure!(
        peer_spool.is_absolute(),
        "runtime peer spool must be absolute"
    );
    Ok(LinuxVmPlan {
        id,
        jailer,
        firecracker,
        chroot_base,
        jail_root,
        config_path,
        input_view,
        vsock_path,
        arguments,
        config,
        vm_uid,
        vm_gid,
    })
}

pub async fn execute(
    bundle: &VerifiedRuntimeBundleV1,
    toolchain: Option<&VerifiedToolchainBundleV2>,
    job: &GuestJobV1,
    peer_spool: &Path,
) -> Result<GuestResultV1> {
    ensure!(
        rustix::process::getuid().is_root(),
        "Firecracker jailer execution requires the installed root broker"
    );
    let plan = build_plan(bundle, toolchain, job, peer_spool)?;
    let cleanup = JobCleanup(
        plan.chroot_base.clone(),
        plan.id.clone(),
        plan.firecracker.clone(),
    );
    prepare_plan(&plan, bundle, toolchain, job, peer_spool)?;
    let mut child = launch_jailer(&plan)?;
    let mut stream = match connect_guest(&plan.vsock_path).await {
        Ok(stream) => stream,
        Err(error) => {
            let early_status = child.try_wait().ok().flatten();
            kill_process_tree(&mut child).await;
            let diagnostics = boot_failure_diagnostics(&plan, early_status.as_ref());
            return Err(error).context(diagnostics);
        }
    };
    let result = exchange_with_guest(
        &mut stream,
        &plan.input_view,
        job,
        &bundle.installation.bundle_sha256,
    )
    .await;
    kill_process_tree(&mut child).await;
    drop(cleanup);
    result
}

fn prepare_plan(
    plan: &LinuxVmPlan,
    bundle: &VerifiedRuntimeBundleV1,
    toolchain: Option<&VerifiedToolchainBundleV2>,
    job: &GuestJobV1,
    peer_spool: &Path,
) -> Result<()> {
    ensure_root_owned_directory(&plan.chroot_base)?;
    ensure_staging_capacity(&plan.chroot_base, bundle, toolchain, job)?;
    ensure!(!plan.jail_root.exists(), "Firecracker jail already exists");
    fs::create_dir_all(plan.jail_root.join("run")).context("create Firecracker jail root")?;
    fs::create_dir(&plan.input_view).context("create Firecracker input view")?;
    let kernel = bundle.artifact_path(RuntimeArtifactKindV1::Kernel)?;
    let rootfs = bundle.artifact_path(RuntimeArtifactKindV1::Rootfs)?;
    let kernel_identity = bundle
        .manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.kind == RuntimeArtifactKindV1::Kernel)
        .context("verified runtime manifest has no kernel identity")?;
    let rootfs_identity = bundle
        .manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.kind == RuntimeArtifactKindV1::Rootfs)
        .context("verified runtime manifest has no rootfs identity")?;
    copy_verified_read_only_asset(
        &kernel,
        &plan.jail_root.join("vmlinux"),
        kernel_identity.size,
        &kernel_identity.sha256,
        plan.vm_gid,
    )?;
    copy_verified_read_only_asset(
        &rootfs,
        &plan.jail_root.join("rootfs.cpio"),
        rootfs_identity.size,
        &rootfs_identity.sha256,
        plan.vm_gid,
    )?;
    if let Some(toolchain) = toolchain {
        copy_verified_read_only_asset(
            &toolchain.path,
            &plan.jail_root.join("toolchain.ext4"),
            toolchain.bundle.size,
            &toolchain.bundle.sha256,
            plan.vm_gid,
        )?;
    }
    materialize_input_view(&plan.input_view, peer_spool, job)?;
    let config = serde_json::to_vec_pretty(&plan.config).context("serialize Firecracker config")?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&plan.config_path)
        .context("create Firecracker config")?;
    file.write_all(&config)
        .context("write Firecracker config")?;
    file.sync_all().context("sync Firecracker config")?;
    fs::set_permissions(&plan.config_path, fs::Permissions::from_mode(0o400))?;
    chown(&plan.config_path, plan.vm_uid, plan.vm_gid)?;
    for directory in [&plan.jail_root, &plan.jail_root.join("run")] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        chown(directory, plan.vm_uid, plan.vm_gid)?;
    }
    Ok(())
}

fn materialize_input_view(view: &Path, spool: &Path, job: &GuestJobV1) -> Result<()> {
    let object_root = view
        .parent()
        .context("runtime input view has no parent")?
        .join("input-objects");
    fs::create_dir(&object_root).context("create runtime input object cache")?;
    fs::set_permissions(&object_root, fs::Permissions::from_mode(0o700))?;
    let mut objects = std::collections::HashMap::new();
    for input in &job.inputs {
        let digest = input
            .sha256
            .strip_prefix("sha256:")
            .context("invalid input digest")?;
        let destination = view.join(&input.path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).context("create runtime input-view directory")?;
        }
        let object_path = if let Some(path) = objects.get(digest) {
            path
        } else {
            let path = object_root.join(digest);
            copy_verified_spool_object(spool, digest, input.size, &input.sha256, &path)?;
            objects.insert(digest, path);
            objects.get(digest).context("cache runtime input object")?
        };
        fs::hard_link(object_path, &destination)
            .context("link verified runtime input into private view")?;
    }
    Ok(())
}

fn copy_verified_spool_object(
    spool: &Path,
    digest: &str,
    expected_size: u64,
    expected_digest: &str,
    destination: &Path,
) -> Result<()> {
    let mut source = open_spool_object(spool, digest)?;
    let source_metadata = source
        .metadata()
        .context("inspect opened peer spool object")?;
    ensure!(
        source_metadata.is_file() && source_metadata.len() == expected_size,
        "peer spool object size or type changed"
    );
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .context("create root-owned runtime input object")?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = source.read(&mut buffer).context("read peer spool object")?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("input object size overflow")?;
        ensure!(total <= expected_size, "peer spool object grew");
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .context("write runtime input object")?;
    }
    ensure!(total == expected_size, "peer spool object is truncated");
    ensure!(
        format!("sha256:{}", hex::encode(hasher.finalize())) == expected_digest,
        "peer spool object digest changed"
    );
    output.sync_all().context("sync runtime input object")?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o400))?;
    Ok(())
}

fn ensure_staging_capacity(
    root: &Path,
    bundle: &VerifiedRuntimeBundleV1,
    toolchain: Option<&VerifiedToolchainBundleV2>,
    job: &GuestJobV1,
) -> Result<()> {
    const RESERVED_BYTES: u64 = 512 * 1_048_576;

    let required = required_staging_bytes(bundle, toolchain, job)?
        .checked_add(RESERVED_BYTES)
        .context("runtime staging reserve overflow")?;
    let status = rustix::fs::statvfs(root).context("inspect runtime staging filesystem")?;
    let available = status
        .f_bavail
        .checked_mul(status.f_frsize)
        .context("runtime available-space overflow")?;
    ensure!(
        available >= required,
        "runtime staging needs {required} bytes but only {available} bytes are available"
    );
    Ok(())
}

fn required_staging_bytes(
    bundle: &VerifiedRuntimeBundleV1,
    toolchain: Option<&VerifiedToolchainBundleV2>,
    job: &GuestJobV1,
) -> Result<u64> {
    let mut required = bundle
        .manifest
        .artifacts
        .iter()
        .try_fold(0_u64, |total, artifact| {
            total
                .checked_add(artifact.size)
                .context("runtime staging size overflow")
        })?;
    if let Some(toolchain) = toolchain {
        required = required
            .checked_add(toolchain.bundle.size)
            .context("runtime staging size overflow")?;
    }
    let mut digests = std::collections::HashSet::new();
    for input in &job.inputs {
        if digests.insert(input.sha256.as_str()) {
            required = required
                .checked_add(input.size)
                .context("runtime staging size overflow")?;
        }
    }
    Ok(required)
}

fn open_spool_object(spool: &Path, digest: &str) -> Result<fs::File> {
    use rustix::fs::{Mode, OFlags, openat};
    use std::path::Component;

    ensure!(spool.is_absolute(), "peer spool path must be absolute");
    let mut directory = fs::File::open("/").context("open filesystem root")?;
    for component in spool.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(component) => {
                let fd = openat(
                    &directory,
                    component,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .context("open peer spool directory without following symlinks")?;
                directory = fd.into();
            }
            _ => anyhow::bail!("peer spool path is not normalized"),
        }
    }
    let prefix = &digest[..2];
    let prefix = openat(
        &directory,
        prefix,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .context("open peer spool prefix without following symlinks")?;
    let file = openat(
        &prefix,
        digest,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .context("open peer spool object without following symlinks")?;
    Ok(file.into())
}

fn copy_verified_read_only_asset(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    expected_sha256: &str,
    vm_gid: u32,
) -> Result<()> {
    use rustix::fs::{Mode, OFlags, open};

    let source = open(
        source,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .context("open verified toolchain without following symlinks")?;
    let mut source = fs::File::from(source);
    ensure!(
        source.metadata()?.is_file() && source.metadata()?.len() == expected_size,
        "verified toolchain image changed before jail materialization"
    );
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .context("create private jail toolchain image")?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .context("read verified toolchain")?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("toolchain image size overflow")?;
        ensure!(total <= expected_size, "verified toolchain image grew");
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .context("copy toolchain into VM jail")?;
    }
    ensure!(
        total == expected_size,
        "verified toolchain image was truncated"
    );
    ensure!(
        format!("sha256:{}", hex::encode(hasher.finalize())) == expected_sha256,
        "verified toolchain image digest changed"
    );
    output.sync_all().context("sync jail toolchain image")?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o440))?;
    chown(destination, 0, vm_gid)
}

fn ensure_root_owned_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).context("create runtime jail base")?;
    let metadata = fs::symlink_metadata(path).context("inspect runtime jail base")?;
    use std::os::unix::fs::MetadataExt as _;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == 0
            && metadata.permissions().mode() & 0o022 == 0,
        "runtime jail base must be root-owned and not group/world writable"
    );
    Ok(())
}

fn launch_jailer(plan: &LinuxVmPlan) -> Result<Child> {
    let stdout = private_boot_log(&plan.jail_root.join("jailer.stdout.log"))?;
    let stderr = private_boot_log(&plan.jail_root.join("jailer.stderr.log"))?;
    let mut command = Command::new(&plan.jailer);
    command
        .args(&plan.arguments)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true)
        .process_group(0);
    command.spawn().context("launch Firecracker jailer")
}

fn private_boot_log(path: &Path) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create private runtime boot log {}", path.display()))?;
    Ok(file)
}

fn boot_failure_diagnostics(
    plan: &LinuxVmPlan,
    early_status: Option<&std::process::ExitStatus>,
) -> String {
    let status = early_status
        .map(ToString::to_string)
        .unwrap_or_else(|| "still running before forced cleanup".into());
    let stderr = bounded_boot_log(&plan.jail_root.join("jailer.stderr.log"));
    let stdout = bounded_boot_log(&plan.jail_root.join("jailer.stdout.log"));
    format!("Firecracker jailer status: {status}; stderr: {stderr}; stdout: {stdout}")
}

fn bounded_boot_log(path: &Path) -> String {
    let result = (|| -> Result<Vec<u8>> {
        let mut file = fs::File::open(path)?;
        let length = file.metadata()?.len();
        if length > BOOT_LOG_LIMIT_BYTES {
            file.seek(SeekFrom::Start(length - BOOT_LOG_LIMIT_BYTES))?;
        }
        let mut output = Vec::with_capacity(length.min(BOOT_LOG_LIMIT_BYTES) as usize);
        file.take(BOOT_LOG_LIMIT_BYTES).read_to_end(&mut output)?;
        Ok(output)
    })();
    match result {
        Ok(bytes) if bytes.is_empty() => "<empty>".into(),
        Ok(bytes) => String::from_utf8_lossy(&bytes)
            .chars()
            .map(|character| {
                if character == '\n'
                    || character == '\r'
                    || character == '\t'
                    || !character.is_control()
                {
                    character
                } else {
                    '\u{fffd}'
                }
            })
            .collect(),
        Err(error) => format!("<unavailable: {error}>"),
    }
}

async fn connect_guest(path: &Path) -> Result<UnixStream> {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    let mut stream = loop {
        match UnixStream::connect(path).await {
            Ok(stream) => break stream,
            Err(error) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let _ = error;
            }
            Err(error) => return Err(error).context("connect to Firecracker vsock backend"),
        }
    };
    stream
        .write_all(format!("CONNECT {VSOCK_PORT}\n").as_bytes())
        .await
        .context("request Firecracker guest vsock connection")?;
    let mut reader = BufReader::new(stream);
    let mut acknowledgement = String::new();
    tokio::time::timeout(VSOCK_ACK_TIMEOUT, reader.read_line(&mut acknowledgement))
        .await
        .context("Firecracker vsock acknowledgement timed out")??;
    ensure!(
        acknowledgement.starts_with("OK ")
            && acknowledgement.ends_with('\n')
            && acknowledgement.len() <= 64,
        "Firecracker rejected the guest vsock connection"
    );
    Ok(reader.into_inner())
}

async fn kill_process_tree(child: &mut Child) {
    if let Some(id) = child.id()
        && let Ok(raw) = i32::try_from(id)
        && let Some(pid) = rustix::process::Pid::from_raw(raw)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn chown(path: &Path, uid: u32, gid: u32) -> Result<()> {
    rustix::fs::chown(
        path,
        Some(rustix::process::Uid::from_raw(uid)),
        Some(rustix::process::Gid::from_raw(gid)),
    )
    .context("set runtime jail ownership")
}

fn required_non_root_id(name: &str) -> Result<u32> {
    let value = std::env::var(name)
        .with_context(|| format!("installed runtime service is missing {name}"))?
        .parse::<u32>()
        .with_context(|| format!("installed runtime service has invalid {name}"))?;
    ensure!(value != 0, "runtime VM UID/GID must not be root");
    Ok(value)
}

struct JobCleanup(PathBuf, String, PathBuf);

impl Drop for JobCleanup {
    fn drop(&mut self) {
        if let Some(name) = self.2.file_name() {
            let path = self.0.join(name).join(&self.1);
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_serialization_has_no_network_and_all_drives_are_read_only() {
        let config = FirecrackerConfig {
            boot_source: BootSource {
                kernel_image_path: "/vmlinux",
                initrd_path: "/rootfs.cpio",
                boot_args: "rdinit=/sbin/reporch-guestd".into(),
            },
            drives: Vec::new(),
            machine_config: MachineConfig {
                vcpu_count: 1,
                mem_size_mib: 64,
                smt: false,
                track_dirty_pages: false,
            },
            vsock: Vsock {
                guest_cid: 3,
                uds_path: "/run/v.sock",
            },
        };
        let value = serde_json::to_value(config).unwrap();
        assert!(value.get("network-interfaces").is_none());
        assert_eq!(value["drives"].as_array().unwrap().len(), 0);
        assert_eq!(value["boot-source"]["initrd_path"], "/rootfs.cpio");
        assert_eq!(value["machine-config"]["smt"], false);
    }

    #[test]
    fn installed_jail_vsock_path_fits_the_unix_socket_limit() {
        let id = format!("rp-{}", uuid::Uuid::nil().simple());
        let path = Path::new("/var/lib/reporch-runtime/jailer")
            .join("firecracker")
            .join(id)
            .join("root/run/v.sock");
        assert!(path.as_os_str().as_bytes().len() <= MAX_UNIX_SOCKET_PATH_BYTES);
    }

    #[test]
    fn boot_diagnostics_are_bounded_and_strip_control_characters() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("boot.log");
        let mut contents = vec![b'a'; BOOT_LOG_LIMIT_BYTES as usize + 32];
        contents.extend_from_slice(b"tail\0\x07\n");
        fs::write(&path, contents).unwrap();

        let diagnostic = bounded_boot_log(&path);

        assert!(diagnostic.len() <= BOOT_LOG_LIMIT_BYTES as usize + 8);
        assert!(!diagnostic.contains('\0'));
        assert!(!diagnostic.contains('\x07'));
        assert!(diagnostic.ends_with("tail��\n"));
    }
}
