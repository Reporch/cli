import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

import {
  TARGETS,
  restoreTransferredRuntimePermissions,
  stagePlatformPackage,
  stageRootPackage,
  validateOutputArgument
} from "./release-lib.mjs";

const [artifactArgument, outputArgument] = process.argv.slice(2);
if (!artifactArgument || !outputArgument) {
  throw new Error("usage: node scripts/prepare-npm-packages.mjs <artifacts> <new-output>");
}
const artifacts = resolve(artifactArgument);
const output = validateOutputArgument(outputArgument);
if (!existsSync(artifacts)) throw new Error(`artifact directory does not exist: ${artifacts}`);
if (existsSync(output)) throw new Error(`release output already exists: ${output}`);
mkdirSync(output, { recursive: false });

const checksums = {};
const manifestTargets = [];
for (const target of TARGETS) {
  const binary = join(artifacts, target.target, target.binaryName);
  const runtimeTree = join(artifacts, "runtime", target.runtimeTarget);
  restoreTransferredRuntimePermissions(runtimeTree, target.runtimeTarget);
  const staged = stagePlatformPackage(target, binary, runtimeTree, output);
  checksums[target.packageName] = staged.sha256;
  manifestTargets.push({
    target: target.target,
    package: target.packageName,
    binary: `bin/${target.binaryName}`,
    sha256: staged.sha256,
    runtimeSequence: staged.runtime.sequence,
    runtimeManifestSha256: staged.runtime.manifestSha256
  });
}
stageRootPackage(checksums, output);

const version = JSON.parse(readFileSync(join(output, "cli/package.json"), "utf8")).version;
writeFileSync(
  join(output, "release-manifest.json"),
  `${JSON.stringify(
    {
      schema: "reporch.cli-npm-release.v1",
      version,
      sourceRevision: process.env.GITHUB_SHA ?? null,
      targets: manifestTargets
    },
    null,
    2
  )}\n`
);
console.log(`staged @reporch/cli ${version} with ${manifestTargets.length} native packages`);
