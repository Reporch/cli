#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const MIN_IMAGE_MIB: u64 = 256;
const MAX_IMAGE_MIB: u64 = 32 * 1024;
const MAX_ENTRIES: u64 = 1_000_000;
const MAX_SOURCE_BYTES: u64 = 24 * 1024 * 1024 * 1024;

fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let root = required_path(&mut arguments, "root filesystem directory")?;
    let output = required_path(&mut arguments, "output image")?;
    let size_mib = required_utf8(&mut arguments, "image size in MiB")?
        .parse::<u64>()
        .context("image size must be an integer")?;
    let filesystem_uuid = required_utf8(&mut arguments, "filesystem UUID")?
        .parse::<Uuid>()
        .context("filesystem UUID is invalid")?;
    let mke2fs = required_path(&mut arguments, "absolute mke2fs executable")?;
    let e2fsck = required_path(&mut arguments, "absolute e2fsck executable")?;
    ensure!(arguments.next().is_none(), "too many arguments");

    build_ext4(&root, &output, size_mib, filesystem_uuid, &mke2fs, &e2fsck)?;
    Ok(())
}

fn build_ext4(
    root: &Path,
    output: &Path,
    size_mib: u64,
    filesystem_uuid: Uuid,
    mke2fs: &Path,
    e2fsck: &Path,
) -> Result<String> {
    ensure!(
        (MIN_IMAGE_MIB..=MAX_IMAGE_MIB).contains(&size_mib),
        "toolchain image must be between {MIN_IMAGE_MIB} and {MAX_IMAGE_MIB} MiB"
    );
    let root = canonical_directory(root, "toolchain root")?;
    validate_builder(mke2fs, "mke2fs")?;
    validate_builder(e2fsck, "e2fsck")?;
    let inventory = inventory_root(&root)?;
    let image_bytes = size_mib
        .checked_mul(1024 * 1024)
        .context("toolchain image size overflow")?;
    ensure!(
        image_bytes >= inventory.bytes.saturating_add(128 * 1024 * 1024),
        "toolchain image has insufficient free space for the inventoried root"
    );
    validate_output(output, &root)?;

    let parent = output
        .parent()
        .context("toolchain image output has no parent")?;
    let temporary = parent.join(format!(".toolchain-{}.ext4.tmp", Uuid::now_v7()));
    let result = (|| -> Result<String> {
        let image = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .context("create toolchain image")?;
        image
            .set_len(image_bytes)
            .context("allocate toolchain image")?;
        image.sync_all().context("sync allocated toolchain image")?;
        drop(image);

        run_builder(
            mke2fs,
            &[
                OsString::from("-q"),
                OsString::from("-F"),
                OsString::from("-t"),
                OsString::from("ext4"),
                OsString::from("-L"),
                OsString::from("REPORCH_TC"),
                OsString::from("-U"),
                OsString::from(filesystem_uuid.to_string()),
                OsString::from("-O"),
                OsString::from("^has_journal"),
                OsString::from("-E"),
                OsString::from("root_owner=0:0,lazy_itable_init=0,lazy_journal_init=0"),
                OsString::from("-d"),
                root.as_os_str().to_owned(),
                temporary.as_os_str().to_owned(),
            ],
            "build ext4 toolchain image",
        )?;
        run_builder(
            e2fsck,
            &[
                OsString::from("-f"),
                OsString::from("-n"),
                temporary.as_os_str().to_owned(),
            ],
            "verify ext4 toolchain image",
        )?;
        ensure!(
            fs::metadata(&temporary)?.len() == image_bytes,
            "toolchain builder changed the signed image size"
        );
        let digest = hash_regular(&temporary, image_bytes)?;
        fs::rename(&temporary, output).context("atomically install toolchain image")?;
        sync_parent(parent)?;
        Ok(digest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    let digest = result?;
    println!(
        "{{\"schema\":\"reporch.toolchain-bundle-build.v1\",\"sha256\":\"{digest}\",\"size\":{image_bytes},\"entries\":{},\"source_bytes\":{}}}",
        inventory.entries, inventory.bytes
    );
    Ok(digest)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RootInventory {
    entries: u64,
    bytes: u64,
}

fn inventory_root(root: &Path) -> Result<RootInventory> {
    for required in [
        "bin",
        "etc",
        "dev",
        "proc",
        "run/reporch",
        "tmp",
        "workspace",
    ] {
        let path = root.join(required);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("toolchain root is missing {required}"))?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "toolchain root path must be a real directory: {required}"
        );
    }
    ensure!(
        fs::symlink_metadata(root.join("bin/sh"))
            .context("toolchain root is missing bin/sh")?
            .file_type()
            .is_file()
            || fs::symlink_metadata(root.join("bin/sh"))?
                .file_type()
                .is_symlink(),
        "toolchain root bin/sh must be a file or symlink"
    );

    let mut stack = vec![root.to_owned()];
    let mut inventory = RootInventory {
        entries: 0,
        bytes: 0,
    };
    while let Some(directory) = stack.pop() {
        let mut entries = fs::read_dir(&directory)
            .with_context(|| format!("read toolchain directory {}", directory.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .context("toolchain entry escaped its root")?;
            validate_relative_path(relative)?;
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("inspect toolchain entry {}", relative.display()))?;
            inventory.entries = inventory.entries.checked_add(1).context("entry overflow")?;
            ensure!(
                inventory.entries <= MAX_ENTRIES,
                "toolchain root has too many entries"
            );
            if metadata.is_dir() {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "directory symlink is invalid"
                );
                stack.push(path);
            } else if metadata.is_file() {
                inventory.bytes = inventory
                    .bytes
                    .checked_add(metadata.len())
                    .context("toolchain source size overflow")?;
                ensure!(
                    inventory.bytes <= MAX_SOURCE_BYTES,
                    "toolchain root is too large"
                );
            } else if metadata.file_type().is_symlink() {
                let target = fs::read_link(&path).context("read toolchain symlink")?;
                ensure!(
                    !target.as_os_str().is_empty() && target.as_os_str().len() <= 4_096,
                    "toolchain symlink target is invalid"
                );
            } else {
                anyhow::bail!(
                    "toolchain root contains a device, socket, or FIFO: {}",
                    relative.display()
                );
            }
        }
    }
    Ok(inventory)
}

fn validate_relative_path(path: &Path) -> Result<()> {
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "toolchain path is not normalized"
    );
    let text = path
        .to_str()
        .context("toolchain paths must be portable UTF-8")?;
    ensure!(
        !text.contains('\\') && !text.contains('\0') && text.len() <= 4_096,
        "toolchain path is not portable"
    );
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

fn validate_builder(path: &Path, label: &str) -> Result<()> {
    ensure!(path.is_absolute(), "{label} path must be absolute");
    let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {label}"))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "{label} must be a regular non-symlink executable"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        ensure!(
            metadata.permissions().mode() & 0o111 != 0,
            "{label} is not executable"
        );
    }
    Ok(())
}

fn validate_output(output: &Path, root: &Path) -> Result<()> {
    ensure!(
        output.is_absolute(),
        "toolchain output path must be absolute"
    );
    ensure!(!output.exists(), "toolchain output already exists");
    let parent = canonical_directory(
        output.parent().context("toolchain output has no parent")?,
        "toolchain output parent",
    )?;
    ensure!(
        !parent.starts_with(root),
        "toolchain output must not be inside the source root"
    );
    Ok(())
}

fn run_builder(program: &Path, arguments: &[OsString], label: &str) -> Result<()> {
    let status = Command::new(program)
        .args(arguments)
        .env_clear()
        .env("E2FSPROGS_FAKE_TIME", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("start {label}"))?;
    ensure!(status.success(), "{label} failed with {status}");
    Ok(())
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

    fn minimal_root(path: &Path) {
        for directory in [
            "bin",
            "etc",
            "dev",
            "proc",
            "run/reporch",
            "tmp",
            "workspace",
        ] {
            fs::create_dir_all(path.join(directory)).unwrap();
        }
        fs::write(path.join("bin/sh"), b"static shell fixture").unwrap();
    }

    #[test]
    fn inventory_is_bounded_and_never_follows_symlinks() {
        let root = tempfile::tempdir().unwrap();
        minimal_root(root.path());
        fs::write(root.path().join("etc/config"), b"abc").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("config", root.path().join("etc/current")).unwrap();
        let inventory = inventory_root(root.path()).unwrap();
        assert!(inventory.entries >= 9);
        assert_eq!(inventory.bytes, 23);
    }

    #[cfg(unix)]
    #[test]
    fn inventory_rejects_fifo_and_source_root_symlinks() {
        let root = tempfile::tempdir().unwrap();
        minimal_root(root.path());
        let fifo = root.path().join("tmp/fifo");
        let status = Command::new("/usr/bin/mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success());
        assert!(inventory_root(root.path()).is_err());

        let parent = tempfile::tempdir().unwrap();
        let linked = parent.path().join("linked-root");
        std::os::unix::fs::symlink(root.path(), &linked).unwrap();
        assert!(canonical_directory(&linked, "root").is_err());
    }
}
