# Toolchain index and VM bundle signing

RC8 has two deliberately separate catalogs.

- `artifacts/toolchains-v1.json` is the embedded, signed compatibility catalog
  for users who explicitly select the deprecated Docker or Podman backend.
- `toolchains-v2-index.json` is the expiring runtime-channel catalog used by
  the default Reporch VM backend. It is downloaded from the immutable
  `reporch-toolchains-v2-seq8` prerelease and verified with the same offline
  trust root as the base VM runtime (`FF2F931B66DAA966`). Runtime and toolchain
  channels are separate because a published immutable GitHub release cannot
  accept additional assets.

Neither private key is stored in the repository or release artifacts.

## V2 source and build contract

`runtime/toolchains.lock.json` pins all twelve official Studio OCI images by
multi-architecture SHA-256 digest. The release pipeline accesses them
anonymously, materializes both `linux/arm64` and `linux/amd64` as
content-addressed OCI layouts, and verifies every manifest, config, and layer
before conversion.

The pure-Rust converter creates a deterministic ext4 image with a filesystem
UUID derived from the signed OCI identity and architecture. macOS and Linux
share that read-only image for a matching architecture. Windows receives a
fixed VHDX containing the same x64 filesystem; random QEMU header, log, and
virtual-disk identifiers are normalized, CRC32C is recomputed, and QEMU checks
both structure and logical equality with the source ext4.

Images are zstd-compressed with fixed settings. The signed index binds both
compressed and expanded sizes and SHA-256 digests. Installation streams through
a bounded pure-Rust decoder and atomically publishes only an exact expanded
match. A lower sequence, reused sequence with different bytes, expired index,
unknown key, changed archive, decompression overflow, or image mismatch fails
closed.

Pinned Syft 1.51.0 inventories packages and declared licenses from each verified
OCI layout. Host paths, random document identifiers, and wall-clock timestamps
are normalized before the SPDX document is accepted. Every released archive is
also bound to deterministic SLSA provenance, the source lock, OCI digest, Git
revision, and normalized SBOM digest.

## Release procedure

The zero-cost self-hosted `Release Reporch VM Toolchains` workflow:

1. verifies pinned Skopeo, QEMU, Syft, Rust, Node, the source lock, and a clean
   checkout;
2. materializes and hashes both OCI architectures without using Docker
   credentials;
3. builds all ext4 and VHDX bundles twice in independent directories;
4. requires byte-identical modes, sizes, contents, SPDX, provenance, and index;
5. signs the V2 index using `REPORCH_RUNTIME_SIGNING_KEY` and trusted signer
   code from `main`;
6. creates a draft, attests and uploads the flat assets, verifies the complete
   draft, and only then publishes `reporch-toolchains-v2-seq8` immutably;
7. removes Docker environment variables and installs `bash-5.3` through the
   public updater as the final channel check.

The source lock sequence must increase for any catalog change. Key rotation uses
an overlap release: ship clients that trust both keys, rotate the protected
secret, then remove the old trust root only after the supported-client window.
