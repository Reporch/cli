import { createHash } from "node:crypto";
import {
  chmodSync,
  copyFileSync,
  cpSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  writeFileSync
} from "node:fs";
import { basename, join, resolve } from "node:path";

export const TARGETS = Object.freeze([
  {
    target: "aarch64-apple-darwin",
    packageDirectory: "darwin-arm64",
    packageName: "@reporch/cli-darwin-arm64",
    binaryName: "reporch"
  },
  {
    target: "x86_64-apple-darwin",
    packageDirectory: "darwin-x64",
    packageName: "@reporch/cli-darwin-x64",
    binaryName: "reporch"
  },
  {
    target: "aarch64-unknown-linux-gnu",
    packageDirectory: "linux-arm64-gnu",
    packageName: "@reporch/cli-linux-arm64-gnu",
    binaryName: "reporch"
  },
  {
    target: "x86_64-unknown-linux-gnu",
    packageDirectory: "linux-x64-gnu",
    packageName: "@reporch/cli-linux-x64-gnu",
    binaryName: "reporch"
  },
  {
    target: "x86_64-pc-windows-msvc",
    packageDirectory: "win32-x64-msvc",
    packageName: "@reporch/cli-win32-x64-msvc",
    binaryName: "reporch.exe"
  }
]);

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

export function copyMetadata(destination) {
  copyFileSync("LICENSE", join(destination, "LICENSE"));
  copyFileSync("NOTICE", join(destination, "NOTICE"));
}

export function stagePlatformPackage(target, binary, output) {
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
  return { destination, stagedBinary, sha256: sha256(stagedBinary) };
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
