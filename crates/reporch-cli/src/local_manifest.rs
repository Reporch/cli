use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use studio_core::ReleaseManifestV1;

pub(crate) fn verify_files(manifest_path: &Path, manifest: &ReleaseManifestV1) -> Result<()> {
    let source_root = manifest_directory(manifest_path)?;
    let source_root = fs::canonicalize(source_root)
        .with_context(|| format!("resolve manifest directory {}", source_root.display()))?;
    ensure!(
        source_root.is_dir(),
        "manifest directory is not a directory"
    );

    for expected in &manifest.files {
        verify_file(
            &source_root,
            &expected.path,
            expected.size_bytes,
            expected.sha256.as_str(),
        )?;
    }
    Ok(())
}

fn manifest_directory(manifest_path: &Path) -> Result<&Path> {
    let parent = manifest_path
        .parent()
        .context("manifest path has no parent directory")?;
    if parent.as_os_str().is_empty() {
        Ok(Path::new("."))
    } else {
        Ok(parent)
    }
}

fn verify_file(
    source_root: &Path,
    relative_path: &str,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<()> {
    let source = source_root.join(relative_path);
    let symlink_metadata = fs::symlink_metadata(&source)
        .with_context(|| format!("inspect manifest file {relative_path}"))?;
    ensure!(
        symlink_metadata.file_type().is_file() && !symlink_metadata.file_type().is_symlink(),
        "manifest file is not a regular file: {relative_path}"
    );

    let canonical_source = fs::canonicalize(&source)
        .with_context(|| format!("resolve manifest file {relative_path}"))?;
    let expected_source = absolute_lexical_path(source_root, relative_path);
    ensure!(
        canonical_source.starts_with(source_root) && canonical_source == expected_source,
        "manifest file resolves through a symlink or outside the project: {relative_path}"
    );

    let mut file = File::open(&canonical_source)
        .with_context(|| format!("open manifest file {relative_path}"))?;
    let initial_size = file.metadata()?.len();
    ensure!(
        initial_size == expected_size,
        "manifest file size mismatch: {relative_path} (expected {expected_size}, got {initial_size})"
    );

    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut actual_size = 0_u64;
    let mut hasher = Sha256::new();
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read manifest file {relative_path}"))?;
        if read == 0 {
            break;
        }
        actual_size = actual_size
            .checked_add(read as u64)
            .context("manifest file size overflow")?;
        ensure!(
            actual_size <= expected_size,
            "manifest file grew while hashing: {relative_path}"
        );
        hasher.update(&buffer[..read]);
    }
    let actual_sha256 = hex::encode(hasher.finalize());
    ensure!(
        actual_size == expected_size,
        "manifest file size changed while hashing: {relative_path}"
    );
    ensure!(
        actual_sha256 == expected_sha256,
        "manifest file digest mismatch: {relative_path}"
    );
    Ok(())
}

fn absolute_lexical_path(source_root: &Path, relative_path: &str) -> PathBuf {
    source_root.join(relative_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_core::ReleaseManifestV1;
    use uuid::Uuid;

    fn generated_project() -> (tempfile::TempDir, PathBuf, ReleaseManifestV1) {
        let temporary = tempfile::tempdir().unwrap();
        reporch_cli::init_project_with_id(temporary.path(), "Integrity", Uuid::now_v7()).unwrap();
        let manifest_path = temporary.path().join("reporch.problem.json");
        let manifest = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        (temporary, manifest_path, manifest)
    }

    #[test]
    fn accepts_files_that_match_the_manifest() {
        let (_temporary, manifest_path, manifest) = generated_project();
        verify_files(&manifest_path, &manifest).unwrap();
    }

    #[test]
    fn resolves_a_bare_manifest_filename_from_the_current_directory() {
        assert_eq!(
            manifest_directory(Path::new("reporch.problem.json")).unwrap(),
            Path::new(".")
        );
    }

    #[test]
    fn rejects_a_tampered_file() {
        let (temporary, manifest_path, manifest) = generated_project();
        fs::write(temporary.path().join("statements/ko.md"), b"tampered\n").unwrap();

        let error = verify_files(&manifest_path, &manifest).unwrap_err();
        assert!(error.to_string().contains("size mismatch"));
        assert!(error.to_string().contains("statements/ko.md"));
    }

    #[test]
    fn rejects_a_missing_file() {
        let (temporary, manifest_path, manifest) = generated_project();
        fs::remove_file(temporary.path().join("tests/1.in")).unwrap();

        let error = verify_files(&manifest_path, &manifest).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("inspect manifest file tests/1.in")
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlinked_file() {
        use std::os::unix::fs::symlink;

        let (temporary, manifest_path, manifest) = generated_project();
        let statement = temporary.path().join("statements/ko.md");
        let replacement = temporary.path().join("statement-copy.md");
        fs::copy(&statement, &replacement).unwrap();
        fs::remove_file(&statement).unwrap();
        symlink(&replacement, &statement).unwrap();

        let error = verify_files(&manifest_path, &manifest).unwrap_err();
        assert!(error.to_string().contains("not a regular file"));
    }
}
