import assert from "node:assert/strict";
import test from "node:test";

import { validateRuntimeManifest } from "./fetch-runtime-bundle.mjs";

function fixture(target = "windows-x64-msvc") {
  const file = target.startsWith("windows") ? "kernel" : "vmlinux";
  const prefix = `https://github.com/Reporch/cli/releases/download/reporch-runtime-v1-seq23/runtime-${target}-${file}`;
  return {
    schema: "reporch.runtime-bundle-manifest.v1",
    sequence: 23,
    version: "1.0.0-rc.8",
    target,
    artifacts: ["kernel", "rootfs", "guest_agent"].map((kind, index) => ({
      kind,
      file_name: index === 0 ? file : `${kind}.bin`,
      sha256: `sha256:${String(index + 1).repeat(64)}`,
      size: index + 1,
      source_url: index === 0 ? prefix : `${prefix.slice(0, -file.length)}${kind}.bin`,
      sbom_url:
        (index === 0 ? prefix : `${prefix.slice(0, -file.length)}${kind}.bin`) + ".spdx.json",
      provenance_url:
        (index === 0 ? prefix : `${prefix.slice(0, -file.length)}${kind}.bin`) + ".intoto.jsonl"
    }))
  };
}

test("runtime downloader accepts only target-namespaced immutable assets", () => {
  const manifest = fixture();
  assert.equal(validateRuntimeManifest(manifest, "windows-x64-msvc"), manifest);
  const crossed = structuredClone(manifest);
  crossed.artifacts[0].source_url = crossed.artifacts[0].source_url.replace(
    "windows-x64-msvc",
    "linux-x64-gnu"
  );
  assert.throws(() => validateRuntimeManifest(crossed, "windows-x64-msvc"));
});

test("runtime downloader rejects rollback-shaped, duplicate, and credentialed metadata", () => {
  const rollback = fixture();
  rollback.sequence = 0;
  assert.throws(() => validateRuntimeManifest(rollback, rollback.target));
  const duplicate = fixture();
  duplicate.artifacts[1].file_name = duplicate.artifacts[0].file_name;
  assert.throws(() => validateRuntimeManifest(duplicate, duplicate.target));
  const credentialed = fixture();
  credentialed.artifacts[0].source_url = credentialed.artifacts[0].source_url.replace(
    "https://",
    "https://token@example.invalid@"
  );
  assert.throws(() => validateRuntimeManifest(credentialed, credentialed.target));
});
