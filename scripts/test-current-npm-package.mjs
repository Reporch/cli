import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";

import { TARGETS, stagePlatformPackage, stageRootPackage } from "./release-lib.mjs";
import { createRuntimeTreeFixture } from "./runtime-tree-fixture.mjs";

const [targetName, binaryArgument] = process.argv.slice(2);
const target = TARGETS.find((candidate) => candidate.target === targetName);
if (!target || !binaryArgument) {
  throw new Error(
    "usage: node scripts/test-current-npm-package.mjs <rust-target> <binary>"
  );
}
const binary = resolve(binaryArgument);
const temporary = mkdtempSync(join(tmpdir(), "reporch-npm-e2e-"));
const packages = join(temporary, "packages");
mkdirSync(packages);
const runtimeTree = createRuntimeTreeFixture(
  join(temporary, "runtime", target.runtimeTarget),
  target.runtimeTarget
);
const staged = stagePlatformPackage(target, binary, runtimeTree, packages);
const checksums = Object.fromEntries(TARGETS.map((item) => [item.packageName, "0".repeat(64)]));
checksums[target.packageName] = staged.sha256;
const root = stageRootPackage(checksums, packages);

function run(command, args, cwd = temporary) {
  const commandString = String(command);
  const result = spawnSync(command, args, {
    cwd,
    encoding: "utf8",
    shell: process.platform === "win32" && commandString.toLowerCase().endsWith(".cmd"),
    windowsHide: true,
    env: { ...process.env, npm_config_audit: "false", npm_config_fund: "false" }
  });
  const diagnostics = [result.stdout, result.stderr, result.error?.stack]
    .filter(Boolean)
    .join("\n");
  assert.equal(result.status, 0, `${commandString} ${args.join(" ")}\n${diagnostics}`);
  return result.stdout.trim();
}

const npmCommand = process.platform === "win32" ? "npm.cmd" : "npm";
const platformPack = JSON.parse(run(npmCommand, ["pack", staged.destination, "--json"]))[0]
  .filename;
const rootPack = JSON.parse(run(npmCommand, ["pack", root, "--json"]))[0].filename;
writeFileSync(
  join(temporary, "package.json"),
  `${JSON.stringify({ name: "reporch-cli-install-test", version: "1.0.0", private: true })}\n`
);
run(npmCommand, ["install", "--ignore-scripts", `./${platformPack}`, `./${rootPack}`]);
const executable = process.platform === "win32" ? "reporch.cmd" : "reporch";
const output = run(join(temporary, "node_modules/.bin", executable), ["--version"]);
const expectedVersion = JSON.parse(readFileSync(join(root, "package.json"), "utf8")).version;
assert.equal(output, `reporch ${expectedVersion}`);
console.log(`npm install test passed for ${target.packageName}`);
