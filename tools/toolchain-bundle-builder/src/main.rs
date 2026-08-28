#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use arcbox_ext4::Reader as Ext4Reader;
use oci2rootfs::{Converter, Ext4Options, OciLayoutSource, Platform};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use wait_timeout::ChildExt as _;

const MIN_IMAGE_MIB: u64 = 256;
const MAX_IMAGE_MIB: u64 = 8 * 1024;
const EXT4_MAGIC_OFFSET: u64 = 1_024 + 56;
const VHDX_HEADER_SIZE: usize = 4 * 1_024;
const VHDX_REGION_TABLE_SIZE: usize = 64 * 1_024;
const VHDX_HEADER_OFFSETS: [u64; 2] = [64 * 1_024, 128 * 1_024];
const VHDX_REGION_TABLE_OFFSETS: [u64; 2] = [192 * 1_024, 256 * 1_024];
const VHDX_METADATA_REGION_GUID: [u8; 16] = [
    0x06, 0xa2, 0x7c, 0x8b, 0x90, 0x47, 0x9a, 0x4b, 0xb8, 0xfe, 0x57, 0x5f, 0x05, 0x0f, 0x88, 0x6e,
];
const VHDX_VIRTUAL_DISK_ID_GUID: [u8; 16] = [
    0xab, 0x12, 0xca, 0xbe, 0xe6, 0xb2, 0x23, 0x45, 0x93, 0xef, 0xc3, 0x09, 0xe0, 0x00, 0xc7, 0x46,
];
const QEMU_IMG_VERSION_PREFIX: &str = "qemu-img version 11.1.1";
const QEMU_TIMEOUT: Duration = Duration::from_secs(180);

enum OutputFilesystem {
    Ext4,
    Vhdx { qemu_img: PathBuf },
}

fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let layout = required_path(&mut arguments, "OCI image layout")?;
    let architecture = required_utf8(&mut arguments, "linux architecture")?;
    let output = required_path(&mut arguments, "output compressed ext4 archive")?;
    let size_mib = required_utf8(&mut arguments, "image size in MiB")?
        .parse::<u64>()
        .context("image size must be an integer")?;
    let filesystem_identity = required_utf8(&mut arguments, "filesystem UUID or signed identity")?;
    let filesystem_uuid = filesystem_uuid(&filesystem_identity, &architecture)?;
    let qemu_img = arguments.next().map(PathBuf::from);
    ensure!(arguments.next().is_none(), "too many arguments");

    let layout = canonical_directory(&layout, "OCI image layout")?;
    validate_output(&output, &layout)?;
    let filesystem = if output.to_string_lossy().ends_with(".ext4.zst") {
        ensure!(qemu_img.is_none(), "qemu-img is only valid for VHDX output");
        OutputFilesystem::Ext4
    } else if output.to_string_lossy().ends_with(".vhdx.zst") {
        let qemu_img = qemu_img.context("VHDX output requires an absolute qemu-img executable")?;
        validate_qemu_img(&qemu_img)?;
        OutputFilesystem::Vhdx { qemu_img }
    } else {
        anyhow::bail!("toolchain output must end in .ext4.zst or .vhdx.zst");
    };
    let parent = output.parent().context("toolchain output has no parent")?;
    let build_id = Uuid::now_v7();
    let raw_ext4 = parent.join(format!(".toolchain-{build_id}.ext4"));
    let vhdx = parent.join(format!(".toolchain-{build_id}.vhdx"));
    let result = (|| -> Result<()> {
        let ext4_sha256 = build_ext4(&layout, &architecture, &raw_ext4, size_mib, filesystem_uuid)?;
        let (image, image_sha256, filesystem_name) = match &filesystem {
            OutputFilesystem::Ext4 => (&raw_ext4, ext4_sha256, "ext4"),
            OutputFilesystem::Vhdx { qemu_img } => {
                let digest = convert_vhdx(qemu_img, &raw_ext4, &vhdx, &ext4_sha256)?;
                (&vhdx, digest, "vhdx")
            }
        };
        let image_size = fs::metadata(image)?.len();
        let (archive_sha256, archive_size) = compress_image(image, &output, image_size)?;
        println!(
            "{{\"schema\":\"reporch.toolchain-bundle-build.v2\",\"source_identity\":\"{filesystem_identity}\",\"architecture\":\"{architecture}\",\"filesystem\":\"{filesystem_name}\",\"image_sha256\":\"{image_sha256}\",\"image_size\":{image_size},\"archive_sha256\":\"{archive_sha256}\",\"archive_size\":{archive_size},\"compression\":\"zstd\"}}"
        );
        Ok(())
    })();
    let _ = fs::remove_file(&raw_ext4);
    let _ = fs::remove_file(&vhdx);
    result?;
    Ok(())
}

fn filesystem_uuid(value: &str, architecture: &str) -> Result<Uuid> {
    if let Ok(value) = value.parse::<Uuid>() {
        return Ok(value);
    }
    let digest = value
        .strip_prefix("sha256:")
        .context("filesystem identity must be a UUID or SHA-256 identity")?;
    ensure!(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "filesystem SHA-256 identity is invalid"
    );
    let mut hasher = Sha256::new();
    hasher.update(b"reporch-toolchain-filesystem-uuid-v1");
    hasher.update([0]);
    hasher.update(digest.as_bytes());
    hasher.update([0]);
    hasher.update(architecture.as_bytes());
    let bytes = hasher.finalize();
    let mut uuid = [0_u8; 16];
    uuid.copy_from_slice(&bytes[..16]);
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    Ok(Uuid::from_bytes(uuid))
}

fn validate_qemu_img(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "qemu-img path must be absolute");
    let metadata = fs::symlink_metadata(path).context("inspect qemu-img")?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "qemu-img must be a regular non-symlink executable"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        ensure!(
            metadata.permissions().mode() & 0o111 != 0,
            "qemu-img is not executable"
        );
    }
    let version = run_qemu(path, &["--version"], true)?;
    let version = String::from_utf8(version).context("qemu-img version output is not UTF-8")?;
    ensure!(
        version.starts_with(QEMU_IMG_VERSION_PREFIX),
        "qemu-img version must start with {QEMU_IMG_VERSION_PREFIX}"
    );
    Ok(())
}

fn convert_vhdx(qemu_img: &Path, raw_ext4: &Path, output: &Path, identity: &str) -> Result<String> {
    ensure!(!output.exists(), "VHDX output already exists");
    let raw = raw_ext4
        .to_str()
        .context("raw ext4 path must be portable UTF-8")?;
    let vhdx = output
        .to_str()
        .context("VHDX path must be portable UTF-8")?;
    run_qemu(
        qemu_img,
        &[
            "convert",
            "-q",
            "-f",
            "raw",
            "-O",
            "vhdx",
            "-o",
            "subformat=fixed",
            raw,
            vhdx,
        ],
        false,
    )?;
    let result = (|| -> Result<String> {
        normalize_vhdx(output, identity)?;
        run_qemu(qemu_img, &["check", "-q", "-f", "vhdx", vhdx], false)?;
        run_qemu(
            qemu_img,
            &["compare", "-q", "-f", "raw", "-F", "vhdx", raw, vhdx],
            false,
        )?;
        set_read_only(output)?;
        let size = fs::metadata(output)?.len();
        hash_regular(output, size)
    })();
    if result.is_err() {
        let _ = fs::remove_file(output);
    }
    result
}

fn run_qemu(program: &Path, arguments: &[&str], capture_stdout: bool) -> Result<Vec<u8>> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .env_clear()
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(if capture_stdout {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command.spawn().context("start pinned qemu-img")?;
    let status = match child
        .wait_timeout(QEMU_TIMEOUT)
        .context("wait for pinned qemu-img")?
    {
        Some(status) => status,
        None => {
            #[cfg(unix)]
            if let Ok(raw) = i32::try_from(child.id())
                && let Some(pid) = rustix::process::Pid::from_raw(raw)
            {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("qemu-img exceeded its bounded execution time");
        }
    };
    ensure!(status.success(), "qemu-img failed with {status}");
    if let Some(mut stdout) = child.stdout.take() {
        let mut bytes = Vec::new();
        stdout
            .by_ref()
            .take(16 * 1_024)
            .read_to_end(&mut bytes)
            .context("read bounded qemu-img output")?;
        ensure!(
            bytes.len() < 16 * 1_024,
            "qemu-img output exceeded its limit"
        );
        Ok(bytes)
    } else {
        Ok(Vec::new())
    }
}

fn normalize_vhdx(path: &Path, identity: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("inspect generated VHDX")?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "generated VHDX must be a regular non-symlink file"
    );
    ensure!(
        metadata.len() >= 4 * 1_024 * 1_024 && metadata.len() <= 16 * 1_073_741_824,
        "generated VHDX size is outside the supported range"
    );
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .context("open generated VHDX for deterministic normalization")?;
    let identifier = read_exact_at::<8>(&mut file, 0)?;
    ensure!(&identifier == b"vhdxfile", "generated image is not VHDX");

    let first_header = read_header(&mut file, VHDX_HEADER_OFFSETS[0])?;
    let second_header = read_header(&mut file, VHDX_HEADER_OFFSETS[1])?;
    let log_offset = read_u64(&first_header, 72)?;
    let log_length = u64::from(read_u32(&first_header, 68)?);
    ensure!(
        read_u64(&second_header, 72)? == log_offset
            && u64::from(read_u32(&second_header, 68)?) == log_length,
        "VHDX headers disagree about the log region"
    );
    ensure!(
        log_length > 0
            && log_length <= 16 * 1_024 * 1_024
            && log_offset % (1_024 * 1_024) == 0
            && log_length % (1_024 * 1_024) == 0
            && log_offset
                .checked_add(log_length)
                .is_some_and(|end| end <= metadata.len()),
        "VHDX log region is invalid"
    );
    zero_region(&mut file, log_offset, log_length)?;

    let file_write_guid = deterministic_guid(identity, b"reporch-vhdx-file-write-guid-v1");
    let data_write_guid = deterministic_guid(identity, b"reporch-vhdx-data-write-guid-v1");
    for (index, offset) in VHDX_HEADER_OFFSETS.into_iter().enumerate() {
        let mut header = if index == 0 {
            first_header.clone()
        } else {
            second_header.clone()
        };
        header[8..16].copy_from_slice(&(index as u64 + 1).to_le_bytes());
        header[16..32].copy_from_slice(&file_write_guid);
        header[32..48].copy_from_slice(&data_write_guid);
        header[48..64].fill(0);
        header[4..8].fill(0);
        let checksum = crc32c(&header);
        header[4..8].copy_from_slice(&checksum.to_le_bytes());
        write_all_at(&mut file, offset, &header)?;
    }

    let primary_region = read_region_table(&mut file, VHDX_REGION_TABLE_OFFSETS[0])?;
    let secondary_region = read_region_table(&mut file, VHDX_REGION_TABLE_OFFSETS[1])?;
    ensure!(
        primary_region == secondary_region,
        "VHDX region table copies differ"
    );
    let (metadata_offset, metadata_length) = metadata_region(&primary_region, metadata.len())?;
    let metadata_length = usize::try_from(metadata_length).context("metadata region too large")?;
    ensure!(
        metadata_length <= 16 * 1_024 * 1_024,
        "VHDX metadata region exceeds the supported limit"
    );
    let mut metadata_bytes = vec![0_u8; metadata_length];
    read_all_at(&mut file, metadata_offset, &mut metadata_bytes)?;
    ensure!(
        metadata_bytes.get(..8) == Some(b"metadata"),
        "VHDX metadata signature is invalid"
    );
    let entry_count = usize::from(read_u16(&metadata_bytes, 10)?);
    ensure!(entry_count <= 2_047, "VHDX has too many metadata entries");
    let entries_end = 32_usize
        .checked_add(
            entry_count
                .checked_mul(32)
                .context("metadata entry overflow")?,
        )
        .context("metadata table overflow")?;
    ensure!(
        entries_end <= metadata_bytes.len(),
        "VHDX metadata table is truncated"
    );
    let mut disk_id_offset = None;
    for index in 0..entry_count {
        let entry = &metadata_bytes[32 + index * 32..32 + (index + 1) * 32];
        if entry[..16] == VHDX_VIRTUAL_DISK_ID_GUID {
            let item_offset = u64::from(read_u32(entry, 16)?);
            let item_length = u64::from(read_u32(entry, 20)?);
            ensure!(item_length == 16, "VHDX virtual disk ID has invalid length");
            ensure!(
                item_offset
                    .checked_add(item_length)
                    .is_some_and(|end| end <= metadata_bytes.len() as u64),
                "VHDX virtual disk ID is outside the metadata region"
            );
            ensure!(
                disk_id_offset.is_none(),
                "VHDX has duplicate virtual disk IDs"
            );
            disk_id_offset = Some(item_offset);
        }
    }
    let disk_id_offset = disk_id_offset.context("VHDX metadata is missing virtual disk ID")?;
    let disk_id = deterministic_guid(identity, b"reporch-vhdx-virtual-disk-guid-v1");
    write_all_at(&mut file, metadata_offset + disk_id_offset, &disk_id)?;
    file.sync_all().context("sync normalized VHDX")?;
    Ok(())
}

fn read_header(file: &mut fs::File, offset: u64) -> Result<Vec<u8>> {
    let mut header = vec![0_u8; VHDX_HEADER_SIZE];
    read_all_at(file, offset, &mut header)?;
    ensure!(&header[..4] == b"head", "VHDX header signature is invalid");
    let expected = read_u32(&header, 4)?;
    header[4..8].fill(0);
    ensure!(
        crc32c(&header) == expected,
        "VHDX header checksum is invalid"
    );
    header[4..8].copy_from_slice(&expected.to_le_bytes());
    Ok(header)
}

fn read_region_table(file: &mut fs::File, offset: u64) -> Result<Vec<u8>> {
    let mut table = vec![0_u8; VHDX_REGION_TABLE_SIZE];
    read_all_at(file, offset, &mut table)?;
    ensure!(
        &table[..4] == b"regi",
        "VHDX region table signature is invalid"
    );
    let expected = read_u32(&table, 4)?;
    table[4..8].fill(0);
    ensure!(
        crc32c(&table) == expected,
        "VHDX region table checksum is invalid"
    );
    table[4..8].copy_from_slice(&expected.to_le_bytes());
    Ok(table)
}

fn metadata_region(table: &[u8], file_size: u64) -> Result<(u64, u64)> {
    let entry_count = usize::try_from(read_u32(table, 8)?).context("region count overflow")?;
    ensure!(entry_count <= 2_047, "VHDX has too many region entries");
    let entries_end = 16_usize
        .checked_add(
            entry_count
                .checked_mul(32)
                .context("region entry overflow")?,
        )
        .context("region table overflow")?;
    ensure!(entries_end <= table.len(), "VHDX region table is truncated");
    let mut result = None;
    for index in 0..entry_count {
        let entry = &table[16 + index * 32..16 + (index + 1) * 32];
        if entry[..16] == VHDX_METADATA_REGION_GUID {
            let offset = read_u64(entry, 16)?;
            let length = u64::from(read_u32(entry, 24)?);
            ensure!(
                length > 0
                    && offset % (1_024 * 1_024) == 0
                    && offset
                        .checked_add(length)
                        .is_some_and(|end| end <= file_size),
                "VHDX metadata region is invalid"
            );
            ensure!(result.is_none(), "VHDX has duplicate metadata regions");
            result = Some((offset, length));
        }
    }
    result.context("VHDX region table is missing metadata")
}

fn deterministic_guid(identity: &str, domain: &[u8]) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update([0]);
    hasher.update(identity.as_bytes());
    let digest = hasher.finalize();
    let mut guid = [0_u8; 16];
    guid.copy_from_slice(&digest[..16]);
    guid[7] = (guid[7] & 0x0f) | 0x40;
    guid[8] = (guid[8] & 0x3f) | 0x80;
    guid
}

fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn zero_region(file: &mut fs::File, offset: u64, length: u64) -> Result<()> {
    file.seek(SeekFrom::Start(offset))
        .context("seek VHDX log")?;
    let zeros = [0_u8; 1024 * 1024];
    let mut remaining = length;
    while remaining > 0 {
        let count = usize::try_from(remaining.min(zeros.len() as u64)).unwrap_or(zeros.len());
        std::io::Write::write_all(file, &zeros[..count]).context("zero VHDX log")?;
        remaining -= count as u64;
    }
    Ok(())
}

fn read_exact_at<const N: usize>(file: &mut fs::File, offset: u64) -> Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    read_all_at(file, offset, &mut bytes)?;
    Ok(bytes)
}

fn read_all_at(file: &mut fs::File, offset: u64, bytes: &mut [u8]) -> Result<()> {
    file.seek(SeekFrom::Start(offset)).context("seek VHDX")?;
    file.read_exact(bytes).context("read VHDX structure")
}

fn write_all_at(file: &mut fs::File, offset: u64, bytes: &[u8]) -> Result<()> {
    file.seek(SeekFrom::Start(offset)).context("seek VHDX")?;
    std::io::Write::write_all(file, bytes).context("write VHDX structure")
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .context("VHDX u16 is out of bounds")?;
    Ok(u16::from_le_bytes(value.try_into().unwrap()))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .context("VHDX u32 is out of bounds")?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let value = bytes
        .get(offset..offset + 8)
        .context("VHDX u64 is out of bounds")?;
    Ok(u64::from_le_bytes(value.try_into().unwrap()))
}

fn build_ext4(
    layout: &Path,
    architecture: &str,
    output: &Path,
    size_mib: u64,
    filesystem_uuid: Uuid,
) -> Result<String> {
    source_date_epoch()?;
    ensure!(
        (MIN_IMAGE_MIB..=MAX_IMAGE_MIB).contains(&size_mib),
        "toolchain image must be between {MIN_IMAGE_MIB} and {MAX_IMAGE_MIB} MiB"
    );
    let layout = canonical_directory(layout, "OCI image layout")?;
    ensure!(
        matches!(architecture, "amd64" | "arm64"),
        "linux architecture must be amd64 or arm64"
    );
    validate_output(output, &layout)?;
    let image_bytes = size_mib
        .checked_mul(1_024 * 1_024)
        .context("toolchain image size overflow")?;
    let parent = output
        .parent()
        .context("toolchain image output has no parent")?;
    let temporary = parent.join(format!(".toolchain-{}.ext4.tmp", Uuid::now_v7()));
    let result = (|| -> Result<String> {
        let source = OciLayoutSource::open(&layout)
            .context("open content-addressed OCI image layout")?
            .platform(Platform::new("linux", architecture));
        Converter::new(&temporary)
            .size(image_bytes)
            .ext4_options(Ext4Options::new().label("REPORCH_TC").uuid(filesystem_uuid))
            .convert(source)
            .context("convert OCI layers directly into deterministic ext4")?;
        let metadata = fs::symlink_metadata(&temporary).context("inspect generated ext4 image")?;
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() == image_bytes,
            "toolchain builder produced an invalid image: expected {image_bytes} bytes, got {} bytes, regular={}, symlink={}",
            metadata.len(),
            metadata.is_file(),
            metadata.file_type().is_symlink()
        );
        verify_ext4_magic(&temporary)?;
        verify_root_filesystem(&temporary, architecture)?;
        set_read_only(&temporary)?;
        let digest = hash_regular(&temporary, image_bytes)?;
        fs::rename(&temporary, output).context("atomically install toolchain image")?;
        sync_parent(parent)?;
        Ok(digest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn compress_image(image: &Path, output: &Path, image_size: u64) -> Result<(String, u64)> {
    ensure!(!output.exists(), "toolchain archive already exists");
    let parent = output.parent().context("toolchain archive has no parent")?;
    let temporary = parent.join(format!(".toolchain-{}.zst.tmp", Uuid::now_v7()));
    let result = (|| -> Result<(String, u64)> {
        let mut input = fs::File::open(image).context("open ext4 image for compression")?;
        let archive = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .context("create toolchain archive")?;
        let mut encoder = zstd::stream::write::Encoder::new(archive, 19)
            .context("initialize deterministic zstd encoder")?;
        encoder.include_checksum(true)?;
        encoder.include_contentsize(true)?;
        encoder.set_pledged_src_size(Some(image_size))?;
        std::io::copy(&mut input, &mut encoder).context("compress ext4 toolchain image")?;
        let archive = encoder.finish().context("finish toolchain zstd frame")?;
        archive.sync_all().context("sync toolchain archive")?;
        drop(archive);
        let archive_size = fs::metadata(&temporary)?.len();
        ensure!(
            archive_size > 0 && archive_size < image_size && archive_size <= 2 * 1_073_741_824,
            "toolchain archive size is invalid"
        );
        let archive_sha256 = hash_regular(&temporary, archive_size)?;
        set_read_only(&temporary)?;
        fs::rename(&temporary, output).context("atomically install toolchain archive")?;
        sync_parent(parent)?;
        Ok((archive_sha256, archive_size))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn verify_root_filesystem(path: &Path, architecture: &str) -> Result<()> {
    let mut reader = Ext4Reader::new(path).context("parse generated ext4 filesystem")?;
    ensure!(reader.exists("/bin"), "toolchain image is missing /bin");
    ensure!(
        reader.exists("/bin/sh"),
        "toolchain image is missing /bin/sh"
    );
    let shell = reader
        .read_file("/bin/sh", 0, Some(64 * 1024 * 1024))
        .context("read toolchain /bin/sh")?;
    ensure!(
        shell.len() >= 20 && &shell[..4] == b"\x7fELF" && shell[4] == 2 && shell[5] == 1,
        "toolchain /bin/sh must resolve to little-endian ELF64"
    );
    let machine = u16::from_le_bytes([shell[18], shell[19]]);
    let expected = match architecture {
        "amd64" => 62,
        "arm64" => 183,
        _ => anyhow::bail!("unsupported toolchain architecture"),
    };
    ensure!(
        machine == expected,
        "toolchain /bin/sh architecture mismatch"
    );
    Ok(())
}

fn source_date_epoch() -> Result<u64> {
    let value = std::env::var("SOURCE_DATE_EPOCH")
        .context("SOURCE_DATE_EPOCH is required for reproducible toolchain images")?;
    let value = value
        .parse::<u64>()
        .context("SOURCE_DATE_EPOCH must be an integer")?;
    ensure!(
        value > 0 && value < (1_u64 << 34),
        "SOURCE_DATE_EPOCH is outside the ext4 timestamp range"
    );
    Ok(value)
}

fn verify_ext4_magic(path: &Path) -> Result<()> {
    let mut file = fs::File::open(path).context("open generated ext4 image")?;
    file.seek(SeekFrom::Start(EXT4_MAGIC_OFFSET))
        .context("seek ext4 superblock")?;
    let mut magic = [0_u8; 2];
    file.read_exact(&mut magic).context("read ext4 magic")?;
    ensure!(magic == [0x53, 0xef], "generated image is not ext4");
    Ok(())
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {label}"))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{label} must be a real directory"
    );
    path.canonicalize()
        .with_context(|| format!("resolve {label}"))
}

fn validate_output(output: &Path, layout: &Path) -> Result<()> {
    ensure!(
        output.is_absolute(),
        "toolchain output path must be absolute"
    );
    ensure!(!output.exists(), "toolchain output already exists");
    let parent = canonical_directory(
        output.parent().context("toolchain output has no parent")?,
        "toolchain output parent",
    )?;
    let layout = canonical_directory(layout, "OCI image layout")?;
    ensure!(
        !parent.starts_with(&layout),
        "toolchain output must not be inside the untrusted OCI layout"
    );
    Ok(())
}

fn set_read_only(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions).context("make toolchain image read-only")
}

fn hash_regular(path: &Path, expected_size: u64) -> Result<String> {
    let mut file = fs::File::open(path).context("open toolchain image")?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).context("hash toolchain image")?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("image size overflow")?;
        ensure!(size <= expected_size, "toolchain image grew while hashing");
        hasher.update(&buffer[..read]);
    }
    ensure!(size == expected_size, "toolchain image was truncated");
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn sync_parent(parent: &Path) -> Result<()> {
    let directory = fs::File::open(parent).context("open toolchain output parent")?;
    directory.sync_all().context("sync toolchain output parent")
}

fn required_path(arguments: &mut impl Iterator<Item = OsString>, label: &str) -> Result<PathBuf> {
    arguments
        .next()
        .map(PathBuf::from)
        .with_context(|| format!("missing {label}"))
}

fn required_utf8(arguments: &mut impl Iterator<Item = OsString>, label: &str) -> Result<String> {
    arguments
        .next()
        .with_context(|| format!("missing {label}"))?
        .into_string()
        .map_err(|_| anyhow::anyhow!("{label} must be UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_cannot_alias_or_enter_the_untrusted_layout() {
        let layout = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        assert!(validate_output(&outside.path().join("toolchain.ext4"), layout.path()).is_ok());
        assert!(validate_output(&layout.path().join("toolchain.ext4"), layout.path()).is_err());
        assert!(validate_output(Path::new("relative.ext4"), layout.path()).is_err());
    }

    #[test]
    fn ext4_magic_check_fails_closed() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(2_048).unwrap();
        assert!(verify_ext4_magic(file.path()).is_err());
    }

    #[test]
    fn crc32c_matches_the_castagnoli_standard_vector() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn deterministic_vhdx_guids_are_stable_distinct_and_well_formed() {
        let first = deterministic_guid("sha256:fixture", b"file");
        let second = deterministic_guid("sha256:fixture", b"data");
        assert_eq!(first, deterministic_guid("sha256:fixture", b"file"));
        assert_ne!(first, second);
        assert_ne!(first, [0; 16]);
        assert_eq!(first[7] >> 4, 4);
        assert_eq!(first[8] >> 6, 2);
    }

    #[test]
    fn filesystem_uuid_is_derived_from_signed_identity_and_architecture() {
        let identity = format!("sha256:{}", "a".repeat(64));
        let amd64 = filesystem_uuid(&identity, "amd64").unwrap();
        assert_eq!(amd64, filesystem_uuid(&identity, "amd64").unwrap());
        assert_ne!(amd64, filesystem_uuid(&identity, "arm64").unwrap());
        assert_eq!(amd64.get_version_num(), 4);
    }
}
