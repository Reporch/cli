import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { TARGETS } from "./release-lib.mjs";
import { nativeArchiveName, packNativeRelease } from "./pack-native-release.mjs";
import { createRuntimeTreeFixture } from "./runtime-tree-fixture.mjs";

test("standalone archive names cover every official target", () => {
  const names = TARGETS.map((target) => nativeArchiveName("0.9.0", target));
  assert.equal(new Set(names).size, TARGETS.length);
  assert.deepEqual(names, [
    "reporch-v0.9.0-aarch64-apple-darwin.tar.gz",
    "reporch-v0.9.0-x86_64-apple-darwin.tar.gz",
    "reporch-v0.9.0-aarch64-unknown-linux-gnu.tar.gz",
    "reporch-v0.9.0-x86_64-unknown-linux-gnu.tar.gz",
    "reporch-v0.9.0-x86_64-pc-windows-msvc.zip"
  ]);
});

test(
  "standalone archives and their manifest are deterministic",
  { skip: process.platform !== "linux" },
  () => {
    const root = mkdtempSync(join(tmpdir(), "reporch-native-release-test-"));
    try {
      const artifacts = join(root, "artifacts");
      mkdirSync(artifacts);
      const runtimes = join(artifacts, "runtime");
      mkdirSync(runtimes);
      for (const [index, target] of TARGETS.entries()) {
        const directory = join(artifacts, target.target);
        mkdirSync(directory);
        writeFileSync(join(directory, target.binaryName), randomBytes(120_000 + index));
        createRuntimeTreeFixture(join(runtimes, target.runtimeTarget), target.runtimeTarget);
      }
      const first = packNativeRelease(artifacts, join(root, "first"));
      const second = packNativeRelease(artifacts, join(root, "second"));
      assert.equal(first.archives.length, TARGETS.length);
      for (const entry of first.archives) {
        const digest = (directory) =>
          createHash("sha256")
            .update(readFileSync(join(directory, entry.filename)))
            .digest("hex");
        assert.equal(digest(first.output), digest(second.output));
      }
      const files = readdirSync(first.output).sort();
      assert.deepEqual(files, [
        "native-release-manifest.json",
        ...TARGETS.map((target) => nativeArchiveName(first.version, target))
      ].sort());
      const manifest = JSON.parse(
        readFileSync(join(first.output, "native-release-manifest.json"), "utf8")
      );
      assert.equal(manifest.schema, "reporch.cli-native-release.v1");
      assert.equal(manifest.archives.length, TARGETS.length);
      assert.ok(manifest.archives.every((archive) => archive.runtimeSequence === 8));
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  }
);
