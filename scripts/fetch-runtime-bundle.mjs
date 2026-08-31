import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  closeSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  renameSync,
  rmSync,
  writeSync
} from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const CHANNEL = "https://github.com/Reporch/cli/releases/download/reporch-runtime-v1-seq13/";
const TARGETS = new Set([
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64-gnu",
  "linux-x64-gnu",
  "windows-x64-msvc"
]);
const MAX_MANIFEST = 256 * 1024;
const MAX_SIGNATURE = 16 * 1024;
const MAX_ARTIFACT = 4 * 1024 * 1024 * 1024;

function safeUrl(value, label) {
  const url = new URL(value);
  assert.equal(url.protocol, "https:", `${label} must use HTTPS`);
  assert.equal(url.username, "", `${label} must not contain credentials`);
  assert.equal(url.password, "", `${label} must not contain credentials`);
  assert.equal(url.search, "", `${label} must not contain a query`);
  assert.equal(url.hash, "", `${label} must not contain a fragment`);
  return url;
}

function digest(value) {
  assert.match(value, /^sha256:[a-f0-9]{64}$/);
  return value.slice("sha256:".length);
}

function writeNew(path, bytes) {
  const handle = openSync(path, "wx", 0o600);
  try {
    writeSync(handle, bytes);
  } finally {
    closeSync(handle);
  }
}

export function validateRuntimeManifest(manifest, target, channel = CHANNEL) {
  assert.ok(TARGETS.has(target), `unsupported runtime target ${target}`);
  assert.equal(manifest?.schema, "reporch.runtime-bundle-manifest.v1");
  assert.equal(manifest.target, target);
  assert.ok(Number.isSafeInteger(manifest.sequence) && manifest.sequence > 0);
  assert.match(manifest.version, /^[0-9A-Za-z][0-9A-Za-z._-]{0,63}$/);
  assert.ok(Array.isArray(manifest.artifacts) && manifest.artifacts.length >= 3);
  const base = safeUrl(channel, "runtime channel");
  const expectedPrefix = `${base.pathname}runtime-${target}-`;
  const names = new Set();
  for (const artifact of manifest.artifacts) {
    assert.match(artifact.file_name, /^[0-9A-Za-z][0-9A-Za-z._-]{0,127}$/);
    assert.ok(!names.has(artifact.file_name), "runtime artifact names must be unique");
    names.add(artifact.file_name);
    assert.ok(Number.isSafeInteger(artifact.size) && artifact.size > 0 && artifact.size <= MAX_ARTIFACT);
    digest(artifact.sha256);
    for (const [field, suffix] of [
      ["source_url", ""],
      ["sbom_url", ".spdx.json"],
      ["provenance_url", ".intoto.jsonl"]
    ]) {
      const url = safeUrl(artifact[field], `runtime artifact ${field}`);
      assert.equal(url.origin, base.origin, `runtime artifact ${field} changed origin`);
      assert.equal(
        url.pathname,
        `${expectedPrefix}${artifact.file_name}${suffix}`,
        `runtime artifact ${field} is not target-bound`
      );
    }
  }
  return manifest;
}

async function fetchBytes(url, maximum) {
  const response = await fetch(url, { redirect: "follow", signal: AbortSignal.timeout(30_000) });
  assert.ok(response.ok, `download failed: ${response.status} ${url}`);
  const declared = Number(response.headers.get("content-length"));
  if (Number.isFinite(declared)) assert.ok(declared > 0 && declared <= maximum);
  const reader = response.body.getReader();
  const chunks = [];
  let total = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    assert.ok(total <= maximum, `download exceeded ${maximum} bytes`);
    chunks.push(value);
  }
  assert.ok(total > 0, "download was empty");
  return Buffer.concat(chunks, total);
}

async function fetchFile(url, output, expectedSize, expectedDigest) {
  const response = await fetch(url, { redirect: "follow", signal: AbortSignal.timeout(30 * 60_000) });
  assert.ok(response.ok, `artifact download failed: ${response.status} ${url}`);
  const declared = Number(response.headers.get("content-length"));
  if (Number.isFinite(declared)) assert.equal(declared, expectedSize);
  const handle = openSync(output, "wx", 0o600);
  const hasher = createHash("sha256");
  let total = 0;
  try {
    const reader = response.body.getReader();
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      total += value.byteLength;
      assert.ok(total <= expectedSize, "runtime artifact exceeded its declared size");
      hasher.update(value);
      writeSync(handle, value);
    }
  } finally {
    closeSync(handle);
  }
  assert.equal(total, expectedSize, "runtime artifact size mismatch");
  assert.equal(hasher.digest("hex"), digest(expectedDigest), "runtime artifact SHA-256 mismatch");
}

export async function fetchRuntimeBundle(target, outputArgument, channel = CHANNEL) {
  assert.ok(TARGETS.has(target), `unsupported runtime target ${target}`);
  const output = resolve(outputArgument);
  assert.ok(!existsSync(output), `runtime bundle output already exists: ${output}`);
  const parent = resolve(dirname(output));
  assert.ok(parent !== resolve("/") && basename(output).length >= 3, "unsafe runtime output");
  mkdirSync(parent, { recursive: true, mode: 0o755 });
  const staging = mkdtempSync(join(parent, `.runtime-${target}-`));
  try {
    const base = safeUrl(channel, "runtime channel");
    const manifestName = `runtime-${target}-manifest.json`;
    const manifestBytes = await fetchBytes(new URL(manifestName, base), MAX_MANIFEST);
    const manifest = validateRuntimeManifest(JSON.parse(manifestBytes), target, base);
    const signatureBytes = await fetchBytes(
      new URL(`${manifestName}.minisig`, base),
      MAX_SIGNATURE
    );
    writeNew(join(staging, "manifest.json"), manifestBytes);
    writeNew(join(staging, "manifest.json.minisig"), signatureBytes);
    const artifacts = join(staging, "artifacts");
    mkdirSync(artifacts, { mode: 0o700 });
    for (const artifact of manifest.artifacts) {
      await fetchFile(
        artifact.source_url,
        join(artifacts, artifact.file_name),
        artifact.size,
        artifact.sha256
      );
    }
    renameSync(staging, output);
    return { target, sequence: manifest.sequence, version: manifest.version, output };
  } catch (error) {
    rmSync(staging, { recursive: true, force: true });
    throw error;
  }
}

async function main() {
  const [target, output] = process.argv.slice(2);
  if (!target || !output) {
    throw new Error("usage: node scripts/fetch-runtime-bundle.mjs <runtime-target> <new-output>");
  }
  console.log(JSON.stringify(await fetchRuntimeBundle(target, output)));
}

const invoked = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : null;
if (invoked === import.meta.url) await main();
