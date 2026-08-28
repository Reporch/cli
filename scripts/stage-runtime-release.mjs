import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  writeFileSync
} from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

import { validateRuntimeManifest } from "./fetch-runtime-bundle.mjs";

const TARGETS = [
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64-gnu",
  "linux-x64-gnu",
  "windows-x64-msvc"
];

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function regular(path, maximum, label) {
  const stat = lstatSync(path);
  assert.ok(
    stat.isFile() && !stat.isSymbolicLink() && stat.size > 0 && stat.size <= maximum,
    `${label} is not a bounded regular file`
  );
  return stat;
}

export function stageRuntimeRelease(manifestsArgument, artifactsArgument, outputArgument) {
  const manifests = resolve(manifestsArgument);
  const artifacts = resolve(artifactsArgument);
  const output = resolve(outputArgument);
  assert.ok(!existsSync(output), `runtime release output already exists: ${output}`);
  assert.ok(basename(output).length >= 3, "unsafe runtime release output");
  mkdirSync(dirname(output), { recursive: true, mode: 0o755 });
  const staging = mkdtempSync(join(dirname(output), ".runtime-release-"));
  const records = [];
  try {
    for (const target of TARGETS) {
    const manifestName = `runtime-${target}-manifest.json`;
    const manifestPath = join(manifests, manifestName);
    const signaturePath = `${manifestPath}.minisig`;
    regular(manifestPath, 256 * 1024, `${target} runtime manifest`);
    regular(signaturePath, 16 * 1024, `${target} runtime signature`);
    const manifest = validateRuntimeManifest(JSON.parse(readFileSync(manifestPath)), target);
    const targetArtifacts = join(artifacts, target);
    for (const artifact of manifest.artifacts) {
      const source = join(targetArtifacts, artifact.file_name);
      const stat = regular(source, 4 * 1024 * 1024 * 1024, `${target} ${artifact.file_name}`);
      assert.equal(stat.size, artifact.size, `${target} artifact size drifted`);
      assert.equal(`sha256:${sha256(source)}`, artifact.sha256, `${target} artifact hash drifted`);
      const releaseName = basename(new URL(artifact.source_url).pathname);
      assert.equal(releaseName, `runtime-${target}-${artifact.file_name}`);
      copyFileSync(source, join(staging, releaseName));
      for (const [suffix, urlField] of [
        [".spdx.json", "sbom_url"],
        [".intoto.jsonl", "provenance_url"]
      ]) {
        const evidence = `${source}${suffix}`;
        regular(evidence, 16 * 1024 * 1024, `${target} ${artifact.file_name}${suffix}`);
        const evidenceName = basename(new URL(artifact[urlField]).pathname);
        assert.equal(evidenceName, `${releaseName}${suffix}`);
        copyFileSync(evidence, join(staging, evidenceName));
      }
    }
      copyFileSync(manifestPath, join(staging, manifestName));
      copyFileSync(signaturePath, join(staging, `${manifestName}.minisig`));
    }
    for (const name of readdirSync(staging).sort()) {
      const path = join(staging, name);
      const stat = regular(path, 4 * 1024 * 1024 * 1024, `runtime release asset ${name}`);
      records.push({ filename: name, sha256: sha256(path), size: stat.size });
    }
    writeFileSync(
      join(staging, "runtime-release-manifest.json"),
      `${JSON.stringify({ schema: "reporch.runtime-release.v1", targets: TARGETS, assets: records }, null, 2)}\n`
    );
    const checksumNames = [
      ...records.map((record) => record.filename),
      "runtime-release-manifest.json"
    ];
    writeFileSync(
      join(staging, "SHA256SUMS"),
      checksumNames
        .sort()
        .map((name) => `${sha256(join(staging, name))}  ${name}\n`)
        .join("")
    );
    renameSync(staging, output);
    return { output, targets: TARGETS.length, assets: checksumNames.length + 1 };
  } catch (error) {
    rmSync(staging, { recursive: true, force: true });
    throw error;
  }
}

function main() {
  const [manifests, artifacts, output] = process.argv.slice(2);
  if (!manifests || !artifacts || !output) {
    throw new Error("usage: node scripts/stage-runtime-release.mjs <signed-manifests> <target-artifacts> <new-output>");
  }
  console.log(JSON.stringify(stageRuntimeRelease(manifests, artifacts, output)));
}

const invoked = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : null;
if (invoked === import.meta.url) main();
