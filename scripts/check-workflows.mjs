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
assert.doesNotMatch(
  release,
  /^\s*-\s+run:\s+npm install --global/m,
  "the privileged release job must not execute an unverified registry bootstrap"
);
assert.match(release, /npm-11\.18\.0\.tgz/);
assert.match(release, /NPM_TARBALL_SHA256:\s*[a-f0-9]{64}/);
assert.match(release, /sha256sum --check --strict/);
assert.doesNotMatch(
  release,
  /echo "\$install_dir\/package\/bin" >> "\$GITHUB_PATH"/,
  "the npm tarball wrapper must not be used outside a Node installation prefix"
);
assert.match(
  release,
  /ln -s "\$install_dir\/package\/bin\/npm-cli\.js" "\$shim_dir\/npm"/,
  "the pinned npm client must invoke npm-cli.js through a dedicated shim"
);
assert.match(
  release,
  /test "\$\("\$shim_dir\/npm" --version\)" = "11\.18\.0"/,
  "the release must exercise the exact pinned npm shim before publishing"
);
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
  /cmp "\$native" "\$npm_native"/,
  "published npm binaries must match the independently attested GitHub release bytes"
);
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
assert.match(release, /qualify-published-artifacts:/);
assert.match(release, /start-beta-window:/);
assert.match(release, /node scripts\/verify-stability-window\.mjs/);
const stability = readFileSync(".github/workflows/rc-stability.yml", "utf8");
assert.match(stability, /schedule:/);
assert.match(stability, /@reporch\/cli@\$VERSION/);
assert.match(stability, /reporch\.studio-capabilities\.v1/);
assert.match(stability, /reporch\.authoring-spec\.v2/);
assert.match(stability, /reporch-cli-stability:/);
assert.match(stability, /retention-days: 90/);
assert.match(stability, /persist-credentials: false/);
assert.doesNotMatch(
  stability,
  /jobs:\n\s+monitor:\n(?:.|\n)*?timeout-minutes: 20\n\s+env:\n\s+GH_TOKEN:/,
  "the npm dogfood process must not inherit an issue-write token"
);
console.log(`workflow contract passed for ${workflows.length} workflows`);
