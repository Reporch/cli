import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { gzipSync } from "node:zlib";

import {
  PLATFORM_PACKAGES,
  packageFor,
  extractPlatformBinary,
  resolveBinary,
  resolveOrRecoverBinary,
  sha256File,
  verifyBinary
} from "../bin/reporch.mjs";

function tarEntry(path, contents, type = "0") {
  const body = Buffer.from(contents);
  const header = Buffer.alloc(512);
  header.write(path, 0, 100, "utf8");
  const octal = (value, length) => `${value.toString(8).padStart(length - 1, "0")}\0`;
  header.write(octal(0o755, 8), 100, 8, "ascii");
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

test("recovers an omitted optional package once and reuses the verified cache", async () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-cli-recovery-"));
  const contents = Buffer.from("recovered native binary\n");
  const packageName = "@reporch/cli-linux-x64-gnu";
  const checksums = { [packageName]: sha256File(writeFixture(root, contents)) };
  const tarball = npmTarball("package/bin/reporch", contents);
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
