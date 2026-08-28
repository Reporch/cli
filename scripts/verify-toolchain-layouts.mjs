import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { createReadStream, existsSync, linkSync, lstatSync, mkdirSync, readFileSync, readdirSync } from "node:fs";
import { basename, join, resolve } from "node:path";
import { pipeline } from "node:stream/promises";
import { Transform } from "node:stream";
import { pathToFileURL } from "node:url";

import { readToolchainLock } from "./check-toolchain-sources.mjs";

const MAX_BLOB_BYTES = 4 * 1024 * 1024 * 1024;

function regular(path, maximum, label) {
  const stat = lstatSync(path);
  assert.ok(stat.isFile() && !stat.isSymbolicLink(), `${label} must be a regular non-symlink file`);
  assert.ok(stat.size > 0 && stat.size <= maximum, `${label} exceeds its byte bound`);
  return stat;
}

async function sha256(path) {
  const hash = createHash("sha256");
  await pipeline(
    createReadStream(path),
    new Transform({
      transform(chunk, _encoding, callback) {
        hash.update(chunk);
        callback();
      }
    })
  );
  return hash.digest("hex");
}

async function descriptor(layout, descriptor, label) {
  assert.deepEqual(Object.keys(descriptor).sort(), ["digest", "mediaType", "size"]);
  assert.match(descriptor.digest, /^sha256:[a-f0-9]{64}$/u);
  assert.ok(Number.isSafeInteger(descriptor.size) && descriptor.size > 0 && descriptor.size <= MAX_BLOB_BYTES);
  const digest = descriptor.digest.slice("sha256:".length);
  const path = join(layout, "blobs", "sha256", digest);
  const stat = regular(path, MAX_BLOB_BYTES, label);
  assert.equal(stat.size, descriptor.size, `${label} size differs from its descriptor`);
  assert.equal(await sha256(path), digest, `${label} digest differs from its content address`);
  return path;
}

function hydrateDescriptor(layout, sharedBlobs, descriptorValue, label) {
  assert.match(descriptorValue.digest, /^sha256:[a-f0-9]{64}$/u);
  assert.ok(
    Number.isSafeInteger(descriptorValue.size) && descriptorValue.size > 0 && descriptorValue.size <= MAX_BLOB_BYTES,
    `${label} exceeds its byte bound`
  );
  const digest = descriptorValue.digest.slice("sha256:".length);
  const destination = join(layout, "blobs", "sha256", digest);
  if (!existsSync(destination)) {
    const source = join(sharedBlobs, "sha256", digest);
    const stat = regular(source, MAX_BLOB_BYTES, `shared ${label}`);
    assert.equal(stat.size, descriptorValue.size, `shared ${label} size differs from its descriptor`);
    mkdirSync(join(layout, "blobs", "sha256"), { recursive: true, mode: 0o700 });
    linkSync(source, destination);
  }
  return destination;
}

export function hydrateOciLayout(layoutArgument, sharedBlobsArgument) {
  const layout = resolve(layoutArgument);
  const sharedBlobs = resolve(sharedBlobsArgument);
  const root = lstatSync(layout);
  const shared = lstatSync(sharedBlobs);
  assert.ok(root.isDirectory() && !root.isSymbolicLink(), "OCI layout root must be a real directory");
  assert.ok(shared.isDirectory() && !shared.isSymbolicLink(), "shared blob root must be a real directory");
  const index = JSON.parse(readFileSync(join(layout, "index.json")));
  assert.ok(Array.isArray(index.manifests) && index.manifests.length === 1);
  const manifestPath = hydrateDescriptor(layout, sharedBlobs, index.manifests[0], "OCI manifest");
  const manifest = JSON.parse(readFileSync(manifestPath));
  assert.ok(Array.isArray(manifest.layers) && manifest.layers.length > 0 && manifest.layers.length <= 512);
  hydrateDescriptor(layout, sharedBlobs, manifest.config, "OCI config");
  for (const [indexValue, layer] of manifest.layers.entries()) {
    hydrateDescriptor(layout, sharedBlobs, layer, `OCI layer ${indexValue}`);
  }
}

export async function verifyOciLayout(layoutArgument, architecture) {
  assert.ok(["amd64", "arm64"].includes(architecture));
  const layout = resolve(layoutArgument);
  const root = lstatSync(layout);
  assert.ok(root.isDirectory() && !root.isSymbolicLink(), "OCI layout root must be a real directory");
  assert.deepEqual(JSON.parse(readFileSync(join(layout, "oci-layout"))), { imageLayoutVersion: "1.0.0" });
  const index = JSON.parse(readFileSync(join(layout, "index.json")));
  assert.equal(index.schemaVersion, 2);
  assert.ok(Array.isArray(index.manifests) && index.manifests.length === 1);
  const manifestPath = await descriptor(layout, index.manifests[0], "OCI manifest");
  const manifest = JSON.parse(readFileSync(manifestPath));
  assert.equal(manifest.schemaVersion, 2);
  assert.ok(Array.isArray(manifest.layers) && manifest.layers.length > 0 && manifest.layers.length <= 512);
  const configPath = await descriptor(layout, manifest.config, "OCI config");
  const config = JSON.parse(readFileSync(configPath));
  assert.equal(config.os, "linux");
  assert.equal(config.architecture, architecture);
  const referencedBlobs = new Set([basename(manifestPath), basename(configPath)]);
  for (const [indexValue, layer] of manifest.layers.entries()) {
    const layerPath = await descriptor(layout, layer, `OCI layer ${indexValue}`);
    referencedBlobs.add(basename(layerPath));
  }
  const algorithms = readdirSync(join(layout, "blobs"));
  assert.deepEqual(algorithms, ["sha256"]);
  const presentBlobs = readdirSync(join(layout, "blobs", "sha256")).sort();
  assert.deepEqual(presentBlobs, [...referencedBlobs].sort(), "OCI layout contains unreferenced blobs");
  return { architecture, manifest: index.manifests[0].digest, layers: manifest.layers.length };
}

export async function verifyToolchainLayouts(lockArgument, rootArgument) {
  const source = readToolchainLock(lockArgument);
  const root = resolve(rootArgument);
  const rootStat = lstatSync(root);
  assert.ok(rootStat.isDirectory() && !rootStat.isSymbolicLink(), "toolchain source cache must be a real directory");
  const cachedLock = join(root, basename(source.path));
  regular(cachedLock, 64 * 1024, "cached toolchain lock");
  assert.equal(await sha256(cachedLock), source.sha256, "cached toolchain lock changed");
  const images = new Map();
  for (const entry of source.lock.entries) {
    const digest = entry.image.slice(entry.image.lastIndexOf("sha256:") + "sha256:".length);
    images.set(digest, entry.image);
  }
  const records = [];
  for (const digest of [...images.keys()].sort()) {
    for (const architecture of ["amd64", "arm64"]) {
      records.push(
        await verifyOciLayout(join(root, "layouts", digest, architecture), architecture)
      );
    }
  }
  return { schema: "reporch.toolchain-layout-verification.v1", images: images.size, layouts: records.length };
}

async function main() {
  const [lock, root, ...extra] = process.argv.slice(2);
  assert.ok(lock && root && extra.length === 0, "usage: node scripts/verify-toolchain-layouts.mjs <lock> <cache>");
  process.stdout.write(`${JSON.stringify(await verifyToolchainLayouts(lock, root))}\n`);
}

const invoked = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : null;
if (invoked === import.meta.url) await main();
