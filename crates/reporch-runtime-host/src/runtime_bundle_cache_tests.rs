// Regression: special-problem verification re-hashed every runtime artifact before each VM.
// Found by /qa on 2026-09-04.
// Report: .gstack/qa-reports/qa-report-reporch-cli-deep-2026-09-04.md

use std::fs;

use chrono::{Duration, Utc};
use reporch_runtime_core::{
    HostTarget, INSTALLATION_SCHEMA, RUNTIME_SIGNING_KEY_ID, RuntimeArtifactKindV1,
    RuntimeArtifactV1, RuntimeBundleManifestV1, RuntimeInstallationV1,
};

use super::{
    VerifiedRuntimeBundleV1, cache_verified_runtime_bundle, cached_verified_runtime_bundle,
};

fn digest(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

#[test]
fn process_cache_reuses_unchanged_runtime_and_invalidates_artifact_changes() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let target = HostTarget::current().unwrap();
    let directory = root.join("bundles/1-test");
    fs::create_dir_all(&directory).unwrap();
    for (path, contents) in [
        (root.join("current.json"), b"state".as_slice()),
        (directory.join("manifest.json"), b"manifest".as_slice()),
        (
            directory.join("manifest.json.minisig"),
            b"signature".as_slice(),
        ),
        (directory.join(".complete"), b"complete".as_slice()),
        (directory.join("kernel.bin"), b"kernel".as_slice()),
    ] {
        fs::write(path, contents).unwrap();
    }
    let installation = RuntimeInstallationV1 {
        schema: INSTALLATION_SCHEMA.into(),
        sequence: 1,
        version: "test".into(),
        target,
        bundle_sha256: digest('a'),
        installed_at: Utc::now(),
    };
    let verified = VerifiedRuntimeBundleV1 {
        installation: installation.clone(),
        manifest: RuntimeBundleManifestV1 {
            schema: reporch_runtime_core::BUNDLE_MANIFEST_SCHEMA.into(),
            sequence: 1,
            version: "test".into(),
            target,
            backend: target.native_backend(),
            minimum_os_version: "1".into(),
            protocol_min: 1,
            protocol_max: 2,
            generated_at: Utc::now() - Duration::hours(1),
            expires_at: Utc::now() + Duration::hours(1),
            signing_key_id: RUNTIME_SIGNING_KEY_ID.into(),
            artifacts: vec![RuntimeArtifactV1 {
                kind: RuntimeArtifactKindV1::Kernel,
                file_name: "kernel.bin".into(),
                sha256: digest('b'),
                size: 6,
                source_url: "https://example.invalid/kernel".into(),
                sbom_url: "https://example.invalid/sbom".into(),
                provenance_url: "https://example.invalid/provenance".into(),
            }],
        },
        directory: directory.clone(),
    };

    cache_verified_runtime_bundle(root, &verified).unwrap();
    assert!(
        cached_verified_runtime_bundle(root, &installation)
            .unwrap()
            .is_some()
    );

    fs::write(directory.join("kernel.bin"), b"changed").unwrap();
    assert!(
        cached_verified_runtime_bundle(root, &installation)
            .unwrap()
            .is_none(),
        "a changed runtime artifact must never reuse the cached verification"
    );
}
