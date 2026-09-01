import assert from "node:assert/strict";
import { createHash } from "node:crypto";
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

import { stageRuntimeRelease } from "./stage-runtime-release.mjs";

const TARGETS = [
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64-gnu",
  "linux-x64-gnu",
  "windows-x64-msvc"
];

function sha(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function fixture(root) {
  const manifests = join(root, "manifests");
  const artifacts = join(root, "artifacts");
  mkdirSync(manifests);
  mkdirSync(artifacts);
  for (const target of TARGETS) {
    const directory = join(artifacts, target);
    mkdirSync(directory);
    const definitions = [
      ["kernel", target === "windows-x64-msvc" ? "kernel" : "vmlinux"],
      ["rootfs", "rootfs.cpio"],
      ["guest_agent", "reporch-guestd"]
    ];
    const manifestArtifacts = definitions.map(([kind, file]) => {
      const bytes = Buffer.from(`${target} ${kind}\n`);
      writeFileSync(join(directory, file), bytes);
      writeFileSync(join(directory, `${file}.spdx.json`), "{}\n");
      writeFileSync(join(directory, `${file}.intoto.jsonl`), "{}\n");
      const prefix = `https://github.com/Reporch/cli/releases/download/reporch-runtime-v1-seq21/runtime-${target}-${file}`;
      return {
        kind,
        file_name: file,
        sha256: `sha256:${sha(bytes)}`,
        size: bytes.length,
        source_url: prefix,
        sbom_url: `${prefix}.spdx.json`,
        provenance_url: `${prefix}.intoto.jsonl`
      };
    });
    const name = `runtime-${target}-manifest.json`;
    writeFileSync(
      join(manifests, name),
      `${JSON.stringify({
        schema: "reporch.runtime-bundle-manifest.v1",
        sequence: 21,
        version: "1.0.0-rc.8",
        target,
        artifacts: manifestArtifacts
      })}\n`
    );
    writeFileSync(join(manifests, `${name}.minisig`), "trusted fixture signature\n");
  }
  return { manifests, artifacts };
}

test("runtime release staging is flat, collision-free, and digest-bound", () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-runtime-release-test-"));
  try {
    const input = fixture(root);
    const output = join(root, "release");
    const result = stageRuntimeRelease(input.manifests, input.artifacts, output);
    assert.equal(result.targets, 5);
    const names = readdirSync(output);
    assert.equal(new Set(names).size, names.length);
    assert.ok(names.includes("runtime-windows-x64-msvc-kernel"));
    assert.ok(names.includes("runtime-darwin-arm64-vmlinux"));
    assert.ok(readFileSync(join(output, "SHA256SUMS"), "utf8").endsWith("\n"));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("runtime release staging rejects artifact mutation before publishing", () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-runtime-release-test-"));
  try {
    const input = fixture(root);
    writeFileSync(join(input.artifacts, "windows-x64-msvc", "kernel"), "changed\n");
    assert.throws(() => stageRuntimeRelease(input.manifests, input.artifacts, join(root, "release")));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
