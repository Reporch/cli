import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { chmodSync, mkdtempSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { gzipSync } from "node:zlib";

import {
  PLATFORM_PACKAGES,
  packageFor,
  extractPlatformBinary,
  extractPlatformPayload,
  resolveBinary,
  resolveOrRecoverBinary,
  sha256File,
  verifyBinary
} from "../bin/reporch.mjs";

function tarEntry(path, contents, type = "0", mode = 0o755) {
  const body = Buffer.from(contents);
  const header = Buffer.alloc(512);
  header.write(path, 0, 100, "utf8");
  const octal = (value, length) => `${value.toString(8).padStart(length - 1, "0")}\0`;
  header.write(octal(mode, 8), 100, 8, "ascii");
  header.write(octal(0, 8), 108, 8, "ascii");
  header.write(octal(0, 8), 116, 8, "ascii");
  header.write(octal(body.length, 12), 124, 12, "ascii");
  header.write(octal(0, 12), 136, 12, "ascii");
  header.fill(0x20, 148, 156);
  header[156] = type.charCodeAt(0);
  header.write("ustar\0", 257, 6, "ascii");
  header.write("00", 263, 2, "ascii");
  const checksum = header.reduce((sum, byte) => sum + byte, 0);
  header.write(`${checksum.toString(8).padStart(6, "0")}\0 `, 148, 8, "ascii");
  const padding = Buffer.alloc((512 - (body.length % 512)) % 512);
  return Buffer.concat([header, body, padding]);
}

function npmTarball(path, contents, type = "0") {
  return gzipSync(Buffer.concat([tarEntry(path, contents, type), Buffer.alloc(1024)]));
}

function npmTarballEntries(entries) {
  return gzipSync(
    Buffer.concat([
      ...entries.map(({ path, contents, type = "0", mode = 0o444 }) =>
        tarEntry(path, contents, type, mode)
      ),
      Buffer.alloc(1024)
    ])
  );
}

function runtimePayloadEntries(binaryContents, target = "linux-x64-gnu") {
  const digest = (contents) =>
    `sha256:${createHash("sha256").update(contents).digest("hex")}`;
  const definitions = [
    ["kernel", "vmlinux", Buffer.from("kernel\n")],
    ["rootfs", "rootfs.cpio", Buffer.from("rootfs\n")],
    ["guest_agent", "reporch-guestd", Buffer.from("guestd\n")]
  ];
  const artifacts = definitions.map(([kind, file_name, contents]) => ({
    kind,
    file_name,
    sha256: digest(contents),
    size: contents.length,
    source_url: `https://example.test/${file_name}`,
    sbom_url: `https://example.test/${file_name}.spdx.json`,
    provenance_url: `https://example.test/${file_name}.intoto.jsonl`
  }));
  const manifest = Buffer.from(
    `${JSON.stringify({
      schema: "reporch.runtime-bundle-manifest.v1",
      sequence: 13,
      version: "1.0.0-rc.8",
      target,
      backend: "firecracker",
      minimum_os_version: "1",
      protocol_min: 1,
      protocol_max: 1,
      generated_at: "2026-08-29T00:00:00Z",
      expires_at: "2026-10-03T00:00:00Z",
      signing_key_id: "FF2F931B66DAA966",
      artifacts
    })}\n`
  );
  const manifestDigest = digest(manifest);
  const bundle = `package/bin/runtime/${target}/bundles/13-1.0.0-rc.8`;
  return [
    { path: "package/bin/reporch", contents: binaryContents, mode: 0o755 },
    {
      path: `package/bin/runtime/${target}/current.json`,
      contents: Buffer.from(`${JSON.stringify({
        schema: "reporch.runtime-installation.v1",
        sequence: 13,
        version: "1.0.0-rc.8",
        target,
        bundle_sha256: manifestDigest,
        installed_at: "2026-08-29T00:00:00Z"
      })}\n`)
    },
    { path: `${bundle}/manifest.json`, contents: manifest },
    { path: `${bundle}/manifest.json.minisig`, contents: Buffer.from("signature\n") },
    { path: `${bundle}/.complete`, contents: Buffer.from(`${manifestDigest}\n`) },
    ...definitions.map(([kind, file_name, contents]) => ({
      path: `${bundle}/${file_name}`,
      contents,
      mode: kind === "guest_agent" ? 0o555 : 0o444
    }))
  ];
}

test("maps every supported npm target exactly", () => {
  assert.equal(packageFor("darwin", "arm64"), "@reporch/cli-darwin-arm64");
  assert.equal(packageFor("darwin", "x64"), "@reporch/cli-darwin-x64");
  assert.equal(packageFor("linux", "arm64"), "@reporch/cli-linux-arm64-gnu");
  assert.equal(packageFor("linux", "x64"), "@reporch/cli-linux-x64-gnu");
  assert.equal(packageFor("win32", "x64"), "@reporch/cli-win32-x64-msvc");
  assert.equal(Object.keys(PLATFORM_PACKAGES).length, 5);
  assert.throws(() => packageFor("freebsd", "x64"), /does not support/);
});

test("resolves and verifies the selected native binary", () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-cli-wrapper-"));
  const packageJson = join(root, "package.json");
  const binary = join(root, "bin", "reporch");
  mkdirSync(dirname(binary));
  writeFileSync(packageJson, "{}\n");
  writeFileSync(binary, "safe fixture\n");
  mkdirSync(join(root, "bin/runtime/darwin-arm64"), { recursive: true });
  writeFileSync(join(root, "bin/runtime/darwin-arm64/current.json"), "{}\n");
  chmodSync(binary, 0o755);
  const packageName = "@reporch/cli-darwin-arm64";
  assert.equal(
    resolveBinary({
      platform: "darwin",
      arch: "arm64",
      resolvePackage: () => packageJson,
      checksums: { [packageName]: sha256File(binary) }
    }),
    binary
  );
});

test("fails closed for a changed binary or missing optional package", () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-cli-integrity-"));
  const binary = join(root, "reporch");
  writeFileSync(binary, "changed\n");
  assert.throws(() => verifyBinary(binary, "0".repeat(64)), /integrity check failed/);
  assert.throws(
    () =>
      resolveBinary({
        platform: "linux",
        arch: "x64",
        resolvePackage: () => {
          throw new Error("not installed");
        }
      }),
    /platform package.*is missing/
  );
});

test("safely extracts only the exact regular platform binary", () => {
  const archive = npmTarball("package/bin/reporch", "verified binary\n");
  assert.equal(extractPlatformBinary(archive, "reporch").toString(), "verified binary\n");
  assert.throws(
    () => extractPlatformBinary(npmTarball("package/bin/reporch", "link", "2"), "reporch"),
    /link or unsupported entry/
  );
  assert.throws(
    () => extractPlatformBinary(npmTarball("package/../bin/reporch", "bad"), "reporch"),
    /unsafe path/
  );
});

test("extracts a complete digest-bound runtime payload and rejects mutation", () => {
  const entries = runtimePayloadEntries(Buffer.from("verified binary\n"));
  const tarball = npmTarballEntries(entries);
  const payload = extractPlatformPayload(tarball, "reporch", "linux-x64-gnu");
  assert.equal(payload.get("reporch").contents.toString(), "verified binary\n");
  assert.ok(payload.has("runtime/linux-x64-gnu/current.json"));

  const changed = runtimePayloadEntries(Buffer.from("verified binary\n"));
  changed.find((entry) => entry.path.endsWith("/vmlinux")).contents = Buffer.from("changed\n");
  assert.throws(
    () => extractPlatformPayload(npmTarballEntries(changed), "reporch", "linux-x64-gnu"),
    /failed integrity/
  );
});

test("recovers an omitted optional package once and reuses the verified cache", async () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-cli-recovery-"));
  const contents = Buffer.from("recovered native binary\n");
  const packageName = "@reporch/cli-linux-x64-gnu";
  const checksums = { [packageName]: sha256File(writeFixture(root, contents)) };
  const tarball = npmTarballEntries(runtimePayloadEntries(contents));
  let requests = 0;
  const fetchImpl = async () => {
    requests += 1;
    return new Response(tarball, {
      status: 200,
      headers: { "content-length": String(tarball.length) }
    });
  };
  const options = {
    platform: "linux",
    arch: "x64",
    resolvePackage: () => { throw new Error("omitted"); },
    checksums,
    fetchImpl,
    cacheDirectory: join(root, "cache")
  };
  const binary = await resolveOrRecoverBinary(options);
  assert.deepEqual(readFileSync(binary), contents);
  assert.equal(requests, 1);
  assert.equal(await resolveOrRecoverBinary(options), binary);
  assert.equal(requests, 1);
});

function writeFixture(root, contents) {
  const path = join(root, "expected-binary");
  writeFileSync(path, contents);
  return path;
}
