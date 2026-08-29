#![deny(unsafe_code)]

use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use serde::Serialize;
use uuid::Uuid;

#[cfg(windows)]
mod windows_backend;
#[cfg(windows)]
pub use windows_backend::{HcsVirtualMachine, HvSocketStream, terminate_compute_system};

pub const HYPERV_VSOCK_PORT: u32 = 7_000;
const HCS_SCSI_CONTROLLER_0: &str = "df6d0690-79e5-55b6-a5ec-c1e2f77f580a";
const HCS_KERNEL_COMMAND_LINE: &str = "8250_core.nr_uarts=0 panic=-1 quiet pci=off rdinit=/sbin/reporch-guestd reporch.host_challenge=1 reporch.transport=vsock initcall_blacklist=virtio_vsock_init";

#[derive(Clone, Debug)]
pub struct HcsVmConfigV1 {
    pub id: Uuid,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub toolchain_vhdx: Option<PathBuf>,
    pub memory_mib: u64,
    pub processor_count: u32,
    pub vsock_port: u32,
}

impl HcsVmConfigV1 {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.id.get_version_num() == 7,
            "HCS VM identifier must be UUIDv7"
        );
        validate_boot_file_path(&self.kernel, "HCS kernel")?;
        validate_boot_file_path(&self.initrd, "HCS initrd")?;
        ensure!(
            self.kernel != self.initrd,
            "HCS kernel and initrd must be distinct"
        );
        if let Some(toolchain) = &self.toolchain_vhdx {
            validate_vhdx_path(toolchain)?;
        }
        ensure!(
            (128..=8_192).contains(&self.memory_mib),
            "HCS VM memory must be between 128 and 8192 MiB"
        );
        ensure!(
            (1..=16).contains(&self.processor_count),
            "HCS VM processor count must be between 1 and 16"
        );
        ensure!(
            (1_024..=u32::MAX).contains(&self.vsock_port),
            "HCS VM vsock port is invalid"
        );
        Ok(())
    }

    pub fn configuration_json(&self) -> Result<String> {
        self.validate()?;
        let scsi = self.toolchain_vhdx.as_ref().map(|toolchain| {
            serde_json::json!({
                (HCS_SCSI_CONTROLLER_0): {
                    "Attachments": {
                        "0": ReadOnlyDiskV1::new(toolchain)
                    }
                }
            })
        });
        let service_id = hyperv_vsock_service_id(self.vsock_port);
        let mut configuration = serde_json::json!({
            "SchemaVersion": { "Major": 2, "Minor": 1 },
            "Owner": "Reporch Runtime",
            "ShouldTerminateOnLastHandleClosed": true,
            "VirtualMachine": {
                "StopOnReset": true,
                "Chipset": {
                    "LinuxKernelDirect": {
                        "KernelFilePath": self.kernel.to_string_lossy(),
                        "InitRdPath": self.initrd.to_string_lossy(),
                        "KernelCmdLine": HCS_KERNEL_COMMAND_LINE
                    }
                },
                "ComputeTopology": {
                    "Memory": {
                        "Backing": "Virtual",
                        "SizeInMB": self.memory_mib
                    },
                    "Processor": { "Count": self.processor_count }
                },
                "Devices": {
                    "HvSocket": {
                        "HvSocketConfig": {
                            "DefaultBindSecurityDescriptor": "D:P(A;;GA;;;SY)(A;;GA;;;BA)",
                            "DefaultConnectSecurityDescriptor": "D:P(A;;GA;;;SY)(A;;GA;;;BA)",
                            "ServiceTable": {
                                service_id: { "AllowWildcardBinds": false }
                            }
                        }
                    }
                }
            }
        });
        if let Some(scsi) = scsi {
            configuration["VirtualMachine"]["Devices"]["Scsi"] = scsi;
        }
        Ok(serde_json::to_string(&configuration)?)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ReadOnlyDiskV1 {
    #[serde(rename = "Type")]
    kind: &'static str,
    path: String,
    read_only: bool,
}

impl ReadOnlyDiskV1 {
    fn new(path: &Path) -> Self {
        Self {
            kind: "VirtualDisk",
            path: path.to_string_lossy().into_owned(),
            read_only: true,
        }
    }
}

fn validate_vhdx_path(path: &Path) -> Result<()> {
    validate_boot_file_path(path, "HCS VHDX")?;
    ensure!(
        path.extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("vhdx")),
        "HCS disk image must use the VHDX format"
    );
    Ok(())
}

fn validate_boot_file_path(path: &Path, label: &str) -> Result<()> {
    ensure!(path.is_absolute(), "{label} path must be absolute");
    ensure!(
        !path
            .as_os_str()
            .to_string_lossy()
            .contains(['\0', '\r', '\n']),
        "{label} path contains invalid characters"
    );
    Ok(())
}

/// Hyper-V sockets map Linux AF_VSOCK ports into this service GUID template.
pub fn hyperv_vsock_service_id(port: u32) -> String {
    format!("{port:08x}-facb-11e6-bd58-64006a7986d3")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_uses_direct_kernel_no_network_and_read_only_toolchain() {
        let kernel = if cfg!(windows) {
            PathBuf::from(r"C:\ProgramData\Reporch\kernel")
        } else {
            PathBuf::from("/var/lib/reporch/kernel")
        };
        let initrd = kernel.with_file_name("rootfs.cpio");
        let toolchain = kernel.with_file_name("toolchain.vhdx");
        let config = HcsVmConfigV1 {
            id: Uuid::now_v7(),
            kernel: kernel.clone(),
            initrd: initrd.clone(),
            toolchain_vhdx: Some(toolchain),
            memory_mib: 512,
            processor_count: 1,
            vsock_port: HYPERV_VSOCK_PORT,
        };
        let value: serde_json::Value =
            serde_json::from_str(&config.configuration_json().unwrap()).unwrap();
        let devices = &value["VirtualMachine"]["Devices"];
        assert!(devices.get("NetworkAdapters").is_none());
        assert_eq!(
            devices["Scsi"][HCS_SCSI_CONTROLLER_0]["Attachments"]["0"]["ReadOnly"],
            true
        );
        assert_eq!(
            value["VirtualMachine"]["Chipset"]["LinuxKernelDirect"]["KernelFilePath"],
            kernel.to_string_lossy().as_ref()
        );
        assert_eq!(
            value["VirtualMachine"]["Chipset"]["LinuxKernelDirect"]["InitRdPath"],
            initrd.to_string_lossy().as_ref()
        );
        assert_eq!(
            value["VirtualMachine"]["Chipset"]["LinuxKernelDirect"]["KernelCmdLine"],
            HCS_KERNEL_COMMAND_LINE
        );
        assert_eq!(value["VirtualMachine"]["StopOnReset"], true);
        assert!(
            devices["HvSocket"]["HvSocketConfig"]["ServiceTable"]
                .get(hyperv_vsock_service_id(HYPERV_VSOCK_PORT))
                .is_some()
        );
    }

    #[test]
    fn configuration_rejects_host_path_and_resource_ambiguity() {
        let mut config = HcsVmConfigV1 {
            id: Uuid::now_v7(),
            kernel: PathBuf::from("relative-vmlinux"),
            initrd: PathBuf::from("relative-rootfs.cpio"),
            toolchain_vhdx: None,
            memory_mib: 512,
            processor_count: 1,
            vsock_port: HYPERV_VSOCK_PORT,
        };
        assert!(config.validate().is_err());
        config.kernel = if cfg!(windows) {
            PathBuf::from(r"C:\kernel")
        } else {
            PathBuf::from("/kernel")
        };
        config.initrd = config.kernel.clone();
        assert!(config.validate().is_err());
        config.initrd = config.kernel.with_file_name("rootfs.cpio");
        config.toolchain_vhdx = Some(config.kernel.with_file_name("toolchain.img"));
        assert!(config.validate().is_err());
    }

    #[test]
    fn configuration_omits_scsi_without_a_toolchain() {
        let root = if cfg!(windows) {
            "C:\\Runtime"
        } else {
            "/runtime"
        };
        let config = HcsVmConfigV1 {
            id: Uuid::now_v7(),
            kernel: PathBuf::from(root).join("kernel"),
            initrd: PathBuf::from(root).join("rootfs.cpio"),
            toolchain_vhdx: None,
            memory_mib: 512,
            processor_count: 1,
            vsock_port: HYPERV_VSOCK_PORT,
        };
        let value: serde_json::Value =
            serde_json::from_str(&config.configuration_json().unwrap()).unwrap();
        assert!(value["VirtualMachine"]["Devices"].get("Scsi").is_none());
    }
}
