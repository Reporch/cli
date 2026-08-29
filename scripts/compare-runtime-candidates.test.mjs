import assert from "node:assert/strict";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { compareRuntimeCandidates } from "./compare-runtime-candidates.mjs";

const targets = [
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64-gnu",
  "linux-x64-gnu",
  "windows-x64-msvc"
];
const revision = "a".repeat(40);

function fixture(root, payload = "guest\n") {
  mkdirSync(join(root, "artifacts", "darwin-arm64"), { recursive: true });
  mkdirSync(join(root, "manifests"), { recursive: true });
  writeFileSync(join(root, "artifacts", "darwin-arm64", "guestd"), payload, { mode: 0o555 });
  writeFileSync(
    join(root, "artifacts", "darwin-arm64", "guestd.intoto.jsonl"),
    `${JSON.stringify({ predicate: { buildDefinition: { internalParameters: { sourceRevision: revision } } } })}\n`
  );
  for (const target of targets) {
    writeFileSync(join(root, "manifests", `runtime-${target}-manifest.json`), `${target}\n`);
  }
}

test("runtime candidate comparison proves an independent byte-identical rebuild", () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-runtime-repro-"));
  try {
    const first = join(root, "first");
    const second = join(root, "second");
    fixture(first);
    fixture(second);
    const report = compareRuntimeCandidates(first, second);
    assert.equal(report.schema, "reporch.runtime-reproducibility.v1");
    assert.equal(report.source_revision, revision);
    assert.deepEqual(report.targets, targets);

    const guest = join(second, "artifacts", "darwin-arm64", "guestd");
    chmodSync(guest, 0o755);
    writeFileSync(guest, "mutated\n");
    chmodSync(guest, 0o555);
    assert.throws(
      () => compareRuntimeCandidates(first, second),
      /not byte-for-byte reproducible/
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
