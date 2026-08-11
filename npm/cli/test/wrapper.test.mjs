import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";

import {
  PLATFORM_PACKAGES,
  packageFor,
  resolveBinary,
  sha256File,
  verifyBinary
} from "../bin/reporch.mjs";

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
