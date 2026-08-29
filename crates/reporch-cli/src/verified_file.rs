use std::fs;
use std::io::{Read, Seek as _, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use cap_std::ambient_authority;
use sha2::{Digest, Sha256};

pub fn open_root(root: &Path) -> Result<cap_std::fs::Dir> {
    let absolute = std::path::absolute(root)?;
    let parent_path = absolute
        .parent()
        .context("verified file root must have a parent")?;
    let name = absolute
        .file_name()
        .context("verified file root cannot be a filesystem root")?;
    let parent = cap_std::fs::Dir::open_ambient_dir(parent_path, ambient_authority())
        .with_context(|| format!("open verified file parent {}", parent_path.display()))?;
    let parent_file = parent.try_clone()?.into_std_file();
    let root = cap_primitives::fs::open_dir_nofollow(&parent_file, Path::new(name))
        .context("open verified file root without following a symlink")?;
    Ok(cap_std::fs::Dir::from_std_file(root))
}

pub fn snapshot_verified(
    root: &cap_std::fs::Dir,
    relative: &str,
    expected_size: u64,
    expected_digest: &str,
) -> Result<fs::File> {
    let mut snapshot = tempfile::tempfile().context("create private verified file snapshot")?;
    copy_verified(
        root,
        relative,
        expected_size,
        expected_digest,
        &mut snapshot,
    )?;
    snapshot
        .seek(SeekFrom::Start(0))
        .context("rewind verified file snapshot")?;
    Ok(snapshot)
}

pub fn copy_verified(
    root: &cap_std::fs::Dir,
    relative: &str,
    expected_size: u64,
    expected_digest: &str,
    destination: &mut impl Write,
) -> Result<()> {
    let mut source = root
        .open(relative)
        .with_context(|| format!("open verified source {relative}"))?
        .into_std();
    let metadata = source
        .metadata()
        .with_context(|| format!("inspect verified source {relative}"))?;
    ensure!(
        metadata.is_file() && metadata.len() == expected_size,
        "source file size changed or type is invalid: {relative}"
    );
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .with_context(|| format!("read verified source {relative}"))?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("source size overflow")?;
        ensure!(
            size <= expected_size,
            "source file size changed: {relative}"
        );
        digest.update(&buffer[..read]);
        destination
            .write_all(&buffer[..read])
            .with_context(|| format!("copy verified source {relative}"))?;
    }
    ensure!(
        size == expected_size,
        "source file size changed: {relative}"
    );
    ensure!(
        hex::encode(digest.finalize()) == expected_digest,
        "source file digest changed: {relative}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_snapshot_is_not_affected_by_a_later_path_swap() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("source"), b"original").unwrap();
        let root = open_root(temporary.path()).unwrap();
        let digest = hex::encode(Sha256::digest(b"original"));
        let mut snapshot = snapshot_verified(&root, "source", 8, &digest).unwrap();
        fs::write(temporary.path().join("source"), b"replaced").unwrap();
        let mut bytes = Vec::new();
        snapshot.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
    }

    #[cfg(unix)]
    #[test]
    fn capability_root_rejects_a_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), b"secret").unwrap();
        symlink(outside.path(), temporary.path().join("source")).unwrap();
        let root = open_root(temporary.path()).unwrap();
        let digest = hex::encode(Sha256::digest(b"secret"));
        assert!(snapshot_verified(&root, "source", 6, &digest).is_err());
    }
}
