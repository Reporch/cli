#!/usr/bin/env node

import { createHash } from "node:crypto";
import {
  chmodSync,
  closeSync,
  constants as fsConstants,
  existsSync,
  lstatSync,
  mkdirSync,
  openSync,
  readFileSync,
  realpathSync,
  readdirSync,
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
const MAX_DOWNLOAD_BYTES = 512 * 1024 * 1024;
const MAX_UNPACKED_BYTES = 1024 * 1024 * 1024;
const MAX_TAR_ENTRIES = 4096;
const LOCK_WAIT_MS = 30_000;

export const PLATFORM_PACKAGES = Object.freeze({
  "darwin-arm64": "@reporch/cli-darwin-arm64",
  "darwin-x64": "@reporch/cli-darwin-x64",
  "linux-arm64": "@reporch/cli-linux-arm64-gnu",
  "linux-x64": "@reporch/cli-linux-x64-gnu",
  "win32-x64": "@reporch/cli-win32-x64-msvc"
});

const RUNTIME_TARGETS = Object.freeze({
  "darwin-arm64": "darwin-arm64",
  "darwin-x64": "darwin-x64",
  "linux-arm64": "linux-arm64-gnu",
  "linux-x64": "linux-x64-gnu",
  "win32-x64": "windows-x64-msvc"
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
  const binaryDirectory = join(dirname(packageJson), "bin");
  const binary = join(binaryDirectory, binaryName);
  if (!existsSync(binary)) {
    throw new PlatformPackageMissingError(
      packageName,
      new Error(`the installed platform package does not contain ${binaryName}`)
    );
  }
  const expectedSha256 = checksums[packageName];
  verifyBinary(binary, expectedSha256);
  const runtimeTarget = RUNTIME_TARGETS[`${platform}-${arch}`];
  if (!existsSync(join(binaryDirectory, "runtime", runtimeTarget, "current.json"))) {
    throw new PlatformPackageMissingError(
      packageName,
      new Error("the platform package omitted its mandatory runtime tree")
    );
  }
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

function scanPlatformTarball(tarball, binaryName, runtimeTarget) {
  if (!Buffer.isBuffer(tarball) || tarball.length === 0 || tarball.length > MAX_DOWNLOAD_BYTES) {
    throw new Error("The recovered npm package has an invalid size.");
  }
  const archive = gunzipSync(tarball, { maxOutputLength: MAX_UNPACKED_BYTES });
  const expectedPath = `package/bin/${binaryName}`;
  const runtimePrefix = runtimeTarget ? `package/bin/runtime/${runtimeTarget}/` : null;
  const selected = new Map();
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
    const rawPath = prefix ? `${prefix}/${name}` : name;
    const size = parseOctal(header, 124, 12, "entry size");
    const type = String.fromCharCode(header[156] || 0);
    const path = type === "5" && rawPath.endsWith("/") ? rawPath.slice(0, -1) : rawPath;
    if (
      !path ||
      path.startsWith("/") ||
      path.includes("\\") ||
      path.split("/").some((part) => part === "" || part === "." || part === "..")
    ) {
      throw new Error("The recovered npm tarball contains an unsafe path.");
    }
    if (!Number.isSafeInteger(size) || size < 0 || size > MAX_UNPACKED_BYTES) {
      throw new Error("The recovered npm tarball contains an oversized entry.");
    }
    if (!["\0", "0", "5"].includes(type)) {
      throw new Error("The recovered npm tarball contains a link or unsupported entry.");
    }
    const contentsStart = offset + 512;
    const contentsEnd = contentsStart + size;
    if (contentsEnd > archive.length) {
      throw new Error("The recovered npm tarball is truncated.");
    }
    if (path === expectedPath || (runtimePrefix && path.startsWith(runtimePrefix))) {
      if (type !== "\0" && type !== "0") {
        throw new Error("The recovered Reporch payload contains a non-regular selected file.");
      }
      const relative = path.slice("package/bin/".length);
      if (selected.has(relative)) {
        throw new Error("The recovered npm tarball contains a duplicate selected file.");
      }
      selected.set(relative, {
        contents: Buffer.from(archive.subarray(contentsStart, contentsEnd)),
        mode: parseOctal(header, 100, 8, "entry mode")
      });
    }
    offset = contentsStart + Math.ceil(size / 512) * 512;
  }
  const binary = selected.get(binaryName)?.contents;
  if (!binary || binary.length === 0 || binary.length > 64 * 1024 * 1024) {
    throw new Error(`The recovered npm tarball does not contain ${expectedPath}.`);
  }
  return selected;
}

export function extractPlatformBinary(tarball, binaryName) {
  return scanPlatformTarball(tarball, binaryName, null).get(binaryName).contents;
}

export function extractPlatformPayload(tarball, binaryName, runtimeTarget) {
  const selected = scanPlatformTarball(tarball, binaryName, runtimeTarget);
  validateRuntimePayload(selected, runtimeTarget);
  return selected;
}

function validateRuntimePayload(payload, runtimeTarget) {
  const prefix = `runtime/${runtimeTarget}`;
  const currentEntry = payload.get(`${prefix}/current.json`);
  if (!currentEntry || currentEntry.contents.length > 64 * 1024) {
    throw new Error("The recovered npm package has no bounded runtime installation record.");
  }
  let current;
  try {
    current = JSON.parse(currentEntry.contents);
  } catch {
    throw new Error("The recovered runtime installation record is invalid JSON.");
  }
  if (
    current?.schema !== "reporch.runtime-installation.v1" ||
    current?.target !== runtimeTarget ||
    !Number.isSafeInteger(current?.sequence) ||
    current.sequence <= 0 ||
    !/^[0-9A-Za-z._-]+$/.test(current?.version ?? "") ||
    !/^sha256:[a-f0-9]{64}$/.test(current?.bundle_sha256 ?? "")
  ) {
    throw new Error("The recovered runtime installation record has an invalid identity.");
  }
  const bundle = `${prefix}/bundles/${current.sequence}-${current.version}`;
  const manifestEntry = payload.get(`${bundle}/manifest.json`);
  const signatureEntry = payload.get(`${bundle}/manifest.json.minisig`);
  const completionEntry = payload.get(`${bundle}/.complete`);
  if (
    !manifestEntry ||
    manifestEntry.contents.length === 0 ||
    manifestEntry.contents.length > 256 * 1024 ||
    !signatureEntry ||
    signatureEntry.contents.length === 0 ||
    signatureEntry.contents.length > 16 * 1024 ||
    !completionEntry ||
    completionEntry.contents.length === 0 ||
    completionEntry.contents.length > 256
  ) {
    throw new Error("The recovered runtime bundle is incomplete.");
  }
  const digest = `sha256:${createHash("sha256").update(manifestEntry.contents).digest("hex")}`;
  if (digest !== current.bundle_sha256 || completionEntry.contents.toString() !== `${digest}\n`) {
    throw new Error("The recovered runtime bundle manifest digest is inconsistent.");
  }
  let manifest;
  try {
    manifest = JSON.parse(manifestEntry.contents);
  } catch {
    throw new Error("The recovered runtime manifest is invalid JSON.");
  }
  if (
    manifest?.schema !== "reporch.runtime-bundle-manifest.v1" ||
    manifest?.target !== runtimeTarget ||
    manifest?.sequence !== current.sequence ||
    manifest?.version !== current.version ||
    !Array.isArray(manifest?.artifacts) ||
    manifest.artifacts.length < 3 ||
    manifest.artifacts.length > 32
  ) {
    throw new Error("The recovered runtime manifest does not match its installation.");
  }
  const expected = new Set([
    `${prefix}/current.json`,
    `${bundle}/manifest.json`,
    `${bundle}/manifest.json.minisig`,
    `${bundle}/.complete`
  ]);
  for (const artifact of manifest.artifacts) {
    if (
      !/^[0-9A-Za-z][0-9A-Za-z._-]{0,127}$/.test(artifact?.file_name ?? "") ||
      !/^sha256:[a-f0-9]{64}$/.test(artifact?.sha256 ?? "") ||
      !Number.isSafeInteger(artifact?.size) ||
      artifact.size <= 0
    ) {
      throw new Error("The recovered runtime manifest contains an invalid artifact.");
    }
    const path = `${bundle}/${artifact.file_name}`;
    if (expected.has(path)) throw new Error("The recovered runtime manifest has duplicate files.");
    expected.add(path);
    const entry = payload.get(path);
    const actual = entry
      ? `sha256:${createHash("sha256").update(entry.contents).digest("hex")}`
      : null;
    if (!entry || entry.contents.length !== artifact.size || actual !== artifact.sha256) {
      throw new Error(`The recovered runtime artifact ${artifact.file_name} failed integrity.`);
    }
  }
  const selectedRuntimeFiles = [...payload.keys()].filter((path) => path.startsWith(`${prefix}/`));
  if (selectedRuntimeFiles.length !== expected.size || selectedRuntimeFiles.some((path) => !expected.has(path))) {
    throw new Error("The recovered runtime tree contains undeclared files.");
  }
  return { current, manifest, bundle };
}

function verifyCachedPayload(directory, binaryName, runtimeTarget, expectedSha256) {
  const binary = join(directory, binaryName);
  verifyBinary(binary, expectedSha256);
  const payload = new Map([[binaryName, { contents: readFileSync(binary), mode: 0o700 }]]);
  const runtime = join(directory, "runtime", runtimeTarget);
  let files = 0;
  let bytes = 0;
  const visit = (path, relative) => {
    const stat = lstatSync(path);
    if (stat.isSymbolicLink()) throw new Error("The cached Reporch runtime contains a symlink.");
    if (stat.isDirectory()) {
      for (const name of readdirSync(path).sort()) visit(join(path, name), `${relative}/${name}`);
      return;
    }
    if (!stat.isFile()) throw new Error("The cached Reporch runtime contains a special file.");
    files += 1;
    bytes += stat.size;
    if (files > 64 || bytes > MAX_UNPACKED_BYTES) {
      throw new Error("The cached Reporch runtime exceeds its bounds.");
    }
    payload.set(`runtime/${runtimeTarget}${relative}`, {
      contents: readFileSync(path),
      mode: stat.mode
    });
  };
  visit(runtime, "");
  validateRuntimePayload(payload, runtimeTarget);
  return binary;
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
  const runtimeTarget = RUNTIME_TARGETS[`${platform}-${arch}`];
  const expectedSha256 = checksums[packageName];
  if (!/^[a-f0-9]{64}$/.test(expectedSha256)) {
    throw new Error("The installed Reporch CLI checksum manifest is invalid.");
  }
  const target = `${platform}-${arch}`;
  const versionDirectory = join(cacheDirectory, PACKAGE_VERSION);
  const directory = join(versionDirectory, target);
  const binaryName = platform === "win32" ? "reporch.exe" : "reporch";
  const binary = join(directory, binaryName);
  mkdirSync(versionDirectory, { recursive: true, mode: 0o700 });
  try {
    return verifyCachedPayload(directory, binaryName, runtimeTarget, expectedSha256);
  } catch {}
  return withBootstrapLock(join(versionDirectory, `.${target}.bootstrap.lock`), async () => {
    try {
      return verifyCachedPayload(directory, binaryName, runtimeTarget, expectedSha256);
    } catch {}
    const tarball = await boundedDownload(packageTarballUrl(packageName), fetchImpl);
    const payload = extractPlatformPayload(tarball, binaryName, runtimeTarget);
    const actual = createHash("sha256").update(payload.get(binaryName).contents).digest("hex");
    if (actual !== expectedSha256) {
      throw new Error("The recovered Reporch binary failed its embedded checksum.");
    }
    const temporary = join(versionDirectory, `.${target}-${process.pid}-${Date.now()}.tmp`);
    mkdirSync(temporary, { mode: 0o700 });
    try {
      for (const [relative, entry] of [...payload.entries()].sort(([left], [right]) => left.localeCompare(right))) {
        const destination = join(temporary, ...relative.split("/"));
        mkdirSync(dirname(destination), { recursive: true, mode: 0o700 });
        writeFileSync(destination, entry.contents, { mode: 0o400, flag: "wx" });
      }
      chmodSync(join(temporary, binaryName), 0o700);
      const { manifest, bundle } = validateRuntimePayload(payload, runtimeTarget);
      for (const artifact of manifest.artifacts) {
        const executable = ["guest_agent", "host_service", "virtual_machine_monitor", "jailer"]
          .includes(artifact.kind);
        chmodSync(join(temporary, ...`${bundle}/${artifact.file_name}`.split("/")), executable ? 0o500 : 0o400);
      }
      if (existsSync(directory)) rmSync(directory, { recursive: true, force: true });
      renameSync(temporary, directory);
    } finally {
      try { rmSync(temporary, { recursive: true, force: true }); } catch {}
    }
    return verifyCachedPayload(directory, binaryName, runtimeTarget, expectedSha256);
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
