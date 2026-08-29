import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

const workflows = readdirSync(".github/workflows").filter((name) => /\.ya?ml$/.test(name));
const standardHostedRunners = new Set([
  "ubuntu-24.04",
  "ubuntu-24.04-arm",
  "macos-15",
  "macos-15-intel",
  "windows-2025"
]);
assert.ok(workflows.length >= 2, "CI and release workflows are required");
for (const name of workflows) {
  const content = readFileSync(join(".github/workflows", name), "utf8");
  assert.doesNotMatch(content, /pull_request_target:/, `${name} must not use pull_request_target`);
  assert.doesNotMatch(content, /NODE_AUTH_TOKEN|NPM_TOKEN/, `${name} must not use npm tokens`);
  for (const line of content.split("\n").filter((candidate) => /\bruns-on:/.test(candidate))) {
    if (/self-hosted/.test(line)) {
      assert.match(line, /cli-zero-cost/, `${name} omits the zero-cost runner guard: ${line.trim()}`);
      continue;
    }
    const runner = line
      .split(/\bruns-on:\s*/u, 2)[1]
      ?.trim()
      .replace(/^['"]|['"]$/gu, "");
    assert.ok(
      runner === "${{ matrix.runner }}" || standardHostedRunners.has(runner),
      `${name} selects a non-standard or billable hosted runner: ${line.trim()}`
    );
  }
  for (const match of content.matchAll(/^\s*-\s+runner:\s+([^\s#]+)\s*$/gmu)) {
    assert.ok(
      standardHostedRunners.has(match[1]),
      `${name} matrix selects a non-standard or billable hosted runner: ${match[1]}`
    );
  }
  assert.match(
    content,
    /github\.actor == vars\.SELF_HOSTED_ACTIONS_ALLOWED_ACTOR/,
    `${name} must restrict jobs to the configured trusted actor`
  );
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
assert.match(release, /shasum -a 256 --check/);
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
  /\(cd dist\/release-assets && shasum -a 256 \.\/\*\) > dist\/SHA256SUMS/,
  "release checksums must remain verifiable after downloading flat release assets"
);
assert.match(release, /shasum -a 256 --check/, "release checksum verification must be strict");
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
assert.match(ci, /actionlint_1\.7\.12_darwin_arm64\.tar\.gz/);
assert.match(ci, /ACTIONLINT_SHA256:\s*[a-f0-9]{64}/);
assert.match(ci, /shasum -a 256 --check/);
const publisher = readFileSync("scripts/publish-npm-release.mjs", "utf8");
assert.match(publisher, /"--tag",\s*npmTag/);
assert.match(release, /gh release edit "\$RELEASE_TAG" --draft=false --prerelease/);
assert.match(release, /gh release edit "\$RELEASE_TAG" --draft=false --latest/);
assert.match(release, /qualify-published-artifacts:/);
assert.match(
  release,
  /scripts\/qualify-installed-auth\.mjs/,
  "every future release must exercise installed Device OAuth and the OS credential store"
);
assert.match(
  release,
  /\.compatibility_report_count == 30/,
  "future releases must exercise all six problem types across all five package profiles"
);
assert.match(
  release,
  /name: installed-auth-\$\{\{ matrix\.target \}\}/,
  "installed authentication evidence must be retained per release target"
);
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
const publishedE2e = readFileSync(".github/workflows/published-artifact-e2e.yml", "utf8");
const runtimeRelease = readFileSync(".github/workflows/release-runtime.yml", "utf8");
const toolchainRelease = readFileSync(".github/workflows/release-toolchains.yml", "utf8");
assert.match(runtimeRelease, /build-runtime-candidates\.sh "\$RUNNER_TEMP\/runtime-candidates"/);
assert.match(runtimeRelease, /build-runtime-candidates\.sh "\$RUNNER_TEMP\/runtime-candidates-rebuild"/);
assert.match(runtimeRelease, /compare-runtime-candidates\.mjs/);
assert.match(runtimeRelease, /runtime-reproducibility\.json/);
assert.match(toolchainRelease, /materialize-toolchain-sources\.sh/);
assert.match(toolchainRelease, /syft[\s\S]*1\.51\.0/i);
assert.match(toolchainRelease, /build-toolchain-release-resumable\.sh/);
assert.match(toolchainRelease, /RUNNER_TOOL_CACHE\/reporch-toolchain-v2\/\$GITHUB_SHA/);
assert.match(toolchainRelease, /TOOLCHAIN_CHECKPOINT_ROOT/);
assert.doesNotMatch(toolchainRelease, /cp "\$RUNNER_TEMP\/toolchain-candidates"\/\*/);
assert.match(toolchainRelease, /REPORCH_RUNTIME_SIGNING_KEY/);
assert.match(toolchainRelease, /artifacts\/runtime-v1\.minisign\.pub/);
assert.match(toolchainRelease, /toolchain prefetch bash-5\.3/);
assert.match(toolchainRelease, /unset DOCKER_HOST CONTAINER_HOST/);
assert.match(toolchainRelease, /TOOLCHAIN-SHA256SUMS/);
assert.match(toolchainRelease, /subject-path: \$\{\{ runner\.temp \}\}\/toolchain-release\/\*/);
for (const target of [
  "aarch64-apple-darwin",
  "x86_64-apple-darwin",
  "aarch64-unknown-linux-gnu",
  "x86_64-unknown-linux-gnu",
  "x86_64-pc-windows-msvc",
]) {
  assert.match(
    publishedE2e,
    new RegExp(`target: ${target.replaceAll("-", "\\-")}`),
    `published-artifact E2E must preserve ${target}`
  );
}
assert.match(publishedE2e, /gh attestation verify/);
assert.match(
  publishedE2e,
  /if \[ "\$RUNNER_OS" = macOS \]; then\s+\(cd "\$assets" && printf '%s\\n' "\$checksum" \| shasum -a 256 --check\)/,
  "published macOS qualification must use the native shasum verifier"
);
assert.match(publishedE2e, /unset GH_TOKEN/);
assert.match(publishedE2e, /Remove-Item Env:GH_TOKEN/);
assert.match(
  publishedE2e,
  /\[System\.IO\.File\]::WriteAllText\([\s\S]*\(\(\$evidenceChecksums -join "`n"\) \+ "`n"\)/,
  "Windows evidence checksum manifests must use portable LF line endings"
);
assert.match(publishedE2e, /scripts\/qualify-installed-auth\.mjs/);
assert.match(publishedE2e, /dbus-run-session/);
assert.match(publishedE2e, /credential_store_round_trip/);
assert.match(
  publishedE2e,
  /\.compatibility_report_count == 30/,
  "published artifacts must prove the complete type/profile compatibility matrix"
);
assert.match(publishedE2e, /authenticated_studio_request/);
assert.match(publishedE2e, /retention-days: 90/);
const installedAuth = readFileSync("scripts/qualify-installed-auth.mjs", "utf8");
assert.match(installedAuth, /server\.listen\(0, "127\.0\.0\.1"/);
assert.match(installedAuth, /request\.headers\.authorization !== `Bearer \$\{accessToken\}`/);
assert.match(installedAuth, /REPORCH_STUDIO_ALLOW_INSECURE_HTTP: "true"/);
assert.match(installedAuth, /delete environment\[key\]/);
assert.match(installedAuth, /key\.startsWith\("REPORCH_"\)/);
assert.match(installedAuth, /REPORCH_CONFIG_HOME: configHome/);
assert.doesNotMatch(installedAuth, /0\.0\.0\.0/);
console.log(`workflow contract passed for ${workflows.length} workflows`);
