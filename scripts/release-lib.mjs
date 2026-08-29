import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  chmodSync,
  copyFileSync,
  cpSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  writeFileSync
} from "node:fs";
import { basename, dirname, join, resolve } from "node:path";

export const TARGETS = Object.freeze([
  {
    target: "aarch64-apple-darwin",
    packageDirectory: "darwin-arm64",
    packageName: "@reporch/cli-darwin-arm64",
    binaryName: "reporch",
    runtimeTarget: "darwin-arm64"
  },
  {
    target: "x86_64-apple-darwin",
    packageDirectory: "darwin-x64",
    packageName: "@reporch/cli-darwin-x64",
    binaryName: "reporch",
    runtimeTarget: "darwin-x64"
  },
  {
    target: "aarch64-unknown-linux-gnu",
    packageDirectory: "linux-arm64-gnu",
    packageName: "@reporch/cli-linux-arm64-gnu",
    binaryName: "reporch",
    runtimeTarget: "linux-arm64-gnu"
  },
  {
    target: "x86_64-unknown-linux-gnu",
    packageDirectory: "linux-x64-gnu",
    packageName: "@reporch/cli-linux-x64-gnu",
    binaryName: "reporch",
    runtimeTarget: "linux-x64-gnu"
  },
  {
    target: "x86_64-pc-windows-msvc",
    packageDirectory: "win32-x64-msvc",
    packageName: "@reporch/cli-win32-x64-msvc",
    binaryName: "reporch.exe",
    runtimeTarget: "windows-x64-msvc"
  }
]);

export function npmTagForVersion(version) {
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?$/.test(version)) {
    throw new Error(`invalid release version: ${version}`);
  }
  return version.includes("-") ? "next" : "latest";
}

export function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

export function assertReleaseBinary(path) {
  const stat = lstatSync(path);
  if (!stat.isFile() || stat.isSymbolicLink()) {
    throw new Error(`release binary must be a regular non-symlink file: ${path}`);
  }
  if (stat.size < 100_000 || stat.size > 256 * 1024 * 1024) {
    throw new Error(`release binary has an unsafe size: ${path} (${stat.size})`);
  }
}

function safeRuntimeVersion(version) {
  return version.replace(/[^0-9A-Za-z._-]/g, "_");
}

function assertRegular(path, label, maximum = 4 * 1024 * 1024 * 1024) {
  const stat = lstatSync(path);
  assert.ok(stat.isFile() && !stat.isSymbolicLink(), `${label} must be a regular file`);
  assert.ok(stat.size > 0 && stat.size <= maximum, `${label} has an unsafe size`);
  return stat;
}

export function assertRuntimeInstallTree(path, expectedTarget) {
  const root = resolve(path);
  const rootStat = lstatSync(root);
  assert.ok(rootStat.isDirectory() && !rootStat.isSymbolicLink(), "runtime tree must be a directory");
  assert.deepEqual(readdirSync(root).sort(), ["bundles", "current.json"]);
  assertRegular(join(root, "current.json"), "runtime current.json", 64 * 1024);
  const current = JSON.parse(readFileSync(join(root, "current.json"), "utf8"));
  assert.equal(current.schema, "reporch.runtime-installation.v1");
  assert.equal(current.target, expectedTarget, "runtime installation target mismatch");
  assert.ok(Number.isSafeInteger(current.sequence) && current.sequence > 0);
  assert.match(current.version, /^[0-9A-Za-z._-]+$/);
  assert.match(current.bundle_sha256, /^sha256:[a-f0-9]{64}$/);

  const bundles = join(root, "bundles");
  const bundlesStat = lstatSync(bundles);
  assert.ok(bundlesStat.isDirectory() && !bundlesStat.isSymbolicLink());
  const bundleName = `${current.sequence}-${safeRuntimeVersion(current.version)}`;
  assert.deepEqual(readdirSync(bundles), [bundleName]);
  const bundle = join(bundles, bundleName);
  const bundleStat = lstatSync(bundle);
  assert.ok(bundleStat.isDirectory() && !bundleStat.isSymbolicLink());

  const manifestPath = join(bundle, "manifest.json");
  const signaturePath = join(bundle, "manifest.json.minisig");
  assertRegular(manifestPath, "runtime manifest", 256 * 1024);
  assertRegular(signaturePath, "runtime signature", 16 * 1024);
  assertRegular(join(bundle, ".complete"), "runtime completion marker", 256);
  const manifestBytes = readFileSync(manifestPath);
  assert.equal(`sha256:${createHash("sha256").update(manifestBytes).digest("hex")}`, current.bundle_sha256);
  assert.equal(readFileSync(join(bundle, ".complete"), "utf8"), `${current.bundle_sha256}\n`);
  const manifest = JSON.parse(manifestBytes);
  assert.equal(manifest.schema, "reporch.runtime-bundle-manifest.v1");
  assert.equal(manifest.target, expectedTarget, "runtime manifest target mismatch");
  assert.equal(manifest.sequence, current.sequence);
  assert.equal(manifest.version, current.version);
  assert.ok(Array.isArray(manifest.artifacts) && manifest.artifacts.length >= 3);
  const expectedEntries = new Set(["manifest.json", "manifest.json.minisig", ".complete"]);
  for (const artifact of manifest.artifacts) {
    assert.match(artifact.file_name, /^[0-9A-Za-z][0-9A-Za-z._-]{0,127}$/);
    assert.match(artifact.sha256, /^sha256:[a-f0-9]{64}$/);
    assert.ok(Number.isSafeInteger(artifact.size) && artifact.size > 0);
    assert.ok(!expectedEntries.has(artifact.file_name), "duplicate runtime artifact name");
    expectedEntries.add(artifact.file_name);
    const artifactPath = join(bundle, artifact.file_name);
    const stat = assertRegular(artifactPath, `runtime artifact ${artifact.file_name}`);
    assert.equal(stat.size, artifact.size, `runtime artifact ${artifact.file_name} size mismatch`);
    assert.equal(
      `sha256:${createHash("sha256").update(readFileSync(artifactPath)).digest("hex")}`,
      artifact.sha256,
      `runtime artifact ${artifact.file_name} hash mismatch`
    );
    if (process.platform !== "win32") {
      assert.equal(stat.mode & 0o222, 0, `runtime artifact ${artifact.file_name} must be read-only`);
    }
  }
  assert.deepEqual(new Set(readdirSync(bundle)), expectedEntries);
  return {
    root,
    sequence: current.sequence,
    version: current.version,
    manifestSha256: current.bundle_sha256
  };
}

export function copyRuntimeInstallTree(runtimeTree, destination, expectedTarget) {
  const verified = assertRuntimeInstallTree(runtimeTree, expectedTarget);
  mkdirSync(dirname(destination), { recursive: true, mode: 0o755 });
  cpSync(verified.root, destination, { recursive: true, errorOnExist: true });
  const copied = assertRuntimeInstallTree(destination, expectedTarget);
  assert.deepEqual(copied, { ...verified, root: resolve(destination) });
  return copied;
}

export function copyMetadata(destination) {
  copyFileSync("LICENSE", join(destination, "LICENSE"));
  copyFileSync("NOTICE", join(destination, "NOTICE"));
}

export function stagePlatformPackage(target, binary, runtimeTree, output) {
  assertReleaseBinary(binary);
  const destination = join(output, target.packageDirectory);
  cpSync(join("npm/platforms", target.packageDirectory), destination, {
    recursive: true,
    errorOnExist: true
  });
  copyFileSync("npm/platforms/README.md", join(destination, "README.md"));
  copyMetadata(destination);
  const binaryDirectory = join(destination, "bin");
  mkdirSync(binaryDirectory);
  const stagedBinary = join(binaryDirectory, target.binaryName);
  copyFileSync(binary, stagedBinary);
  if (target.binaryName === "reporch") chmodSync(stagedBinary, 0o755);
  const runtime = copyRuntimeInstallTree(
    runtimeTree,
    join(binaryDirectory, "runtime", target.runtimeTarget),
    target.runtimeTarget
  );
  return { destination, stagedBinary, sha256: sha256(stagedBinary), runtime };
}

export function stageRootPackage(checksums, output) {
  const destination = join(output, "cli");
  cpSync("npm/cli", destination, { recursive: true, errorOnExist: true });
  copyFileSync("README.md", join(destination, "README.md"));
  copyMetadata(destination);
  writeFileSync(join(destination, "checksums.json"), `${JSON.stringify(checksums, null, 2)}\n`);
  chmodSync(join(destination, "bin/reporch.mjs"), 0o755);
  return destination;
}

export function validateOutputArgument(path) {
  const absolute = resolve(path);
  const cwd = resolve(".");
  if (absolute === cwd || absolute === resolve("/") || basename(absolute).length < 3) {
    throw new Error(`refusing unsafe release output path: ${absolute}`);
  }
  return absolute;
}
