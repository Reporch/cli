#![forbid(unsafe_code)]

use std::fs::{self, OpenOptions};
use std::io::{Cursor, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use minisign::{KeyPair, PublicKeyBox, SecretKeyBox, sign};

const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_KEY_BYTES: u64 = 4 * 1024;

fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1).map(PathBuf::from);
    let first = arguments
        .next()
        .context("missing manifest path or command")?;
    if first == Path::new("--generate-key") {
        let secret_path = arguments.next().context("missing secret-key output")?;
        let public_path = arguments.next().context("missing public-key output")?;
        ensure!(arguments.next().is_none(), "too many arguments");
        return generate_key_pair(&secret_path, &public_path);
    }
    let manifest_path = first;
    let secret_path = arguments.next().context("missing secret-key path")?;
    let public_path = arguments.next().context("missing public-key path")?;
    let signature_path = arguments.next().context("missing signature output path")?;
    ensure!(arguments.next().is_none(), "too many arguments");
    ensure!(
        !signature_path.exists(),
        "signature output already exists; remove the exact output before signing"
    );

    let manifest = read_bounded_regular(&manifest_path, MAX_MANIFEST_BYTES, "runtime manifest")?;
    let secret = String::from_utf8(read_bounded_regular(
        &secret_path,
        MAX_KEY_BYTES,
        "runtime secret key",
    )?)
    .context("runtime secret key must be UTF-8")?;
    let public = String::from_utf8(read_bounded_regular(
        &public_path,
        MAX_KEY_BYTES,
        "runtime public key",
    )?)
    .context("runtime public key must be UTF-8")?;
    let secret = SecretKeyBox::from_string(&secret)
        .context("decode runtime secret key")?
        .into_unencrypted_secret_key()
        .context("open unencrypted runtime secret key")?;
    let public = PublicKeyBox::from_string(&public)
        .context("decode runtime public key")?
        .into_public_key()
        .context("open runtime public key")?;
    let signature = sign(
        Some(&public),
        &secret,
        Cursor::new(&manifest),
        None,
        Some("Reporch Runtime signed manifest"),
    )?;

    let parent = signature_path
        .parent()
        .context("signature output has no parent")?;
    ensure!(
        parent.is_dir(),
        "signature output parent is not a directory"
    );
    let temporary = parent.join(format!(
        ".runtime-manifest-signature-{}.tmp",
        std::process::id()
    ));
    let write = (|| -> Result<()> {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create temporary signature {}", temporary.display()))?;
        output
            .write_all(signature.into_string().as_bytes())
            .context("write runtime signature")?;
        output.sync_all().context("sync runtime signature")?;
        fs::rename(&temporary, &signature_path).with_context(|| {
            format!(
                "atomically install runtime signature {}",
                signature_path.display()
            )
        })?;
        Ok(())
    })();
    if write.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write
}

fn generate_key_pair(secret_path: &Path, public_path: &Path) -> Result<()> {
    ensure!(
        secret_path.is_absolute() && public_path.is_absolute(),
        "key outputs must be absolute paths"
    );
    ensure!(
        secret_path.parent() == public_path.parent(),
        "key outputs must share one private directory"
    );
    ensure!(
        !secret_path.exists() && !public_path.exists(),
        "key output already exists"
    );
    let parent = secret_path.parent().context("key output has no parent")?;
    let metadata = fs::symlink_metadata(parent).context("inspect key output directory")?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "key output directory must be a real directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "key output directory must not be accessible by group or other users"
        );
    }
    let KeyPair { pk, sk } = KeyPair::generate_unencrypted_keypair()?;
    let public = pk.to_box()?.into_string();
    let secret = sk
        .to_box(Some("minisign secret key for Reporch Runtime manifests"))?
        .into_string();
    write_secret_create_new(secret_path, secret.as_bytes())?;
    if let Err(error) = write_public_create_new(public_path, public.as_bytes()) {
        let _ = fs::remove_file(secret_path);
        return Err(error);
    }
    Ok(())
}

fn write_secret_create_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).context("create runtime secret key")?;
    file.write_all(bytes).context("write runtime secret key")?;
    file.sync_all().context("sync runtime secret key")
}

fn write_public_create_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .context("create runtime public key")?;
    file.write_all(bytes).context("write runtime public key")?;
    file.sync_all().context("sync runtime public key")
}

fn read_bounded_regular(path: &Path, limit: u64, label: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() > 0
            && metadata.len() <= limit,
        "{label} must be a non-empty regular non-symlink file of at most {limit} bytes"
    );
    let bytes = fs::read(path).with_context(|| format!("read {label} {}", path.display()))?;
    ensure!(
        bytes.len() as u64 == metadata.len(),
        "{label} changed while being read"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_reader_rejects_empty_input() {
        let root = std::env::temp_dir().join(format!("runtime-signer-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let empty = root.join("empty");
        fs::write(&empty, []).unwrap();
        assert!(read_bounded_regular(&empty, 10, "fixture").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_reader_rejects_symlinked_input() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "runtime-signer-symlink-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let target = root.join("target");
        let link = root.join("link");
        fs::write(&target, b"data").unwrap();
        symlink(&target, &link).unwrap();
        assert!(read_bounded_regular(&link, 10, "fixture").is_err());
        let _ = fs::remove_dir_all(root);
    }
}
