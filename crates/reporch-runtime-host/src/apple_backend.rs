//! Apple Virtualization.framework backend.
//!
//! All Objective-C interaction is confined to this module. VM mutations run
//! on one private serial dispatch queue, and only an owned duplicate of the
//! virtio-socket descriptor crosses back into Tokio.

#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::cell::Cell;
use std::os::fd::{BorrowedFd, IntoRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::AllocAnyThread as _;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSError, NSFileHandle, NSString, NSURL};
use objc2_virtualization::{
    VZBootLoader, VZDiskImageStorageDeviceAttachment, VZEntropyDeviceConfiguration,
    VZFileHandleSerialPortAttachment, VZLinuxBootLoader, VZSerialPortConfiguration,
    VZSocketDeviceConfiguration, VZStorageDeviceAttachment, VZStorageDeviceConfiguration,
    VZVirtioBlockDeviceConfiguration, VZVirtioConsoleDeviceSerialPortConfiguration,
    VZVirtioEntropyDeviceConfiguration, VZVirtioSocketConnection, VZVirtioSocketDevice,
    VZVirtioSocketDeviceConfiguration, VZVirtualMachine, VZVirtualMachineConfiguration,
};
use reporch_runtime_core::{RuntimeArtifactKindV1, RuntimeError};
use reporch_runtime_protocol::{GuestJobV1, GuestResultV1};

use crate::{VerifiedRuntimeBundleV1, VerifiedToolchainBundleV2, exchange_with_guest};

const GUEST_VSOCK_PORT: u32 = 7000;
const VM_START_TIMEOUT: Duration = Duration::from_secs(10);
const VSOCK_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const VM_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const GUEST_OVERHEAD_MIB: u64 = 96;

/// A retained Objective-C object whose methods are only called on `queue`.
///
/// Virtualization.framework binds a VM to the serial queue supplied at
/// construction. The retained object itself stays alive on the caller thread;
/// cloned retains wrapped here are moved onto that same queue and never used
/// anywhere else.
struct QueueBound<T: objc2::Message>(Retained<T>);

impl<T: objc2::Message> QueueBound<T> {
    fn get(&self) -> &T {
        &self.0
    }
}

// SAFETY: every QueueBound value in this module is consumed by the exact
// serial DispatchQueue associated with its VZVirtualMachine. No reference is
// accessed concurrently or on another queue.
unsafe impl<T: objc2::Message> Send for QueueBound<T> {}

pub(crate) fn execute(
    bundle: &VerifiedRuntimeBundleV1,
    toolchain: Option<&VerifiedToolchainBundleV2>,
    project_root: &Path,
    job: &GuestJobV1,
) -> Result<GuestResultV1> {
    job.validate()?;
    ensure!(
        matches!(
            bundle.manifest.target,
            reporch_runtime_core::HostTarget::DarwinArm64
                | reporch_runtime_core::HostTarget::DarwinX64
        ),
        "Apple runtime bundle target does not match this host"
    );
    let kernel = bundle.artifact_path(RuntimeArtifactKindV1::Kernel)?;
    let initramfs = bundle.artifact_path(RuntimeArtifactKindV1::Rootfs)?;
    let configuration = build_configuration(
        &kernel,
        &initramfs,
        &job.nonce,
        &bundle.installation.bundle_sha256,
        job,
        toolchain.map(|toolchain| toolchain.path.as_path()),
    )?;
    let queue = DispatchQueue::new("com.reporch.runtime.vm", None);
    // SAFETY: the validated configuration is retained for this call and the
    // private serial queue outlives the VM and all of its operations below.
    let vm = unsafe {
        VZVirtualMachine::initWithConfiguration_queue(
            VZVirtualMachine::alloc(),
            &configuration,
            &queue,
        )
    };

    start_vm(&queue, &vm)?;
    let execution = (|| -> Result<GuestResultV1> {
        let owned_fd = connect_vsock(&queue, &vm)?;
        let stream = StdUnixStream::from(owned_fd);
        stream
            .set_nonblocking(true)
            .context("make Apple virtio socket nonblocking")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .context("create Apple guest protocol runtime")?;
        runtime.block_on(async move {
            let mut stream = tokio::net::UnixStream::from_std(stream)
                .context("adopt Apple virtio socket in Tokio")?;
            exchange_with_guest(
                &mut stream,
                project_root,
                job,
                &bundle.installation.bundle_sha256,
            )
            .await
        })
    })();
    let stopped = stop_vm(&queue, &vm);
    match (execution, stopped) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(RuntimeError::CleanupFailed(error.to_string()).into()),
    }
}

fn build_configuration(
    kernel: &Path,
    initramfs: &Path,
    nonce: &str,
    bundle_digest: &str,
    job: &GuestJobV1,
    toolchain: Option<&Path>,
) -> Result<Retained<VZVirtualMachineConfiguration>> {
    ensure!(kernel.is_file(), "verified runtime kernel is missing");
    ensure!(initramfs.is_file(), "verified runtime initramfs is missing");
    let memory_mib = job
        .limits
        .memory_mib
        .checked_add(GUEST_OVERHEAD_MIB)
        .context("Apple VM memory size overflow")?;
    let requested_cpu = usize::try_from(u64::from(job.limits.cpu_millis).div_ceil(1_000))
        .context("Apple VM CPU count overflow")?
        .max(1);
    let debug_console = std::env::var_os("REPORCH_RUNTIME_DEBUG_CONSOLE").is_some();
    let quiet = if debug_console {
        ""
    } else {
        "quiet loglevel=0"
    };
    let command_line = format!(
        "console=hvc0 {quiet} panic=-1 rdinit=/sbin/reporch-guestd \
         reporch.nonce={nonce} reporch.bundle={bundle_digest} reporch.transport=vsock"
    );

    let configured = objc2::exception::catch(|| {
        // SAFETY: URLs point to verified regular files. Every subclass is
        // converted to its declared VZ superclass before being installed.
        unsafe {
            let configuration = VZVirtualMachineConfiguration::new();
            let boot =
                VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &file_url(kernel));
            boot.setInitialRamdiskURL(Some(&file_url(initramfs)));
            boot.setCommandLine(&NSString::from_str(&command_line));
            let boot: Retained<VZBootLoader> = Retained::into_super(boot);
            configuration.setBootLoader(Some(&boot));

            let minimum_memory = VZVirtualMachineConfiguration::minimumAllowedMemorySize();
            let maximum_memory = VZVirtualMachineConfiguration::maximumAllowedMemorySize();
            let memory_bytes = memory_mib
                .checked_mul(1_048_576)
                .ok_or_else(|| anyhow!("Apple VM memory size overflow"))?
                .max(minimum_memory);
            ensure!(
                memory_bytes <= maximum_memory,
                "requested Apple VM memory exceeds the host maximum"
            );
            configuration.setMemorySize(memory_bytes);

            let minimum_cpu = VZVirtualMachineConfiguration::minimumAllowedCPUCount();
            let maximum_cpu = VZVirtualMachineConfiguration::maximumAllowedCPUCount();
            ensure!(
                requested_cpu <= maximum_cpu,
                "requested Apple VM CPU count exceeds the host maximum"
            );
            configuration.setCPUCount(requested_cpu.max(minimum_cpu));

            let entropy: Retained<VZEntropyDeviceConfiguration> =
                Retained::into_super(VZVirtioEntropyDeviceConfiguration::new());
            let entropy_devices = NSArray::from_retained_slice(&[entropy]);
            configuration.setEntropyDevices(&entropy_devices);

            let socket: Retained<VZSocketDeviceConfiguration> =
                Retained::into_super(VZVirtioSocketDeviceConfiguration::new());
            let socket_devices = NSArray::from_retained_slice(&[socket]);
            configuration.setSocketDevices(&socket_devices);

            if let Some(toolchain) = toolchain {
                let attachment = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                    VZDiskImageStorageDeviceAttachment::alloc(),
                    &file_url(toolchain),
                    true,
                )
                .map_err(|error| anyhow!("open read-only toolchain image: {error:?}"))?;
                let attachment: Retained<VZStorageDeviceAttachment> = attachment.into_super();
                let storage = VZVirtioBlockDeviceConfiguration::initWithAttachment(
                    VZVirtioBlockDeviceConfiguration::alloc(),
                    &attachment,
                );
                storage.setBlockDeviceIdentifier(&NSString::from_str("reporch-toolchain"));
                let storage: Retained<VZStorageDeviceConfiguration> = Retained::into_super(storage);
                let storage_devices = NSArray::from_retained_slice(&[storage]);
                configuration.setStorageDevices(&storage_devices);
            }

            // VZ Linux guests need a concrete virtio console when `console=hvc0`
            // is present. Normal operation sends it to a real /dev/null file
            // descriptor; an explicit diagnostic opt-in sends it to stderr.
            let output = if debug_console {
                NSFileHandle::fileHandleWithStandardError()
            } else {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/null")
                    .context("open null sink for Apple VM console")?;
                NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                    NSFileHandle::alloc(),
                    file.into_raw_fd(),
                    true,
                )
            };
            let attachment =
                VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                    VZFileHandleSerialPortAttachment::alloc(),
                    None,
                    Some(&output),
                );
            let serial = VZVirtioConsoleDeviceSerialPortConfiguration::new();
            serial.setAttachment(Some(&attachment.into_super()));
            let serial: Retained<VZSerialPortConfiguration> = Retained::into_super(serial);
            let serial_ports = NSArray::from_retained_slice(&[serial]);
            configuration.setSerialPorts(&serial_ports);

            configuration
                .validateWithError()
                .map_err(|error| anyhow!("invalid Apple VM configuration: {error:?}"))?;
            Ok(configuration)
        }
    });
    match configured {
        Ok(result) => result,
        Err(exception) => Err(RuntimeError::GuestBootFailed(format!(
            "Virtualization.framework rejected the configuration: {exception:?}"
        ))
        .into()),
    }
}

fn file_url(path: &Path) -> Retained<NSURL> {
    // SAFETY: `fileURLWithPath:` copies the NSString and performs no access to
    // the file. Non-UTF-8 bytes are represented lossily only for the framework
    // URL; runtime bundle filenames are separately restricted to portable UTF-8.
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

fn start_vm(queue: &DispatchQueue, vm: &Retained<VZVirtualMachine>) -> Result<()> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let vm = QueueBound(vm.clone());
    queue.exec_async(move || {
        let sender = Cell::new(Some(sender));
        let callback = RcBlock::new(move |error: *mut NSError| {
            let result = callback_result(error, "start Apple VM");
            if let Some(sender) = sender.take() {
                let _ = sender.send(result);
            }
        });
        // SAFETY: executed on the VM's configured serial queue; the callback
        // remains retained by Virtualization.framework until completion.
        unsafe { vm.get().startWithCompletionHandler(&callback) };
    });
    receiver
        .recv_timeout(VM_START_TIMEOUT)
        .map_err(|_| RuntimeError::GuestBootFailed("Apple VM start timed out".into()))?
        .map_err(RuntimeError::GuestBootFailed)?;
    Ok(())
}

fn connect_vsock(queue: &DispatchQueue, vm: &Retained<VZVirtualMachine>) -> Result<OwnedFd> {
    let deadline = std::time::Instant::now() + VSOCK_CONNECT_TIMEOUT;
    loop {
        let (sender, receiver) = mpsc::sync_channel(1);
        let vm = QueueBound(vm.clone());
        queue.exec_async(move || {
            // SAFETY: executed on the VM's configured queue. This VM has
            // exactly one socket device, created as VZVirtioSocketDevice.
            let device = unsafe { vm.get().socketDevices().firstObject() };
            let Some(device) = device else {
                let _ = sender.send(Err("Apple VM has no virtio socket device".into()));
                return;
            };
            let Ok(device): Result<Retained<VZVirtioSocketDevice>, _> = device.downcast() else {
                let _ = sender.send(Err("Apple VM socket device has an unexpected type".into()));
                return;
            };
            let sender = Cell::new(Some(sender));
            let callback = RcBlock::new(
                move |connection: *mut VZVirtioSocketConnection, error: *mut NSError| {
                    let result = if !error.is_null() {
                        Err(ns_error_message(error, "connect Apple virtio socket"))
                    } else if connection.is_null() {
                        Err("Apple virtio socket returned no connection".into())
                    } else {
                        // SAFETY: the callback owns a valid connection for its
                        // duration. Duplicate its framework-owned descriptor so
                        // Rust owns an independent close-on-drop handle.
                        let raw = unsafe { (&*connection).fileDescriptor() };
                        if raw < 0 {
                            Err("Apple virtio socket returned a closed descriptor".into())
                        } else {
                            // SAFETY: `raw` is valid for this callback and is
                            // borrowed only for the duration of `dup`.
                            let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
                            rustix::io::dup(borrowed).map_err(|error| error.to_string())
                        }
                    };
                    if let Some(sender) = sender.take() {
                        let _ = sender.send(result);
                    }
                },
            );
            // SAFETY: executed on the VM's serial queue with a retained device.
            unsafe { device.connectToPort_completionHandler(GUEST_VSOCK_PORT, &callback) };
        });
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(RuntimeError::GuestUnresponsive.into());
        }
        match receiver.recv_timeout(remaining.min(Duration::from_secs(2))) {
            Ok(Ok(fd)) => return Ok(fd),
            Ok(Err(_)) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(Err(message)) => {
                return Err(RuntimeError::GuestBootFailed(message).into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) if std::time::Instant::now() < deadline => {}
            Err(_) => return Err(RuntimeError::GuestUnresponsive.into()),
        }
    }
}

fn stop_vm(queue: &DispatchQueue, vm: &Retained<VZVirtualMachine>) -> Result<()> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let vm = QueueBound(vm.clone());
    queue.exec_async(move || {
        let sender = Cell::new(Some(sender));
        let callback = RcBlock::new(move |error: *mut NSError| {
            let result = callback_result(error, "stop Apple VM");
            if let Some(sender) = sender.take() {
                let _ = sender.send(result);
            }
        });
        // SAFETY: executed on the VM's configured serial queue.
        unsafe { vm.get().stopWithCompletionHandler(&callback) };
    });
    receiver
        .recv_timeout(VM_STOP_TIMEOUT)
        .map_err(|_| anyhow!("Apple VM stop timed out"))?
        .map_err(anyhow::Error::msg)?;
    Ok(())
}

fn callback_result(error: *mut NSError, operation: &str) -> Result<(), String> {
    if error.is_null() {
        Ok(())
    } else {
        Err(ns_error_message(error, operation))
    }
}

fn ns_error_message(error: *mut NSError, operation: &str) -> String {
    // SAFETY: Virtualization.framework error pointers are valid and retained
    // for the duration of the completion callback where this is called.
    let description = unsafe { (&*error).localizedDescription().to_string() };
    format!("{operation}: {description}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, Utc};
    use reporch_runtime_core::{
        HostTarget, INSTALLATION_SCHEMA, RuntimeArtifactKindV1, RuntimeArtifactV1, RuntimeBackend,
        RuntimeBundleManifestV1, RuntimeInstallationV1,
    };
    use reporch_runtime_protocol::{
        GuestOperationV1, JOB_SCHEMA, PROTOCOL_VERSION, ResourceLimitsV1,
    };
    use std::collections::BTreeMap;
    use uuid::Uuid;

    #[test]
    #[ignore = "requires signed macOS test binary plus real arm64/x64 kernel and initramfs"]
    fn real_apple_vm_boots_handshakes_executes_and_stops() {
        let kernel = Path::new(&std::env::var("REPORCH_TEST_KERNEL").unwrap()).to_owned();
        let rootfs = Path::new(&std::env::var("REPORCH_TEST_INITRAMFS").unwrap()).to_owned();
        assert_eq!(kernel.parent(), rootfs.parent());
        let directory = kernel.parent().unwrap().to_owned();
        let target = HostTarget::current().unwrap();
        assert!(matches!(
            target,
            HostTarget::DarwinArm64 | HostTarget::DarwinX64
        ));
        let artifact = |kind, path: &Path| RuntimeArtifactV1 {
            kind,
            file_name: path.file_name().unwrap().to_str().unwrap().into(),
            sha256: format!("sha256:{}", "0".repeat(64)),
            size: path.metadata().unwrap().len(),
            source_url: "https://example.invalid/runtime".into(),
            sbom_url: "https://example.invalid/runtime.sbom".into(),
            provenance_url: "https://example.invalid/runtime.provenance".into(),
        };
        let digest = format!("sha256:{}", "1".repeat(64));
        let bundle = VerifiedRuntimeBundleV1 {
            installation: RuntimeInstallationV1 {
                schema: INSTALLATION_SCHEMA.into(),
                sequence: 1,
                version: "test".into(),
                target,
                bundle_sha256: digest.clone(),
                installed_at: Utc::now(),
            },
            manifest: RuntimeBundleManifestV1 {
                schema: reporch_runtime_core::BUNDLE_MANIFEST_SCHEMA.into(),
                sequence: 1,
                version: "test".into(),
                target,
                backend: RuntimeBackend::AppleVirtualization,
                minimum_os_version: "14.0".into(),
                protocol_min: PROTOCOL_VERSION,
                protocol_max: PROTOCOL_VERSION,
                generated_at: Utc::now(),
                expires_at: Utc::now() + ChronoDuration::hours(1),
                signing_key_id: reporch_runtime_core::RUNTIME_SIGNING_KEY_ID.into(),
                artifacts: vec![
                    artifact(RuntimeArtifactKindV1::Kernel, &kernel),
                    artifact(RuntimeArtifactKindV1::Rootfs, &rootfs),
                ],
            },
            directory,
        };
        let job = GuestJobV1 {
            schema: JOB_SCHEMA.into(),
            protocol_version: PROTOCOL_VERSION,
            id: Uuid::now_v7(),
            nonce: "mac-smoke-0123456789abcdef".into(),
            operation: GuestOperationV1::Program,
            toolchain_id: "runtime-self-test".into(),
            toolchain_index_sequence: None,
            toolchain_bundle_sha256: None,
            toolchain_lock_sha256: None,
            command: vec!["/sbin/reporch-guestd".into(), "--self-test-workload".into()],
            environment: BTreeMap::new(),
            inputs: Vec::new(),
            limits: ResourceLimitsV1 {
                timeout_ms: 5_000,
                memory_mib: 128,
                cpu_millis: 1_000,
                pids: 8,
                stdout_bytes: 4_096,
                stderr_bytes: 4_096,
                artifact_bytes: 4_096,
            },
        };
        let project = tempfile::tempdir().unwrap();
        let result = execute(&bundle, None, project.path(), &job).unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout.data, "reporch-runtime-self-test-ok\n");
    }
}
