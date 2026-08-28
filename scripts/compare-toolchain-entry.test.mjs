import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { aggregateToolchainEntryReports, compareToolchainEntry } from "./compare-toolchain-entry.mjs";

function fixture(root, id, payload = "fixture\n") {
  mkdirSync(root, { recursive: true });
  for (const [variant, sbom] of [
    ["linux-arm64.ext4.zst", "linux-arm64.source.spdx.json"],
    ["linux-x64.ext4.zst", "linux-x64.source.spdx.json"],
    ["windows-x64.vhdx.zst", "linux-x64.source.spdx.json"]
  ]) {
    writeFileSync(join(root, `${id}-${variant}`), payload);
    writeFileSync(join(root, `${id}-${variant}.build.json`), "receipt\n");
    writeFileSync(join(root, `${id}-${sbom}`), "sbom\n");
    writeFileSync(join(root, `${id}-${variant}.spdx.json`), "sbom\n");
  }
}

test("one independently rebuilt toolchain covers every platform artifact", async () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-toolchain-entry-"));
  try {
    const primary = join(root, "primary");
    const rebuild = join(root, "rebuild");
    fixture(primary, "bash-5.3");
    fixture(rebuild, "bash-5.3");
    const report = await compareToolchainEntry(primary, rebuild, "bash-5.3");
    assert.equal(report.files, 9);
    writeFileSync(join(rebuild, "bash-5.3-windows-x64.vhdx.zst"), "changed\n");
    await assert.rejects(() => compareToolchainEntry(primary, rebuild, "bash-5.3"), /differs/u);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("aggregate evidence requires twelve distinct toolchains", () => {
  const reports = Array.from({ length: 12 }, (_, index) => ({
    schema: "reporch.toolchain-entry-reproducibility.v2",
    id: `language-${index}`,
    files: 9,
    bytes: 100,
    tree_sha256: String(index).padStart(64, "a")
  }));
  const result = aggregateToolchainEntryReports(reports);
  assert.equal(result.toolchains, 12);
  reports[1].id = reports[0].id;
  assert.throws(() => aggregateToolchainEntryReports(reports));
});
