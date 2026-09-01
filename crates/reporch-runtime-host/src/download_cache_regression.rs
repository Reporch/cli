use std::fs;

use sha2::{Digest, Sha256};

use super::{promote_completed_download, resumable_file_size};

#[test]
fn completed_partial_is_atomically_promoted_to_a_non_partial_cache_entry() {
    let root = tempfile::tempdir().unwrap();
    let partial = root.path().join("asset.part");
    let completed = root.path().join("asset.blob");
    let bytes = b"verified runtime artifact";
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(bytes)));
    fs::write(&partial, bytes).unwrap();

    promote_completed_download(&partial, &completed, bytes.len() as u64, &digest).unwrap();

    assert!(!partial.exists());
    assert_eq!(fs::read(&completed).unwrap(), bytes);
    assert_eq!(
        resumable_file_size(&completed, bytes.len() as u64, &digest).unwrap(),
        bytes.len() as u64
    );
}

#[test]
fn incomplete_partial_is_never_promoted() {
    let root = tempfile::tempdir().unwrap();
    let partial = root.path().join("asset.part");
    let completed = root.path().join("asset.blob");
    let expected = b"verified runtime artifact";
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(expected)));
    fs::write(&partial, &expected[..5]).unwrap();

    let error = promote_completed_download(&partial, &completed, expected.len() as u64, &digest)
        .unwrap_err();

    assert!(error.to_string().contains("not complete"), "{error:#}");
    assert!(partial.exists());
    assert!(!completed.exists());
}
