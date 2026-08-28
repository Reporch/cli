#![forbid(unsafe_code)]

use std::fs;
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const MAX_GUESTD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_KERNEL_MODULE_BYTES: u64 = 32 * 1024 * 1024;
const CPIO_HEADER_BYTES: usize = 110;
const TRAILER: &str = "TRAILER!!!";
const REQUIRED_VSOCK_MODULES: [&str; 3] = [
    "vsock.ko",
    "vmw_vsock_virtio_transport_common.ko",
    "vmw_vsock_virtio_transport.ko",
];

fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1).map(PathBuf::from);
    let guestd = arguments.next().context("missing guestd path")?;
    let output = arguments.next().context("missing output path")?;
    let architecture = arguments
        .next()
        .context("missing architecture (x86_64 or aarch64)")?;
    let architecture = architecture
        .to_str()
        .context("architecture must be UTF-8")?;
    let modules = arguments.collect::<Vec<_>>();
    build(
        &guestd,
        &output,
        architecture,
        source_date_epoch()?,
        &modules,
    )?;
    Ok(())
}

fn build(
    guestd: &Path,
    output: &Path,
    architecture: &str,
    mtime: u32,
    module_paths: &[PathBuf],
) -> Result<String> {
    ensure!(!output.exists(), "runtime image output already exists");
    let guestd = read_static_guestd(guestd, architecture)?;
    let modules = read_kernel_modules(module_paths, architecture)?;
    let parent = output
        .parent()
        .context("runtime image output has no parent")?;
    ensure!(
        parent.is_dir(),
        "runtime image output parent does not exist"
    );
    let temporary = parent.join(format!(".rootfs-{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<String> {
        let mut archive = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .context("create temporary runtime rootfs")?;
        let mut inode = 1_u32;
        let mut directories = vec![
            ("dev", 0o040755),
            ("proc", 0o040555),
            ("run", 0o040755),
            ("run/reporch", 0o040777),
            ("run/reporch/home", 0o040777),
            ("run/reporch/tmp", 0o041777),
            ("sbin", 0o040555),
            ("sys", 0o040555),
            ("tmp", 0o041777),
            ("toolchain", 0o040555),
            ("toolchain-overlay", 0o040700),
            ("toolchain-ro", 0o040555),
            ("workspace", 0o040755),
        ];
        if !modules.is_empty() {
            directories.extend([
                ("lib", 0o040555),
                ("lib/modules", 0o040555),
                ("lib/modules/reporch", 0o040555),
            ]);
        }
        for (name, mode) in directories {
            write_entry(&mut archive, inode, name, mode, 2, mtime, &[])?;
            inode += 1;
        }
        write_entry(
            &mut archive,
            inode,
            "sbin/reporch-guestd",
            0o100555,
            1,
            mtime,
            &guestd,
        )?;
        inode += 1;
        for (name, contents) in &modules {
            write_entry(
                &mut archive,
                inode,
                &format!("lib/modules/reporch/{name}"),
                0o100444,
                1,
                mtime,
                contents,
            )?;
            inode += 1;
        }
        write_entry(&mut archive, inode, TRAILER, 0, 1, mtime, &[])?;
        pad_to(&mut archive, 512)?;
        archive.sync_all().context("sync runtime rootfs")?;
        drop(archive);
        fs::rename(&temporary, output).context("atomically install runtime rootfs")?;
        hash_file(output)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_kernel_modules(paths: &[PathBuf], architecture: &str) -> Result<Vec<(String, Vec<u8>)>> {
    ensure!(
        paths.is_empty() || paths.len() == REQUIRED_VSOCK_MODULES.len(),
        "provide either no kernel modules or the exact three Reporch vsock modules"
    );
    let mut by_name = std::collections::BTreeMap::new();
    for path in paths {
        let metadata = fs::symlink_metadata(path).context("inspect runtime kernel module")?;
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && (1..=MAX_KERNEL_MODULE_BYTES).contains(&metadata.len()),
            "kernel module must be a bounded regular non-symlink file"
        );
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("kernel module name must be portable UTF-8")?;
        ensure!(
            REQUIRED_VSOCK_MODULES.contains(&name),
            "unexpected runtime kernel module {name}"
        );
        let contents = fs::read(path).context("read runtime kernel module")?;
        ensure!(
            contents.len() as u64 == metadata.len(),
            "kernel module changed while being read"
        );
        validate_kernel_module(&contents, architecture)?;
        ensure!(
            by_name.insert(name.to_owned(), contents).is_none(),
            "duplicate runtime kernel module {name}"
        );
    }
    if !paths.is_empty() {
        for name in REQUIRED_VSOCK_MODULES {
            ensure!(
                by_name.contains_key(name),
                "missing runtime kernel module {name}"
            );
        }
    }
    Ok(REQUIRED_VSOCK_MODULES
        .iter()
        .filter_map(|name| {
            by_name
                .remove(*name)
                .map(|contents| ((*name).into(), contents))
        })
        .collect())
}

fn validate_kernel_module(bytes: &[u8], architecture: &str) -> Result<()> {
    validate_elf_identity(bytes, architecture, "kernel module")?;
    ensure!(
        u16::from_le_bytes(bytes[16..18].try_into()?) == 1,
        "kernel module must be a relocatable ELF object"
    );
    Ok(())
}

fn write_entry(
    writer: &mut fs::File,
    inode: u32,
    name: &str,
    mode: u32,
    links: u32,
    mtime: u32,
    contents: &[u8],
) -> Result<()> {
    ensure!(
        !name.is_empty() && !name.starts_with('/') && !name.contains('\0'),
        "invalid initramfs entry name"
    );
    let name_size = u32::try_from(name.len() + 1)?;
    let file_size = u32::try_from(contents.len())?;
    let header = format!(
        "070701{inode:08x}{mode:08x}{:08x}{:08x}{links:08x}{mtime:08x}{file_size:08x}{:08x}{:08x}{:08x}{:08x}{name_size:08x}{:08x}",
        0, 0, 0, 0, 0, 0, 0
    );
    ensure!(
        header.len() == CPIO_HEADER_BYTES,
        "invalid newc header length"
    );
    writer
        .write_all(header.as_bytes())
        .context("write newc header")?;
    writer
        .write_all(name.as_bytes())
        .context("write newc name")?;
    writer.write_all(&[0]).context("terminate newc name")?;
    pad_to(writer, 4)?;
    writer.write_all(contents).context("write newc contents")?;
    pad_to(writer, 4)
}

fn pad_to(writer: &mut fs::File, alignment: u64) -> Result<()> {
    let offset = writer.stream_position().context("read initramfs offset")?;
    let padding = (alignment - offset % alignment) % alignment;
    if padding > 0 {
        writer
            .write_all(&vec![0; padding as usize])
            .context("pad initramfs")?;
    }
    Ok(())
}

fn read_static_guestd(path: &Path, architecture: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).context("inspect guestd binary")?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && (1..=MAX_GUESTD_BYTES).contains(&metadata.len()),
        "guestd must be a bounded regular non-symlink file"
    );
    let bytes = fs::read(path).context("read guestd binary")?;
    ensure!(
        bytes.len() as u64 == metadata.len(),
        "guestd changed while being read"
    );
    validate_static_elf(&bytes, architecture)?;
    Ok(bytes)
}

fn validate_static_elf(bytes: &[u8], architecture: &str) -> Result<()> {
    validate_elf_identity(bytes, architecture, "guestd")?;
    let phoff = u64::from_le_bytes(bytes[32..40].try_into()?);
    let entry_size = u16::from_le_bytes(bytes[54..56].try_into()?) as u64;
    let entries = u16::from_le_bytes(bytes[56..58].try_into()?) as u64;
    ensure!(
        entry_size >= 56 && entries <= 1_024,
        "guestd program header table is invalid"
    );
    let end = phoff
        .checked_add(
            entry_size
                .checked_mul(entries)
                .context("program header overflow")?,
        )
        .context("program header overflow")?;
    ensure!(
        end <= bytes.len() as u64,
        "guestd program header table is truncated"
    );
    for index in 0..entries {
        let offset = usize::try_from(phoff + index * entry_size)?;
        let program_type = u32::from_le_bytes(bytes[offset..offset + 4].try_into()?);
        ensure!(
            program_type != 3,
            "guestd is dynamically linked (PT_INTERP present)"
        );
    }
    Ok(())
}

fn validate_elf_identity(bytes: &[u8], architecture: &str, label: &str) -> Result<()> {
    ensure!(
        bytes.len() >= 64 && &bytes[..4] == b"\x7fELF",
        "{label} is not ELF"
    );
    ensure!(
        bytes[4] == 2 && bytes[5] == 1,
        "{label} must be little-endian ELF64"
    );
    let expected_machine = match architecture {
        "x86_64" => 62_u16,
        "aarch64" => 183_u16,
        _ => anyhow::bail!("unsupported guest architecture"),
    };
    ensure!(
        u16::from_le_bytes(bytes[18..20].try_into()?) == expected_machine,
        "{label} architecture mismatch"
    );
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).context("open built runtime rootfs")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .context("hash built runtime rootfs")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn source_date_epoch() -> Result<u32> {
    std::env::var("SOURCE_DATE_EPOCH")
        .unwrap_or_else(|_| "0".into())
        .parse::<u32>()
        .context("SOURCE_DATE_EPOCH must be a non-negative 32-bit integer")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_elf(machine: u16, interpreter: bool) -> Vec<u8> {
        let mut bytes = vec![0_u8; 64 + 56];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
        bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&1_u16.to_le_bytes());
        bytes[64..68].copy_from_slice(&(if interpreter { 3_u32 } else { 1_u32 }).to_le_bytes());
        bytes
    }

    #[test]
    fn elf_gate_rejects_dynamic_or_wrong_architecture() {
        validate_static_elf(&fake_elf(62, false), "x86_64").unwrap();
        assert!(validate_static_elf(&fake_elf(62, true), "x86_64").is_err());
        assert!(validate_static_elf(&fake_elf(183, false), "x86_64").is_err());
    }

    #[test]
    fn newc_output_is_deterministic_and_has_a_trailer() {
        let root = tempfile::tempdir().unwrap();
        let guestd = root.path().join("guestd");
        fs::write(&guestd, fake_elf(62, false)).unwrap();
        let first = root.path().join("first.cpio");
        let second = root.path().join("second.cpio");
        let first_digest = build(&guestd, &first, "x86_64", 0, &[]).unwrap();
        let second_digest = build(&guestd, &second, "x86_64", 0, &[]).unwrap();
        assert_eq!(first_digest, second_digest);
        assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
        assert!(
            fs::read(&first)
                .unwrap()
                .windows(TRAILER.len())
                .any(|window| window == TRAILER.as_bytes())
        );
    }
}
