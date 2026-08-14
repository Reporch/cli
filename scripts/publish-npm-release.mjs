import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

import { waitForExpectedIntegrity } from "./npm-registry.mjs";
import { npmTagForVersion } from "./release-lib.mjs";

const [releaseDirectory = "dist"] = process.argv.slice(2);
const manifest = JSON.parse(
  readFileSync(join(releaseDirectory, "npm-pack-manifest.json"), "utf8")
);
assert.equal(manifest.schema, "reporch.cli-npm-pack.v1");
assert.equal(manifest.packages.length, 6);
const versions = new Set(manifest.packages.map((item) => item.version));
assert.equal(versions.size, 1, "all npm packages must have one release version");
const [version] = versions;
const npmTag = npmTagForVersion(version);

function command(commandName, args) {
  return spawnSync(commandName, args, {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"]
  });
}

for (const item of manifest.packages) {
  const spec = `${item.name}@${item.version}`;
  const existing = command("npm", [
    "view",
    spec,
    "dist.integrity",
    "--json",
    "--prefer-online"
  ]);
  if (existing.status === 0) {
    const integrity = JSON.parse(existing.stdout);
    assert.equal(integrity, item.integrity, `${spec} exists with different immutable bytes`);
    console.log(`${spec} already exists with the expected integrity`);
    continue;
  }
  assert.match(existing.stderr, /E404|not in this registry|No match found/i, existing.stderr);
  const tarball = join(releaseDirectory, "tarballs", item.filename);
  const published = command("npm", [
    "publish",
    tarball,
    "--access",
    "public",
    "--tag",
    npmTag
  ]);
  assert.equal(published.status, 0, `${spec} publish failed\n${published.stdout}\n${published.stderr}`);
  await waitForExpectedIntegrity({
    lookup: () =>
      command("npm", ["view", spec, "dist.integrity", "--json", "--prefer-online"]),
    spec,
    expectedIntegrity: item.integrity
  });
  console.log(`published and verified ${spec}`);
}
