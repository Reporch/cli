import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { windowsInstallerXml } from "./pack-system-installer.mjs";

test("Windows installer XML binds the CLI, service, and runtime without a network helper", () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-installer-test-"));
  try {
    mkdirSync(join(root, "bin", "runtime", "windows-x64-msvc"), { recursive: true });
    writeFileSync(join(root, "bin", "reporch.exe"), "cli");
    writeFileSync(join(root, "bin", "reporch-runtime-service.exe"), "service");
    writeFileSync(join(root, "bin", "runtime", "windows-x64-msvc", "current.json"), "{}");
    const first = windowsInstallerXml(root, "1.0.0-rc.8");
    const second = windowsInstallerXml(root, "1.0.0-rc.8");
    assert.equal(first, second);
    assert.match(first, /Version="1\.0\.8"/);
    assert.match(first, /Directory Id="BINDIR" Name="bin"/);
    assert.match(first, /ServiceInstall[^>]+Name="ReporchRuntime"/);
    assert.match(first, /Name="REPORCH_RUNTIME_ALLOWED_SID" Value="\[UserSID\]"/);
    assert.match(first, /Environment[^>]+Name="PATH"[^>]+\[BINDIR\]/);
    assert.doesNotMatch(first, /CustomAction/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("Linux package scripts define the dedicated VM identity and constrained broker", () => {
  execFileSync("sh", ["-n", "installers/linux/postinstall.sh"]);
  execFileSync("sh", ["-n", "installers/linux/preremove.sh"]);
  const unit = execFileSync("sed", ["-n", "1,200p", "installers/linux/reporch-runtime.service"], {
    encoding: "utf8"
  });
  assert.match(unit, /Group=reporch-runtime/);
  assert.match(unit, /EnvironmentFile=\/etc\/reporch\/runtime\.env/);
  assert.match(unit, /RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6/);
  assert.doesNotMatch(unit, /PrivateNetwork=yes/);
  assert.match(unit, /DevicePolicy=closed/);
  assert.match(unit, /DeviceAllow=\/dev\/kvm rw/);
  const postinstall = readFileSync("installers/linux/postinstall.sh", "utf8");
  assert.match(postinstall, /runtime_vm_user=reporch-runtime-vm/);
  assert.match(postinstall, /REPORCH_RUNTIME_VM_UID/);
  assert.match(postinstall, /REPORCH_RUNTIME_VM_GID/);
});

test("macOS release entitlement grants virtualization and nothing broader", () => {
  const path = "installers/macos/reporch.entitlements";
  const contents = readFileSync(path, "utf8");
  assert.match(contents, /com\.apple\.security\.virtualization/);
  assert.doesNotMatch(contents, /allow-jit|disable-library-validation|network\./);
  if (process.platform === "darwin") execFileSync("plutil", ["-lint", path]);
});
