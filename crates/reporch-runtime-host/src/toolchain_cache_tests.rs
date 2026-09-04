// Regression: repeated special-problem checks re-hashed the full toolchain image for every VM.
// Found by /qa on 2026-09-04.
// Report: .gstack/qa-reports/qa-report-reporch-cli-deep-2026-09-04.md

use std::fs;

use chrono::{Duration, Utc};
use reporch_runtime_core::{
    HostTarget, TOOLCHAIN_INSTALLATION_SCHEMA, ToolchainBundleV2, ToolchainCompressionV2,
    ToolchainEntryV2, ToolchainFilesystemV2, ToolchainInstallationV2,
};

use super::{
    VerifiedToolchainBundleV2, cache_verified_toolchain, cached_verified_toolchain,
    reuse_installed_toolchain, toolchain_state_path,
};

fn digest(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

#[test]
fn process_cache_reuses_unchanged_toolchain_and_invalidates_changes() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let id = "python-3.13";
    let target = HostTarget::current().unwrap();
    let bundle_digest = digest('a');
    let image_name = "toolchain.ext4";
    let directory = root
        .join("bundles")
        .join(id)
        .join(format!("1-{}", bundle_digest.trim_start_matches("sha256:")));
    fs::create_dir_all(root.join("installed")).unwrap();
    fs::create_dir_all(&directory).unwrap();
    fs::write(toolchain_state_path(root, id), b"state").unwrap();
    fs::write(directory.join("index.json"), b"index").unwrap();
    fs::write(directory.join("index.json.minisig"), b"signature").unwrap();
    fs::write(directory.join(image_name), b"image").unwrap();

    let bundle = ToolchainBundleV2 {
        target,
        filesystem: if cfg!(windows) {
            ToolchainFilesystemV2::Vhdx
        } else {
            ToolchainFilesystemV2::Ext4
        },
        file_name: image_name.into(),
        sha256: bundle_digest.clone(),
        size: 5,
        archive_file_name: "toolchain.ext4.zst".into(),
        archive_sha256: digest('b'),
        archive_size: 1,
        compression: ToolchainCompressionV2::Zstd,
        source_url: "https://example.invalid/toolchain".into(),
        sbom_url: "https://example.invalid/sbom".into(),
        provenance_url: "https://example.invalid/provenance".into(),
    };
    let entry = ToolchainEntryV2 {
        id: id.into(),
        language: "python".into(),
        toolchain_lock_sha256: digest('c'),
        studio_oci_image: format!("example.invalid/python@{}", digest('d')),
        bundles: vec![bundle.clone()],
    };
    let verified = VerifiedToolchainBundleV2 {
        installation: ToolchainInstallationV2 {
            schema: TOOLCHAIN_INSTALLATION_SCHEMA.into(),
            index_sequence: 1,
            id: id.into(),
            target,
            toolchain_lock_sha256: entry.toolchain_lock_sha256.clone(),
            bundle_sha256: bundle_digest,
            file_name: image_name.into(),
            installed_at: Utc::now(),
        },
        entry,
        bundle,
        path: directory.join(image_name),
    };

    cache_verified_toolchain(root, id, target, &verified, Utc::now() + Duration::hours(1)).unwrap();
    assert!(
        cached_verified_toolchain(root, id, target)
            .unwrap()
            .is_some()
    );

    fs::write(&verified.path, b"changed").unwrap();
    assert!(
        cached_verified_toolchain(root, id, target)
            .unwrap()
            .is_none(),
        "a changed image must never reuse the cached verification"
    );
}

#[cfg(unix)]
#[test]
fn an_installed_verified_toolchain_is_reused_without_a_channel_refresh() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let id = "python-3.13-offline";
    let target = HostTarget::current().unwrap();
    let bundle_digest = digest('e');
    let image_name = "toolchain.ext4";
    let directory = root
        .join("bundles")
        .join(id)
        .join(format!("1-{}", bundle_digest.trim_start_matches("sha256:")));
    fs::create_dir_all(root.join("installed")).unwrap();
    fs::create_dir_all(&directory).unwrap();
    fs::write(toolchain_state_path(root, id), b"state").unwrap();
    for (name, bytes) in [
        ("index.json", b"index".as_slice()),
        ("index.json.minisig", b"signature".as_slice()),
        (image_name, b"image".as_slice()),
    ] {
        let path = directory.join(name);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o400)).unwrap();
    }

    let bundle = ToolchainBundleV2 {
        target,
        filesystem: ToolchainFilesystemV2::Ext4,
        file_name: image_name.into(),
        sha256: bundle_digest.clone(),
        size: 5,
        archive_file_name: "toolchain.ext4.zst".into(),
        archive_sha256: digest('f'),
        archive_size: 1,
        compression: ToolchainCompressionV2::Zstd,
        source_url: "https://unreachable.invalid/toolchain".into(),
        sbom_url: "https://unreachable.invalid/sbom".into(),
        provenance_url: "https://unreachable.invalid/provenance".into(),
    };
    let entry = ToolchainEntryV2 {
        id: id.into(),
        language: "python".into(),
        toolchain_lock_sha256: digest('1'),
        studio_oci_image: format!("example.invalid/python@{}", digest('2')),
        bundles: vec![bundle.clone()],
    };
    let verified = VerifiedToolchainBundleV2 {
        installation: ToolchainInstallationV2 {
            schema: TOOLCHAIN_INSTALLATION_SCHEMA.into(),
            index_sequence: 1,
            id: id.into(),
            target,
            toolchain_lock_sha256: entry.toolchain_lock_sha256.clone(),
            bundle_sha256: bundle_digest,
            file_name: image_name.into(),
            installed_at: Utc::now(),
        },
        entry,
        bundle,
        path: directory.join(image_name),
    };
    cache_verified_toolchain(root, id, target, &verified, Utc::now() + Duration::hours(1)).unwrap();

    let reused = reuse_installed_toolchain(root, id, target)
        .unwrap()
        .expect("the verified installed toolchain should be preferred over its offline URL");
    assert_eq!(reused.installation, verified.installation);
    assert_eq!(reused.path, verified.path);
}
