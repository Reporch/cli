#!/usr/bin/env node

import { createHash } from "node:crypto";
import { existsSync, readFileSync, realpathSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";

const require = createRequire(import.meta.url);

export const PLATFORM_PACKAGES = Object.freeze({
  "darwin-arm64": "@reporch/cli-darwin-arm64",
  "darwin-x64": "@reporch/cli-darwin-x64",
  "linux-arm64": "@reporch/cli-linux-arm64-gnu",
  "linux-x64": "@reporch/cli-linux-x64-gnu",
  "win32-x64": "@reporch/cli-win32-x64-msvc"
});

export function packageFor(platform = process.platform, arch = process.arch) {
  const packageName = PLATFORM_PACKAGES[`${platform}-${arch}`];
  if (!packageName) {
    throw new Error(
      `Reporch CLI does not support ${platform}/${arch}. ` +
        "Supported targets: macOS arm64/x64, Linux glibc arm64/x64, and Windows x64."
    );
  }
  return packageName;
}

export function sha256File(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

export function verifyBinary(path, expectedSha256) {
  if (!/^[a-f0-9]{64}$/.test(expectedSha256)) {
    throw new Error("The installed Reporch CLI checksum manifest is invalid.");
  }
  const actual = sha256File(path);
  if (actual !== expectedSha256) {
    throw new Error(
      `Reporch CLI integrity check failed for ${path}. ` +
        "Remove the package and reinstall @reporch/cli from the npm registry."
    );
  }
}

export function resolveBinary({
  platform = process.platform,
  arch = process.arch,
  resolvePackage = (specifier) => require.resolve(specifier),
  checksums = JSON.parse(
    readFileSync(new URL("../checksums.json", import.meta.url), "utf8")
  )
} = {}) {
  const packageName = packageFor(platform, arch);
  let packageJson;
  try {
    packageJson = resolvePackage(`${packageName}/package.json`);
  } catch (error) {
    throw new Error(
      `The platform package ${packageName} is missing. ` +
        "Optional dependencies may have been disabled; reinstall @reporch/cli without --omit=optional.",
      { cause: error }
    );
  }
  const binaryName = platform === "win32" ? "reporch.exe" : "reporch";
  const binary = join(dirname(packageJson), "bin", binaryName);
  if (!existsSync(binary)) {
    throw new Error(`The platform package ${packageName} does not contain ${binaryName}.`);
  }
  const expectedSha256 = checksums[packageName];
  verifyBinary(binary, expectedSha256);
  return binary;
}

export async function run(argv = process.argv.slice(2)) {
  const binary = resolveBinary();
  const child = spawn(binary, argv, { stdio: "inherit", windowsHide: false });
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    process.on(signal, () => child.kill(signal));
  }
  return new Promise((resolveExit, reject) => {
    child.on("error", reject);
    child.on("exit", (code, signal) => {
      if (signal) {
        process.kill(process.pid, signal);
        return;
      }
      resolveExit(code ?? 1);
    });
  });
}

function isMainModule() {
  if (!process.argv[1]) return false;
  try {
    // npm exposes package bins through a symlink on Unix. Compare canonical
    // paths so importing this module stays side-effect free while the symlink
    // entry point still executes the native CLI.
    return realpathSync(resolve(process.argv[1])) === realpathSync(fileURLToPath(import.meta.url));
  } catch {
    return false;
  }
}

if (isMainModule()) {
  run()
    .then((code) => {
      process.exitCode = code;
    })
    .catch((error) => {
      console.error(`reporch: ${error.message}`);
      process.exitCode = 1;
    });
}
