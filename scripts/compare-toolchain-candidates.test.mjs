import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { compareToolchainCandidates } from "./compare-toolchain-candidates.mjs";

const revision = "a".repeat(40);
const targets = ["darwin-arm64", "darwin-x64", "linux-arm64-gnu", "linux-x64-gnu", "windows-x64-msvc"];

function fixture(root, payload = "bundle\n") {
  mkdirSync(root, { recursive: true });
  const entries = [];
  for (let index = 0; index < 12; index += 1) {
    const archive = `language-${index}.ext4.zst`;
    writeFileSync(join(root, archive), payload);
    writeFileSync(
      join(root, `${archive}.intoto.jsonl`),
      `${JSON.stringify({ predicate: { buildDefinition: { internalParameters: { sourceRevision: revision } } } })}\n`
    );
    entries.push({ id: `language-${index}`, bundles: targets.map((target) => ({ target })) });
  }
  writeFileSync(
    join(root, "toolchains-v2-index.json"),
    `${JSON.stringify({ schema: "reporch.toolchain-index.v2", sequence: 8, entries })}\n`
  );
}

test("toolchain candidates require independent byte-identical trees", async () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-toolchain-repro-"));
  try {
    const first = join(root, "first");
    const second = join(root, "second");
    fixture(first);
    fixture(second);
    const result = await compareToolchainCandidates(first, second);
    assert.equal(result.toolchains, 12);
    writeFileSync(join(second, "language-0.ext4.zst"), "changed\n");
    await assert.rejects(() => compareToolchainCandidates(first, second), /not byte-for-byte reproducible/u);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
