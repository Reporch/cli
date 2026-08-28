#!/usr/bin/env node

import { createHash } from "node:crypto";
import {
  chmodSync,
  closeSync,
  constants as fsConstants,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  realpathSync,
  renameSync,
  rmSync,
  unlinkSync,
  writeFileSync
} from "node:fs";
import { createRequire } from "node:module";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";
import { gunzipSync } from "node:zlib";

const require = createRequire(import.meta.url);
const PACKAGE_VERSION = require("../package.json").version;
const MAX_DOWNLOAD_BYTES = 128 * 1024 * 1024;
const MAX_UNPACKED_BYTES = 256 * 1024 * 1024;
const MAX_TAR_ENTRIES = 4096;
const LOCK_WAIT_MS = 30_000;

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

export class PlatformPackageMissingError extends Error {
  constructor(packageName, cause) {
    super(
      `The platform package ${packageName} is missing. ` +
        "Reporch will recover the exact signed release binary before continuing.",
      { cause }
    );
    this.name = "PlatformPackageMissingError";
    this.packageName = packageName;
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
    throw new PlatformPackageMissingError(packageName, error);
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

function cacheRoot({ platform = process.platform, env = process.env } = {}) {
  if (env.REPORCH_NPM_BOOTSTRAP_CACHE) {
    return resolve(env.REPORCH_NPM_BOOTSTRAP_CACHE);
  }
  if (platform === "win32") {
    return join(env.LOCALAPPDATA || tmpdir(), "Reporch", "Bootstrap");
  }
  if (platform === "darwin") {
    return join(homedir(), "Library", "Caches", "Reporch", "Bootstrap");
  }
  return join(env.XDG_CACHE_HOME || join(homedir(), ".cache"), "reporch", "bootstrap");
}

function packageTarballUrl(packageName, version = PACKAGE_VERSION) {
  const encoded = packageName.replace("/", "%2f");
  const basename = packageName.slice(packageName.indexOf("/") + 1);
  return new URL(
    `https://registry.npmjs.org/${encoded}/-/${basename}-${version}.tgz`
  );
}

function parseOctal(header, start, length, label) {
  const value = header
    .subarray(start, start + length)
    .toString("ascii")
    .replace(/\0.*$/s, "")
    .trim();
  if (!/^[0-7]*$/.test(value)) {
    throw new Error(`The recovered npm tarball has an invalid ${label}.`);
  }
  return value === "" ? 0 : Number.parseInt(value, 8);
}

function verifyTarChecksum(header) {
  const expected = parseOctal(header, 148, 8, "header checksum");
  let actual = 0;
  for (let index = 0; index < 512; index += 1) {
    actual += index >= 148 && index < 156 ? 0x20 : header[index];
  }
  if (actual !== expected) {
    throw new Error("The recovered npm tarball failed its header checksum.");
  }
}

export function extractPlatformBinary(tarball, binaryName) {
  if (!Buffer.isBuffer(tarball) || tarball.length === 0 || tarball.length > MAX_DOWNLOAD_BYTES) {
    throw new Error("The recovered npm package has an invalid size.");
  }
  const archive = gunzipSync(tarball, { maxOutputLength: MAX_UNPACKED_BYTES });
  const expectedPath = `package/bin/${binaryName}`;
  let selected;
  let offset = 0;
  let entries = 0;
  while (offset + 512 <= archive.length) {
    const header = archive.subarray(offset, offset + 512);
    if (header.every((byte) => byte === 0)) break;
    entries += 1;
    if (entries > MAX_TAR_ENTRIES) {
      throw new Error("The recovered npm tarball contains too many entries.");
    }
    verifyTarChecksum(header);
    const name = header.subarray(0, 100).toString("utf8").replace(/\0.*$/s, "");
    const prefix = header.subarray(345, 500).toString("utf8").replace(/\0.*$/s, "");
    const path = prefix ? `${prefix}/${name}` : name;
    if (
      !path ||
      path.startsWith("/") ||
      path.includes("\\") ||
      path.split("/").some((part) => part === "" || part === "." || part === "..")
    ) {
      throw new Error("The recovered npm tarball contains an unsafe path.");
    }
    const size = parseOctal(header, 124, 12, "entry size");
    if (!Number.isSafeInteger(size) || size < 0 || size > MAX_UNPACKED_BYTES) {
      throw new Error("The recovered npm tarball contains an oversized entry.");
    }
    const type = String.fromCharCode(header[156] || 0);
    if (!["\0", "0", "5"].includes(type)) {
      throw new Error("The recovered npm tarball contains a link or unsupported entry.");
    }
    const contentsStart = offset + 512;
    const contentsEnd = contentsStart + size;
    if (contentsEnd > archive.length) {
      throw new Error("The recovered npm tarball is truncated.");
    }
    if (path === expectedPath) {
      if (type !== "\0" && type !== "0") {
        throw new Error("The recovered Reporch binary is not a regular file.");
      }
      if (selected) {
        throw new Error("The recovered npm tarball contains duplicate Reporch binaries.");
      }
      selected = Buffer.from(archive.subarray(contentsStart, contentsEnd));
    }
    offset = contentsStart + Math.ceil(size / 512) * 512;
  }
  if (!selected || selected.length === 0 || selected.length > 64 * 1024 * 1024) {
    throw new Error(`The recovered npm tarball does not contain ${expectedPath}.`);
  }
  return selected;
}

async function boundedDownload(url, fetchImpl = globalThis.fetch) {
  const response = await fetchImpl(url, {
    redirect: "follow",
    headers: { "user-agent": `reporch-cli/${PACKAGE_VERSION}` }
  });
  if (!response.ok) {
    throw new Error(`Reporch binary recovery failed with HTTP ${response.status}.`);
  }
  const finalUrl = new URL(response.url || url);
  if (finalUrl.protocol !== "https:" || finalUrl.username || finalUrl.password) {
    throw new Error("Reporch binary recovery was redirected to an unsafe URL.");
  }
  const declared = Number(response.headers.get("content-length"));
  if (Number.isFinite(declared) && (declared <= 0 || declared > MAX_DOWNLOAD_BYTES)) {
    throw new Error("The recovered npm package has an invalid declared size.");
  }
  const reader = response.body?.getReader();
  if (!reader) throw new Error("Reporch binary recovery returned no body.");
  const chunks = [];
  let total = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > MAX_DOWNLOAD_BYTES) {
      await reader.cancel();
      throw new Error("The recovered npm package exceeded the download limit.");
    }
    chunks.push(Buffer.from(value));
  }
  if (total === 0) throw new Error("The recovered npm package was empty.");
  return Buffer.concat(chunks, total);
}

function sleep(milliseconds) {
  return new Promise((resolveSleep) => setTimeout(resolveSleep, milliseconds));
}

async function withBootstrapLock(lockPath, operation) {
  const deadline = Date.now() + LOCK_WAIT_MS;
  while (true) {
    try {
      const descriptor = openSync(
        lockPath,
        fsConstants.O_CREAT | fsConstants.O_EXCL | fsConstants.O_WRONLY,
        0o600
      );
      try {
        writeFileSync(descriptor, `${process.pid}\n`);
      } finally {
        closeSync(descriptor);
      }
    } catch (error) {
      if (error?.code !== "EEXIST") throw error;
      if (Date.now() >= deadline) {
        throw new Error("Timed out waiting for another Reporch bootstrap process.");
      }
      await sleep(100);
      continue;
    }
    try {
      return await operation();
    } finally {
      try { unlinkSync(lockPath); } catch {}
    }
  }
}

export async function resolveOrRecoverBinary({
  platform = process.platform,
  arch = process.arch,
  resolvePackage = (specifier) => require.resolve(specifier),
  checksums = JSON.parse(readFileSync(new URL("../checksums.json", import.meta.url), "utf8")),
  fetchImpl = globalThis.fetch,
  cacheDirectory = cacheRoot({ platform })
} = {}) {
  try {
    return resolveBinary({ platform, arch, resolvePackage, checksums });
  } catch (error) {
    if (!(error instanceof PlatformPackageMissingError)) throw error;
  }
  const packageName = packageFor(platform, arch);
  const expectedSha256 = checksums[packageName];
  if (!/^[a-f0-9]{64}$/.test(expectedSha256)) {
    throw new Error("The installed Reporch CLI checksum manifest is invalid.");
  }
  const target = `${platform}-${arch}`;
  const directory = join(cacheDirectory, PACKAGE_VERSION, target);
  const binaryName = platform === "win32" ? "reporch.exe" : "reporch";
  const binary = join(directory, binaryName);
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  try {
    verifyBinary(binary, expectedSha256);
    return binary;
  } catch {}
  return withBootstrapLock(join(directory, ".bootstrap.lock"), async () => {
    try {
      verifyBinary(binary, expectedSha256);
      return binary;
    } catch {}
    const tarball = await boundedDownload(packageTarballUrl(packageName), fetchImpl);
    const contents = extractPlatformBinary(tarball, binaryName);
    const actual = createHash("sha256").update(contents).digest("hex");
    if (actual !== expectedSha256) {
      throw new Error("The recovered Reporch binary failed its embedded checksum.");
    }
    const temporary = join(directory, `.reporch-${process.pid}-${Date.now()}.tmp`);
    writeFileSync(temporary, contents, { mode: 0o700, flag: "wx" });
    try {
      chmodSync(temporary, 0o700);
      renameSync(temporary, binary);
    } finally {
      try { rmSync(temporary, { force: true }); } catch {}
    }
    verifyBinary(binary, expectedSha256);
    return binary;
  });
}

export async function run(argv = process.argv.slice(2)) {
  const binary = await resolveOrRecoverBinary();
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
