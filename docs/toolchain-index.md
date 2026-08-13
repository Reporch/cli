# Toolchain index signing

`artifacts/toolchains-v1.json` is parsed only after its embedded Minisign
signature verifies. The private key is not stored in the repository or release
artifacts. GitHub stores it as the `REPORCH_TOOLCHAIN_INDEX_SIGNING_KEY`
repository secret; the CLI embeds only the public key.

To update the catalog:

1. Create a same-repository branch from `main`.
2. Change only `artifacts/toolchains-v1.json` and push the branch.
3. Run the `Sign toolchain index` workflow from `main`, passing that branch.
4. The trusted workflow checks that the unsigned diff contains exactly that one
   file, signs it with signer code checked out separately from `main`, removes
   the secret material, runs the full candidate test suite, and pushes only the
   signature commit to the candidate branch.
5. Review the image sources, licenses, architectures, and immutable digests
   before merging. Never accept mutable tags without `@sha256:`.

Key rotation requires an overlap release: add a second embedded public key,
release clients that trust both keys, move the repository secret to the new
private key, sign the next index with the new key, then remove the old public
key only after the supported-client window has elapsed.
