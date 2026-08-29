#![forbid(unsafe_code)]

use std::fs;
use std::fs::OpenOptions;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use minisign::{PublicKeyBox, SecretKey, SecretKeyBox, sign};

const MAX_INDEX_BYTES: u64 = 256 * 1024;
const MAX_KEY_BYTES: u64 = 4 * 1024;

fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1).map(PathBuf::from);
    let index_path = arguments.next().context("missing index path")?;
    let secret_path = arguments.next().context("missing secret-key path")?;
    let public_path = arguments.next().context("missing public-key path")?;
    let signature_path = arguments.next().context("missing signature output path")?;
    ensure!(arguments.next().is_none(), "too many arguments");
    ensure!(
        !signature_path.exists(),
        "signature output already exists; remove the exact output before signing"
    );

    let index = read_bounded_regular(&index_path, MAX_INDEX_BYTES, "toolchain index")?;
    let secret = read_bounded_regular(&secret_path, MAX_KEY_BYTES, "secret key")?;
    let public = read_bounded_regular(&public_path, MAX_KEY_BYTES, "public key")?;
    let secret = String::from_utf8(secret).context("secret key must be UTF-8")?;
    let public = String::from_utf8(public).context("public key must be UTF-8")?;
    let secret = open_unencrypted_secret_key(&secret)?;
    let public = PublicKeyBox::from_string(&public)
        .context("decode toolchain public key")?
        .into_public_key()
        .context("open toolchain public key")?;
    let signature = sign(
        Some(&public),
        &secret,
        Cursor::new(&index),
        None,
        Some("Reporch CLI toolchain index"),
    )?;

    let parent = signature_path
        .parent()
        .context("signature output has no parent")?;
    ensure!(
        parent.is_dir(),
        "signature output parent is not a directory"
    );
    let temporary = tempfile_path(parent);
    let write_result = (|| -> Result<()> {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create temporary signature {}", temporary.display()))?;
        output
            .write_all(signature.into_string().as_bytes())
            .context("write temporary signature")?;
        output.sync_all().context("sync temporary signature")?;
        drop(output);
        fs::rename(&temporary, &signature_path).with_context(|| {
            format!("atomically install signature {}", signature_path.display())
        })?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn open_unencrypted_secret_key(encoded: &str) -> Result<SecretKey> {
    SecretKeyBox::from_string(encoded)
        .context("decode toolchain secret key")?
        .into_unencrypted_secret_key()
        .context("open unencrypted toolchain secret key")
}

fn read_bounded_regular(path: &Path, limit: u64, label: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > limit
    {
        bail!("{label} must be a non-empty regular file of at most {limit} bytes");
    }
    let bytes = fs::read(path).with_context(|| format!("read {label} {}", path.display()))?;
    ensure!(
        bytes.len() as u64 == metadata.len(),
        "{label} changed while being read"
    );
    Ok(bytes)
}

fn tempfile_path(parent: &Path) -> PathBuf {
    parent.join(format!(
        ".toolchain-index-signature-{}.tmp",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use minisign::{KeyPair, SecretKeyBox};

    use super::open_unencrypted_secret_key;

    #[test]
    fn opens_the_same_unencrypted_key_format_as_the_runtime_release() {
        let key_pair = KeyPair::generate_unencrypted_keypair().expect("generate key pair");
        let encoded = key_pair
            .sk
            .to_box(Some("Reporch runtime signing key"))
            .expect("box secret key")
            .into_string();

        open_unencrypted_secret_key(&encoded).expect("open unencrypted secret key");

        let parsed = SecretKeyBox::from_string(&encoded).expect("parse secret key box");
        assert!(parsed.into_secret_key(None).is_err());
    }
}
