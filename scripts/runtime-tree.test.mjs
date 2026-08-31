import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { assertRuntimeInstallTree, copyRuntimeInstallTree } from "./release-lib.mjs";
import { createRuntimeTreeFixture } from "./runtime-tree-fixture.mjs";

test("runtime release trees are target and digest bound", () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-runtime-tree-test-"));
  try {
    const source = createRuntimeTreeFixture(join(root, "source"), "darwin-arm64");
    const verified = assertRuntimeInstallTree(source, "darwin-arm64");
    assert.equal(verified.sequence, 14);
    const copied = copyRuntimeInstallTree(source, join(root, "copied"), "darwin-arm64");
    assert.equal(copied.manifestSha256, verified.manifestSha256);
    assert.throws(() => assertRuntimeInstallTree(source, "darwin-x64"), /target/);

    const artifact = join(source, "bundles/14-1.0.0-rc.8/vmlinux");
    chmodSync(artifact, 0o644);
    writeFileSync(artifact, Buffer.concat([readFileSync(artifact), Buffer.from("changed")]));
    assert.throws(() => assertRuntimeInstallTree(source, "darwin-arm64"), /size|hash/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("runtime release trees never accept symlink artifacts", { skip: process.platform === "win32" }, () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-runtime-tree-symlink-"));
  try {
    const source = createRuntimeTreeFixture(join(root, "source"), "linux-x64-gnu");
    const artifact = join(source, "bundles/14-1.0.0-rc.8/vmlinux");
    chmodSync(artifact, 0o644);
    rmSync(artifact);
    writeFileSync(join(root, "outside"), "kernel linux-x64-gnu\n");
    symlinkSync(join(root, "outside"), artifact);
    assert.throws(() => assertRuntimeInstallTree(source, "linux-x64-gnu"), /regular file/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
