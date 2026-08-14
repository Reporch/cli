import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

const workflows = readdirSync(".github/workflows").filter((name) => /\.ya?ml$/.test(name));
assert.ok(workflows.length >= 2, "CI and release workflows are required");
for (const name of workflows) {
  const content = readFileSync(join(".github/workflows", name), "utf8");
  assert.doesNotMatch(content, /pull_request_target:/, `${name} must not use pull_request_target`);
  assert.doesNotMatch(content, /NODE_AUTH_TOKEN|NPM_TOKEN/, `${name} must not use npm tokens`);
  for (const line of content.split("\n")) {
    const match = line.match(/^\s*-?\s*uses:\s*([^#\s]+)/);
    if (!match) continue;
    const ref = match[1].split("@")[1] ?? "";
    assert.match(ref, /^[a-f0-9]{40}$/, `${name} has an action not pinned to a commit: ${line.trim()}`);
  }
}
const release = readFileSync(".github/workflows/release.yml", "utf8");
assert.match(release, /id-token:\s*write/, "release must mint OIDC tokens");
assert.match(release, /attestations:\s*write/, "release must create attestations");
assert.match(release, /environment:\s*npm-release/, "release must use a protected environment");
assert.match(
  release,
  /node scripts\/publish-npm-release\.mjs dist/,
  "release must invoke the reviewed npm OIDC publisher"
);
assert.match(
  release,
  /gh release edit "\$RELEASE_TAG" --target "\$GITHUB_SHA"/,
  "a retried draft release must target the current verified source revision"
);
assert.match(
  release,
  /node scripts\/pack-native-release\.mjs release-input dist\/native/,
  "release must build standalone archives for every native target"
);
assert.match(
  release,
  /\(cd dist\/release-assets && sha256sum \.\/\*\) > dist\/SHA256SUMS/,
  "release checksums must remain verifiable after downloading flat release assets"
);
assert.match(release, /sha256sum --check --strict/, "release checksum verification must be strict");
assert.match(
  release,
  /subject-path: dist\/release-assets\/\*/,
  "every immutable release asset must receive provenance"
);
const ci = readFileSync(".github/workflows/ci.yml", "utf8");
assert.match(ci, /actionlint_1\.7\.12_linux_amd64\.tar\.gz/);
assert.match(ci, /ACTIONLINT_SHA256:\s*[a-f0-9]{64}/);
assert.match(ci, /sha256sum --check --strict/);
const publisher = readFileSync("scripts/publish-npm-release.mjs", "utf8");
assert.match(publisher, /"--tag",\s*npmTag/);
assert.match(release, /gh release edit "\$RELEASE_TAG" --draft=false --prerelease/);
assert.match(release, /gh release edit "\$RELEASE_TAG" --draft=false --latest/);
console.log(`workflow contract passed for ${workflows.length} workflows`);
