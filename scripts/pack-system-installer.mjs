import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  cpSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  symlinkSync,
  writeFileSync
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve, sep } from "node:path";
import { pathToFileURL } from "node:url";

import {
  TARGETS,
  assertReleaseBinary,
  assertRuntimeInstallTree,
  sha256,
  validateOutputArgument
} from "./release-lib.mjs";

const PRODUCT_GUID = "6A252985-916B-4C10-A3F4-3BAEF5C8DE08";

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: "utf8", ...options });
  assert.equal(result.status, 0, result.stderr || result.stdout || `${command} failed`);
}

function regular(path, label) {
  const stat = lstatSync(path);
  assert.ok(stat.isFile() && !stat.isSymbolicLink() && stat.size > 0, `${label} is invalid`);
  return path;
}

function portable(path) {
  return path.split(sep).join("/");
}

function copyExecutable(source, destination) {
  mkdirSync(dirname(destination), { recursive: true, mode: 0o755 });
  copyFileSync(regular(source, "installer executable"), destination);
  chmodSync(destination, 0o755);
}

function stripMacMetadata(root) {
  const records = [];
  const visit = (path) => {
    const stat = lstatSync(path);
    if (stat.isSymbolicLink()) return;
    assert.ok(stat.isDirectory() || stat.isFile(), `macOS installer rejects special file ${path}`);
    records.push({ path, mode: stat.mode & 0o7777 });
    chmodSync(path, (stat.mode & 0o7777) | 0o200);
    if (stat.isDirectory()) {
      for (const name of readdirSync(path).sort()) visit(join(path, name));
    }
  };
  visit(root);
  try {
    run("xattr", ["-cr", root]);
  } finally {
    for (const record of records.reverse()) chmodSync(record.path, record.mode);
  }
}

function runtimeArtifact(runtimeTree, expectedTarget, kind) {
  const verified = assertRuntimeInstallTree(runtimeTree, expectedTarget);
  const current = JSON.parse(readFileSync(join(verified.root, "current.json"), "utf8"));
  const bundle = join(
    verified.root,
    "bundles",
    `${current.sequence}-${current.version.replace(/[^0-9A-Za-z._-]/g, "_")}`
  );
  const manifest = JSON.parse(readFileSync(join(bundle, "manifest.json"), "utf8"));
  const artifact = manifest.artifacts.find((candidate) => candidate.kind === kind);
  assert.ok(artifact, `runtime ${expectedTarget} is missing ${kind}`);
  return regular(join(bundle, artifact.file_name), `runtime ${kind}`);
}

function copyDocumentation(root) {
  const destination = join(root, "usr", "share", "doc", "reporch-cli");
  mkdirSync(destination, { recursive: true, mode: 0o755 });
  copyFileSync("LICENSE", join(destination, "LICENSE"));
  copyFileSync("NOTICE", join(destination, "NOTICE"));
}

function stageUnixPayload(root, target, binary, runtimeTree, prefix) {
  const library = join(root, ...prefix.slice(1), "lib", "reporch", "bin");
  const commandDirectory = join(root, ...prefix.slice(1), "bin");
  mkdirSync(library, { recursive: true, mode: 0o755 });
  mkdirSync(commandDirectory, { recursive: true, mode: 0o755 });
  copyExecutable(binary, join(library, "reporch"));
  cpSync(runtimeTree, join(library, "runtime", target.runtimeTarget), {
    recursive: true,
    errorOnExist: true
  });
  symlinkSync("../lib/reporch/bin/reporch", join(commandDirectory, "reporch"));
  return { library, commandDirectory };
}

function stageLinuxPayload(root, target, binary, runtimeTree) {
  const staged = stageUnixPayload(root, target, binary, runtimeTree, ["", "usr"]);
  copyExecutable(
    runtimeArtifact(runtimeTree, target.runtimeTarget, "host_service"),
    join(staged.library, "reporch-runtime-service")
  );
  const unit = join(root, "usr", "lib", "systemd", "system", "reporch-runtime.service");
  mkdirSync(dirname(unit), { recursive: true, mode: 0o755 });
  copyFileSync("installers/linux/reporch-runtime.service", unit);
  copyDocumentation(root);
}

function stageMacPayload(root, target, binary, runtimeTree) {
  stageUnixPayload(root, target, binary, runtimeTree, ["", "usr", "local"]);
  const destination = join(root, "usr", "local", "share", "doc", "reporch-cli");
  mkdirSync(destination, { recursive: true, mode: 0o755 });
  copyFileSync("LICENSE", join(destination, "LICENSE"));
  copyFileSync("NOTICE", join(destination, "NOTICE"));
}

function debianVersion(version) {
  return version.includes("-") ? version.replace("-", "~") + "-1" : `${version}-1`;
}

function installerNumericVersion(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)(?:-rc\.(\d+))?$/.exec(version);
  assert.ok(match, `installer version is unsupported: ${version}`);
  const [, major, minor, patch, candidate] = match;
  const build = Number(patch) * 100 + (candidate ? Number(candidate) : 99);
  assert.ok(Number(major) <= 255 && Number(minor) <= 255 && build <= 65_535);
  return `${Number(major)}.${Number(minor)}.${build}`;
}

function packDeb(root, target, version, output) {
  const architecture = target.target.startsWith("aarch64") ? "arm64" : "amd64";
  const control = join(root, "DEBIAN");
  mkdirSync(control, { mode: 0o755 });
  writeFileSync(
    join(control, "control"),
    `Package: reporch-cli\nVersion: ${debianVersion(version)}\nArchitecture: ${architecture}\nMaintainer: Reporch Contributors\nSection: devel\nPriority: optional\nDepends: systemd, passwd, acl\nDescription: Reporch problem authoring CLI and isolated VM runtime\n`
  );
  for (const [source, name] of [
    ["installers/linux/postinstall.sh", "postinst"],
    ["installers/linux/preremove.sh", "prerm"]
  ]) {
    copyFileSync(source, join(control, name));
    chmodSync(join(control, name), 0o755);
  }
  const destination = join(output, `reporch-v${version}-${target.target}.deb`);
  run("dpkg-deb", ["--build", "--root-owner-group", root, destination]);
  return destination;
}

function rpmIdentity(version) {
  const [base, prerelease] = version.split("-", 2);
  return {
    version: base,
    release: prerelease ? `0.${prerelease.replace(/[^0-9A-Za-z.]/g, ".")}.1` : "1"
  };
}

function treeEntries(root, path = "") {
  const entries = [];
  for (const name of readdirSync(join(root, path)).sort()) {
    if (path === "" && name === "DEBIAN") continue;
    const child = join(path, name);
    const stat = lstatSync(join(root, child));
    if (stat.isDirectory()) entries.push(...treeEntries(root, child));
    else {
      assert.ok(stat.isFile() || stat.isSymbolicLink(), `installer payload has special file ${child}`);
      entries.push(`/${portable(child)}`);
    }
  }
  return entries;
}

function packRpm(root, target, version, output, work) {
  const architecture = target.target.startsWith("aarch64") ? "aarch64" : "x86_64";
  const identity = rpmIdentity(version);
  const top = join(work, "rpmbuild");
  for (const directory of ["BUILD", "BUILDROOT", "RPMS", "SOURCES", "SPECS", "SRPMS"]) {
    mkdirSync(join(top, directory), { recursive: true, mode: 0o755 });
  }
  run("tar", [
    "-C",
    root,
    "--exclude=./DEBIAN",
    "-czf",
    join(top, "SOURCES", "payload.tar.gz"),
    "."
  ]);
  const post = readFileSync("installers/linux/postinstall.sh", "utf8").replace(/^#!.*\n/, "");
  const preun = readFileSync("installers/linux/preremove.sh", "utf8").replace(/^#!.*\n/, "");
  const files = treeEntries(root).join("\n");
  const spec = `Name: reporch-cli\nVersion: ${identity.version}\nRelease: ${identity.release}\nSummary: Reporch problem authoring CLI and isolated VM runtime\nLicense: Apache-2.0\nURL: https://github.com/Reporch/cli\nSource0: payload.tar.gz\nBuildArch: ${architecture}\nRequires: systemd, shadow-utils, acl\nAutoReqProv: no\n\n%description\nReporch problem authoring CLI and isolated VM runtime.\n\n%prep\nmkdir payload\ntar -xzf %{SOURCE0} -C payload\n\n%install\nmkdir -p %{buildroot}\ncp -a payload/. %{buildroot}/\n\n%post\n${post}\n\n%preun\n${preun}\n\n%files\n${files}\n`;
  const specPath = join(top, "SPECS", "reporch-cli.spec");
  writeFileSync(specPath, spec);
  run("rpmbuild", ["--define", `_topdir ${top}`, "-bb", specPath]);
  const candidates = treeEntries(join(top, "RPMS"))
    .map((path) => join(top, "RPMS", path.slice(1)))
    .filter((path) => path.endsWith(".rpm"));
  assert.equal(candidates.length, 1, "rpmbuild did not produce exactly one RPM");
  const destination = join(output, `reporch-v${version}-${target.target}.rpm`);
  renameSync(candidates[0], destination);
  return destination;
}

function packMac(root, target, version, output, work) {
  stripMacMetadata(root);
  const scripts = join(work, "pkg-scripts");
  mkdirSync(scripts, { mode: 0o755 });
  writeFileSync(join(scripts, "postinstall"), "#!/bin/sh\nset -eu\nexit 0\n");
  chmodSync(join(scripts, "postinstall"), 0o755);
  const destination = join(output, `reporch-v${version}-${target.target}.pkg`);
  const args = [
    "--root",
    root,
    "--identifier",
    "com.reporch.cli",
    "--version",
    installerNumericVersion(version),
    "--install-location",
    "/",
    "--scripts",
    scripts,
    "--filter",
    "(^|/)\\._",
    "--filter",
    "(^|/)\\.DS_Store$"
  ];
  const identity = process.env.REPORCH_MAC_INSTALLER_IDENTITY;
  if (process.env.REPORCH_RELEASE_SIGNING_REQUIRED === "1") {
    assert.ok(identity, "REPORCH_MAC_INSTALLER_IDENTITY is required for release packaging");
  }
  if (identity) args.push("--sign", identity);
  args.push(destination);
  run("pkgbuild", args, { env: { ...process.env, COPYFILE_DISABLE: "1" } });
  return destination;
}

function xml(value) {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll('"', "&quot;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
}

function windowsDirectoryXml(root, path, componentIds) {
  const entries = readdirSync(join(root, path)).sort();
  return entries
    .map((name) => {
      const child = join(path, name);
      const source = join(root, child);
      const stat = lstatSync(source);
      assert.ok(!stat.isSymbolicLink(), `Windows installer rejects symlink ${child}`);
      if (stat.isDirectory()) {
        const id = child === "bin"
          ? "BINDIR"
          : `D${createHash("sha256").update(portable(child)).digest("hex").slice(0, 20)}`;
        return `<Directory Id="${id}" Name="${xml(name)}">${windowsDirectoryXml(root, child, componentIds)}</Directory>`;
      }
      assert.ok(stat.isFile(), `Windows installer rejects special file ${child}`);
      const id = `C${createHash("sha256").update(portable(child)).digest("hex").slice(0, 20)}`;
      componentIds.push(id);
      const extra = child === join("bin", "reporch.exe")
        ? '<Environment Id="ReporchPath" Name="PATH" Value="[BINDIR]" Action="set" Part="last" System="yes" />'
        : child === join("bin", "reporch-runtime-service.exe")
          ? '<ServiceInstall Id="RuntimeServiceInstall" Name="ReporchRuntime" DisplayName="Reporch Runtime" Description="Isolated Reporch VM execution broker" Type="ownProcess" Start="auto" ErrorControl="normal" Account="LocalSystem" /><ServiceControl Id="RuntimeServiceControl" Name="ReporchRuntime" Start="install" Stop="both" Remove="uninstall" Wait="yes" />'
          : "";
      return `<Component Id="${id}" Guid="*"><File Source="${xml(resolve(source))}" KeyPath="yes" />${extra}</Component>`;
    })
    .join("");
}

export function windowsInstallerXml(root, version) {
  const components = [];
  const contents = windowsDirectoryXml(root, "", components);
  const references = components.map((id) => `<ComponentRef Id="${id}" />`).join("");
  return `<?xml version="1.0" encoding="UTF-8"?>\n<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs"><Package Name="Reporch CLI" Manufacturer="Reporch" Version="${installerNumericVersion(version)}" UpgradeCode="${PRODUCT_GUID}" Scope="perMachine"><MajorUpgrade DowngradeErrorMessage="A newer Reporch CLI is already installed." /><MediaTemplate EmbedCab="yes" CompressionLevel="high" /><StandardDirectory Id="ProgramFiles64Folder"><Directory Id="INSTALLFOLDER" Name="Reporch">${contents}</Directory></StandardDirectory><Feature Id="MainFeature" Title="Reporch CLI" Level="1">${references}</Feature></Package></Wix>\n`;
}

function stageWindowsPayload(root, target, binary, runtimeTree) {
  const bin = join(root, "bin");
  mkdirSync(bin, { recursive: true, mode: 0o755 });
  copyExecutable(binary, join(bin, "reporch.exe"));
  copyExecutable(
    runtimeArtifact(runtimeTree, target.runtimeTarget, "host_service"),
    join(bin, "reporch-runtime-service.exe")
  );
  cpSync(runtimeTree, join(bin, "runtime", target.runtimeTarget), {
    recursive: true,
    errorOnExist: true
  });
  copyFileSync("LICENSE", join(root, "LICENSE"));
  copyFileSync("NOTICE", join(root, "NOTICE"));
}

function packWindows(root, target, version, output, work) {
  const source = join(work, "reporch.wxs");
  writeFileSync(source, windowsInstallerXml(root, version));
  const destination = join(output, `reporch-v${version}-${target.target}.msi`);
  run("wix", ["build", "-arch", "x64", "-o", destination, source]);
  const certificate = process.env.REPORCH_WINDOWS_CERT_SHA1;
  if (process.env.REPORCH_RELEASE_SIGNING_REQUIRED === "1") {
    assert.ok(certificate, "REPORCH_WINDOWS_CERT_SHA1 is required for release packaging");
  }
  if (certificate) {
    run("signtool", [
      "sign",
      "/sha1",
      certificate,
      "/fd",
      "SHA256",
      "/tr",
      "http://timestamp.acs.microsoft.com",
      "/td",
      "SHA256",
      destination
    ]);
  }
  return destination;
}

export function packSystemInstaller(targetName, binary, runtimeTree, outputArgument) {
  const target = TARGETS.find((candidate) => candidate.target === targetName);
  assert.ok(target, `unsupported installer target ${targetName}`);
  assertReleaseBinary(binary);
  assertRuntimeInstallTree(runtimeTree, target.runtimeTarget);
  const output = validateOutputArgument(outputArgument);
  assert.ok(!existsSync(output), `installer output already exists: ${output}`);
  mkdirSync(output, { mode: 0o755 });
  const version = JSON.parse(readFileSync("npm/cli/package.json", "utf8")).version;
  const work = mkdtempSync(join(tmpdir(), "reporch-system-installer-"));
  const root = join(work, "root");
  mkdirSync(root, { mode: 0o755 });
  let installers;
  try {
    if (target.target.includes("apple")) {
      stageMacPayload(root, target, binary, runtimeTree);
      installers = [packMac(root, target, version, output, work)];
    } else if (target.target.includes("windows")) {
      stageWindowsPayload(root, target, binary, runtimeTree);
      installers = [packWindows(root, target, version, output, work)];
    } else {
      stageLinuxPayload(root, target, binary, runtimeTree);
      installers = [
        packDeb(root, target, version, output),
        packRpm(root, target, version, output, work)
      ];
    }
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
  const records = installers.map((path) => ({
    filename: basename(path),
    sha256: sha256(path),
    size: lstatSync(path).size
  }));
  writeFileSync(
    join(output, "system-installer-manifest.json"),
    `${JSON.stringify({ schema: "reporch.cli-system-installer.v1", version, target: targetName, installers: records }, null, 2)}\n`
  );
  return records;
}

function main() {
  const [target, binary, runtimeTree, output] = process.argv.slice(2);
  if (!target || !binary || !runtimeTree || !output) {
    throw new Error("usage: node scripts/pack-system-installer.mjs <target> <binary> <runtime-tree> <new-output>");
  }
  console.log(JSON.stringify(packSystemInstaller(target, binary, runtimeTree, output)));
}

const invoked = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : null;
if (invoked === import.meta.url) main();
