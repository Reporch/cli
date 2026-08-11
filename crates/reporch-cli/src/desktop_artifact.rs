use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use clap::Args as ClapArgs;
use minisign_verify::{Error as MinisignError, PublicKey, Signature};
use sha2::{Digest, Sha256};

const MAX_ARTIFACT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_LEGACY_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_KEY_BYTES: usize = 4 * 1024;
const MAX_SIGNATURE_BYTES: usize = 4 * 1024;

#[derive(Debug, ClapArgs)]
pub struct VerifyDesktopArtifactOptions {
    #[arg(long)]
    artifact: PathBuf,
    #[arg(long, env = "REPORCH_STUDIO_UPDATER_SIGNATURE")]
    signature: String,
    #[arg(long, env = "REPORCH_STUDIO_UPDATER_PUBLIC_KEY")]
    public_key: String,
}

pub fn verify(options: &VerifyDesktopArtifactOptions) -> Result<()> {
    verify_with_schema(options, "reporch.desktop-artifact-verification.v1")
}

pub fn verify_signed_artifact(options: &VerifyDesktopArtifactOptions) -> Result<()> {
    verify_with_schema(options, "reporch.signed-artifact-verification.v1")
}

fn verify_with_schema(options: &VerifyDesktopArtifactOptions, schema: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(&options.artifact)
        .with_context(|| format!("inspect updater artifact {}", options.artifact.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_ARTIFACT_BYTES
    {
        bail!("updater artifact must be a non-empty regular file of at most 4 GiB");
    }

    let public_key_text = decode_bounded(&options.public_key, MAX_KEY_BYTES, "public key")?;
    let public_key = PublicKey::decode(&public_key_text).context("decode Minisign public key")?;
    let signature_text = decode_bounded(&options.signature, MAX_SIGNATURE_BYTES, "signature")?;
    let signature = Signature::decode(&signature_text).context("decode Minisign signature")?;
    let (bytes, digest) = match public_key.verify_stream(&signature) {
        Ok(mut verifier) => {
            let mut hasher = Sha256::new();
            let mut file = File::open(&options.artifact)
                .with_context(|| format!("open updater artifact {}", options.artifact.display()))?;
            let mut buffer = [0_u8; 64 * 1024];
            let mut bytes = 0_u64;
            loop {
                let read = file.read(&mut buffer).context("read updater artifact")?;
                if read == 0 {
                    break;
                }
                bytes = bytes
                    .checked_add(read as u64)
                    .context("updater artifact length overflow")?;
                if bytes > MAX_ARTIFACT_BYTES {
                    bail!("updater artifact grew beyond 4 GiB while being verified");
                }
                verifier.update(&buffer[..read]);
                hasher.update(&buffer[..read]);
            }
            if bytes != metadata.len() {
                bail!("updater artifact changed while being verified");
            }
            verifier
                .finalize()
                .context("updater artifact signature verification failed")?;
            (bytes, hasher.finalize().to_vec())
        }
        Err(MinisignError::UnsupportedLegacyMode) => {
            if metadata.len() > MAX_LEGACY_ARTIFACT_BYTES {
                bail!("legacy updater signatures are limited to 512 MiB artifacts");
            }
            let contents = std::fs::read(&options.artifact)
                .with_context(|| format!("read updater artifact {}", options.artifact.display()))?;
            if contents.len() as u64 != metadata.len() {
                bail!("updater artifact changed while being verified");
            }
            public_key
                .verify(&contents, &signature, true)
                .context("legacy updater artifact signature verification failed")?;
            (metadata.len(), Sha256::digest(&contents).to_vec())
        }
        Err(error) => return Err(error).context("initialize streaming Minisign verification"),
    };

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": schema,
            "size_bytes": bytes,
            "sha256": hex::encode(digest),
            "minisign_verified": true,
            "passed": true,
        }))?
    );
    Ok(())
}

fn decode_bounded(encoded: &str, limit: usize, label: &str) -> Result<String> {
    if encoded.is_empty() || encoded.len() > limit * 2 {
        bail!("{label} encoding has an invalid length");
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .with_context(|| format!("{label} is not standard base64"))?;
    if decoded.is_empty() || decoded.len() > limit {
        bail!("{label} exceeds its decoded size limit");
    }
    String::from_utf8(decoded).with_context(|| format!("{label} is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBLIC_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXkKUldRZjZMUkNHQTlpNTNtbFllY080SXpUNTFUR1Bwdld1Y05TQ2gxQ0JNMFFUYUxuNzNZN0dGTzM=";
    const SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIG1pbmlzaWduIHNlY3JldCBrZXkKUldRZjZMUkNHQTlpNTlTTE9GeHo2Tnh2QVNYREplUnR1Wnlrd1FlcGJERUd0ODdpZzFCTnBXYVZXdU5ybTczWWlJaUpicTcxV2krZFA5ZUtMOE9DMzUxdndJYXNTU2JYeHdBPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNTU1Nzc5OTY2CWZpbGU6dGVzdApRdEtNWFd5WWN3ZHBaQWxQRjd0RTJFTkprUmQxdWp2S2psajFtOVJ0SFRCblpQYTVXS1U1dVdSczVHb1A1TS9WcUU4MVFGdU1LSTVrL1NmTlFVYU9BQT09";

    #[test]
    fn verifies_the_known_updater_fixture_and_rejects_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let artifact = directory.path().join("artifact.bin");
        std::fs::write(&artifact, b"test").unwrap();
        let options = VerifyDesktopArtifactOptions {
            artifact: artifact.clone(),
            signature: SIGNATURE.into(),
            public_key: PUBLIC_KEY.into(),
        };
        verify(&options).unwrap();

        std::fs::write(&artifact, b"changed").unwrap();
        assert!(verify(&options).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_artifacts() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let artifact = directory.path().join("artifact");
        std::fs::write(&target, b"test").unwrap();
        symlink(&target, &artifact).unwrap();
        assert!(
            verify(&VerifyDesktopArtifactOptions {
                artifact,
                signature: SIGNATURE.into(),
                public_key: PUBLIC_KEY.into(),
            })
            .is_err()
        );
    }
}
