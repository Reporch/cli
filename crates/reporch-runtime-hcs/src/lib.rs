#![deny(unsafe_code)]

use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use serde::Serialize;
use uuid::Uuid;

#[cfg(windows)]
mod windows_backend;
#[cfg(windows)]
pub use windows_backend::{HcsVirtualMachine, HvSocketStream};

pub const HYPERV_VSOCK_PORT: u32 = 7_000;

#[derive(Clone, Debug)]
pub struct HcsVmConfigV1 {
    pub id: Uuid,
    pub rootfs_vhdx: PathBuf,
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
        validate_vhdx_path(&self.rootfs_vhdx)?;
        if let Some(toolchain) = &self.toolchain_vhdx {
            validate_vhdx_path(toolchain)?;
            ensure!(
                toolchain != &self.rootfs_vhdx,
                "rootfs and toolchain VHDX must be distinct"
            );
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
        let mut attachments = serde_json::Map::new();
        attachments.insert(
            "0".into(),
            serde_json::to_value(ReadOnlyDiskV1::new(&self.rootfs_vhdx))?,
        );
        if let Some(toolchain) = &self.toolchain_vhdx {
            attachments.insert(
                "1".into(),
                serde_json::to_value(ReadOnlyDiskV1::new(toolchain))?,
            );
        }
        let service_id = hyperv_vsock_service_id(self.vsock_port);
        let configuration = serde_json::json!({
            "SchemaVersion": { "Major": 2, "Minor": 1 },
            "Owner": "Reporch Runtime",
            "ShouldTerminateOnLastHandleClosed": true,
            "VirtualMachine": {
                "Chipset": {
                    "Uefi": {
                        "BootThis": {
                            "DevicePath": "Primary disk",
                            "DiskNumber": 0,
                            "DeviceType": "ScsiDrive"
                        }
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
                    "Scsi": {
                        "Primary disk": { "Attachments": attachments }
                    },
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
    ensure!(path.is_absolute(), "HCS VHDX path must be absolute");
    ensure!(
        path.extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("vhdx")),
        "HCS disk image must use the VHDX format"
    );
    ensure!(
        !path
            .as_os_str()
            .to_string_lossy()
            .contains(['\0', '\r', '\n']),
        "HCS VHDX path contains invalid characters"
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
    fn configuration_has_no_network_and_only_read_only_disks() {
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\ProgramData\Reporch\rootfs.vhdx")
        } else {
            PathBuf::from("/var/lib/reporch/rootfs.vhdx")
        };
        let toolchain = root.with_file_name("toolchain.vhdx");
        let config = HcsVmConfigV1 {
            id: Uuid::now_v7(),
            rootfs_vhdx: root,
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
            devices["Scsi"]["Primary disk"]["Attachments"]["0"]["ReadOnly"],
            true
        );
        assert_eq!(
            devices["Scsi"]["Primary disk"]["Attachments"]["1"]["ReadOnly"],
            true
        );
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
            rootfs_vhdx: PathBuf::from("relative.vhdx"),
            toolchain_vhdx: None,
            memory_mib: 512,
            processor_count: 1,
            vsock_port: HYPERV_VSOCK_PORT,
        };
        assert!(config.validate().is_err());
        config.rootfs_vhdx = if cfg!(windows) {
            PathBuf::from(r"C:\rootfs.vhdx")
        } else {
            PathBuf::from("/rootfs.vhdx")
        };
        config.toolchain_vhdx = Some(config.rootfs_vhdx.clone());
        assert!(config.validate().is_err());
    }
}
