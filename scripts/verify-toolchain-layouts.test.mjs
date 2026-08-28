import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { hydrateOciLayout, verifyOciLayout } from "./verify-toolchain-layouts.mjs";

function blob(layout, content, mediaType) {
  const bytes = Buffer.from(content);
  const digest = createHash("sha256").update(bytes).digest("hex");
  writeFileSync(join(layout, "blobs", "sha256", digest), bytes);
  return { mediaType, digest: `sha256:${digest}`, size: bytes.length };
}

function fixture(root) {
  const layout = join(root, "layout");
  mkdirSync(join(layout, "blobs", "sha256"), { recursive: true });
  writeFileSync(join(layout, "oci-layout"), JSON.stringify({ imageLayoutVersion: "1.0.0" }));
  const config = blob(
    layout,
    JSON.stringify({ architecture: "amd64", os: "linux" }),
    "application/vnd.oci.image.config.v1+json"
  );
  const layer = blob(layout, "layer-fixture", "application/vnd.oci.image.layer.v1.tar+gzip");
  const manifest = blob(
    layout,
    JSON.stringify({ schemaVersion: 2, config, layers: [layer] }),
    "application/vnd.oci.image.manifest.v1+json"
  );
  writeFileSync(
    join(layout, "index.json"),
    JSON.stringify({ schemaVersion: 2, mediaType: "application/vnd.oci.image.index.v1+json", manifests: [manifest] })
  );
  return { layout, config };
}

test("content-addressed OCI layouts are fully verified", async () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-toolchain-layout-"));
  try {
    const value = fixture(root);
    const result = await verifyOciLayout(value.layout, "amd64");
    assert.equal(result.layers, 1);
    writeFileSync(join(value.layout, "blobs", "sha256", "f".repeat(64)), "unreferenced");
    await assert.rejects(() => verifyOciLayout(value.layout, "amd64"), /unreferenced blobs/u);
    rmSync(join(value.layout, "blobs", "sha256", "f".repeat(64)));
    writeFileSync(
      join(value.layout, "blobs", "sha256", value.config.digest.slice("sha256:".length)),
      "tampered"
    );
    await assert.rejects(() => verifyOciLayout(value.layout, "amd64"), /size differs|digest differs/u);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("shared download blobs are hard-linked into a self-contained OCI layout", async () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-toolchain-hydrate-"));
  try {
    const value = fixture(root);
    const shared = join(root, "shared");
    mkdirSync(join(shared, "sha256"), { recursive: true });
    const blobs = join(value.layout, "blobs", "sha256");
    for (const name of [
      value.config.digest.slice("sha256:".length),
      ...JSON.parse(readFileSync(join(value.layout, "index.json"))).manifests.map((item) => item.digest.slice("sha256:".length))
    ]) {
      renameSync(join(blobs, name), join(shared, "sha256", name));
    }
    const manifest = JSON.parse(readFileSync(join(shared, "sha256", JSON.parse(readFileSync(join(value.layout, "index.json"))).manifests[0].digest.slice("sha256:".length))));
    for (const layer of manifest.layers) {
      const name = layer.digest.slice("sha256:".length);
      renameSync(join(blobs, name), join(shared, "sha256", name));
    }
    hydrateOciLayout(value.layout, shared);
    await verifyOciLayout(value.layout, "amd64");
    rmSync(shared, { recursive: true });
    await verifyOciLayout(value.layout, "amd64");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
