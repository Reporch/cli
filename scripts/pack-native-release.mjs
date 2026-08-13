import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  renameSync,
  rmSync,
  utimesSync,
  writeFileSync
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

import {
  TARGETS,
  assertReleaseBinary,
  sha256,
  validateOutputArgument
} from "./release-lib.mjs";

const NORMALIZED_TIME = new Date("1980-01-01T00:00:00.000Z");

export function nativeArchiveName(version, target) {
  assert.match(version, /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/);
  assert.ok(TARGETS.some((candidate) => candidate.target === target.target));
  const extension = target.target.includes("windows") ? "zip" : "tar.gz";
  return `reporch-v${version}-${target.target}.${extension}`;
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: "utf8", ...options });
  assert.equal(result.status, 0, result.stderr || result.stdout || `${command} failed`);
}

function normalize(path, mode) {
  chmodSync(path, mode);
  utimesSync(path, NORMALIZED_TIME, NORMALIZED_TIME);
}

function stageArchiveDirectory(root, directoryName, binary, binaryName) {
  const directory = join(root, directoryName);
  mkdirSync(directory, { mode: 0o755 });
  const entries = [
    ["LICENSE", "LICENSE", 0o644],
    ["NOTICE", "NOTICE", 0o644],
    ["README.md", "README.md", 0o644],
    [binary, binaryName, 0o755]
  ];
  for (const [source, name, mode] of entries) {
    const destination = join(directory, name);
    copyFileSync(source, destination);
    normalize(destination, mode);
  }
  normalize(directory, 0o755);
  return directory;
}

function createTarGz(stagingRoot, directoryName, output) {
  const tarPath = join(stagingRoot, `${directoryName}.tar`);
  run(
    "tar",
    [
      "--format=ustar",
      "--sort=name",
      "--mtime=@315532800",
      "--owner=0",
      "--group=0",
      "--numeric-owner",
      "-cf",
      tarPath,
      directoryName
    ],
    { cwd: stagingRoot }
  );
  run("gzip", ["-n", "-9", tarPath], { cwd: stagingRoot });
  renameSync(`${tarPath}.gz`, output);
}

function createZip(stagingRoot, directoryName, binaryName, output) {
  const entries = ["LICENSE", "NOTICE", "README.md", binaryName].map((name) =>
    join(directoryName, name)
  );
  run("zip", ["-X", "-9", output, ...entries], { cwd: stagingRoot });
}

export function packNativeRelease(artifactsArgument, outputArgument) {
  const artifacts = resolve(artifactsArgument);
  const output = validateOutputArgument(outputArgument);
  if (!existsSync(artifacts)) {
    throw new Error(`artifact directory does not exist: ${artifacts}`);
  }
  if (existsSync(output)) {
    throw new Error(`native release output already exists: ${output}`);
  }
  mkdirSync(output, { recursive: false, mode: 0o755 });

  const version = JSON.parse(readFileSync("npm/cli/package.json", "utf8")).version;
  const stagingRoot = mkdtempSync(join(tmpdir(), "reporch-native-release-"));
  const archives = [];
  try {
    for (const target of TARGETS) {
      const binary = join(artifacts, target.target, target.binaryName);
      assertReleaseBinary(binary);
      const archiveName = nativeArchiveName(version, target);
      const directoryName = archiveName.replace(/\.(?:tar\.gz|zip)$/, "");
      stageArchiveDirectory(stagingRoot, directoryName, binary, target.binaryName);
      const archive = join(output, archiveName);
      if (archiveName.endsWith(".zip")) {
        createZip(stagingRoot, directoryName, target.binaryName, archive);
      } else {
        createTarGz(stagingRoot, directoryName, archive);
      }
      const stat = lstatSync(archive);
      assert.ok(stat.isFile() && !stat.isSymbolicLink() && stat.size > 100_000);
      archives.push({
        target: target.target,
        filename: archiveName,
        binary: target.binaryName,
        binarySha256: sha256(binary),
        archiveSha256: sha256(archive),
        size: stat.size
      });
    }
  } finally {
    rmSync(stagingRoot, { recursive: true, force: true });
  }

  writeFileSync(
    join(output, "native-release-manifest.json"),
    `${JSON.stringify(
      {
        schema: "reporch.cli-native-release.v1",
        version,
        sourceRevision: process.env.GITHUB_SHA ?? null,
        archives
      },
      null,
      2
    )}\n`
  );
  return { version, output, archives };
}

function main() {
  const [artifacts, output] = process.argv.slice(2);
  if (!artifacts || !output) {
    throw new Error("usage: node scripts/pack-native-release.mjs <artifacts> <new-output>");
  }
  const result = packNativeRelease(artifacts, output);
  console.log(`packed ${result.archives.length} standalone Reporch CLI archives`);
}

const invokedPath = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : null;
if (invokedPath === import.meta.url) main();
