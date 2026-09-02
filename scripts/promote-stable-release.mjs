import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";

import { TARGETS } from "./release-lib.mjs";

const version = JSON.parse(readFileSync("npm/cli/package.json", "utf8")).version;
assert.match(version, /^\d+\.\d+\.\d+$/, "only a stable version can be promoted");

function run(command, args) {
  const result = spawnSync(command, args, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
  assert.equal(
    result.status,
    0,
    `${command} ${args.join(" ")} failed\n${result.stdout}\n${result.stderr}`
  );
  return result.stdout.trim();
}

const platformPackages = TARGETS.map((target) => target.packageName);
const packages = [...platformPackages, "@reporch/cli"];
for (const name of packages) {
  const published = JSON.parse(run("npm", ["view", `${name}@${version}`, "version", "--json", "--prefer-online"]));
  assert.equal(published, version, `${name} candidate is missing`);
  const candidate = JSON.parse(run("npm", ["view", name, "dist-tags.candidate", "--json", "--prefer-online"]));
  assert.equal(candidate, version, `${name} candidate tag does not point to the exact version`);
}

// Platform packages must become discoverable before the wrapper. If the final
// wrapper command fails, retrying this script is idempotent.
for (const name of packages) {
  run("npm", ["dist-tag", "add", `${name}@${version}`, "latest"]);
  run("npm", ["dist-tag", "add", `${name}@${version}`, "next"]);
}

const tag = `v${version}`;
assert.equal(
  JSON.parse(run("gh", ["release", "view", tag, "--json", "isDraft"]))?.isDraft,
  true,
  "GitHub stable candidate must still be a draft"
);
run("gh", ["release", "edit", tag, "--draft=false", "--latest"]);
console.log(`promoted ${version} to npm latest/next and published ${tag}`);
