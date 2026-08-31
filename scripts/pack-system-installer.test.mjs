import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { linuxRpmSpec, windowsInstallerXml } from "./pack-system-installer.mjs";

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
    assert.match(first, /RegistryValue[^>]+Name="Environment"[^>]+Type="multiString"/);
    assert.match(first, /MultiString Value="REPORCH_RUNTIME_ALLOWED_SID=\[UserSID\]"/);
    assert.match(first, /MultiString Value="REPORCH_RUNTIME_SERVICE_SCOPE=machine"/);
    assert.doesNotMatch(first, /Environment[^>]+Name="REPORCH_RUNTIME_ALLOWED_SID"/);
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
  assert.match(unit, /^Delegate=yes$/m);
  assert.match(unit, /^ProtectControlGroups=no$/m);
  assert.doesNotMatch(unit, /^ProtectControlGroups=yes$/m);
  assert.match(unit, /^DevicePolicy=closed$/m);
  assert.match(unit, /^DeviceAllow=\/dev\/kvm rwm$/m);
  assert.match(unit, /^DeviceAllow=\/dev\/net\/tun rwm$/m);
  const postinstall = readFileSync("installers/linux/postinstall.sh", "utf8");
  assert.match(postinstall, /runtime_vm_user=reporch-runtime-vm/);
  assert.match(postinstall, /REPORCH_RUNTIME_VM_UID/);
  assert.match(postinstall, /REPORCH_RUNTIME_VM_GID/);
  assert.match(postinstall, /while \[ ! -S "\$runtime_socket" \]/);
  assert.match(postinstall, /runtime service did not become ready within 30 seconds/);
  assert.match(postinstall, /setfacl -m "u:\$\{install_user\}:rw" "\$runtime_socket"/);
  assert.match(postinstall, /setfacl -R -m "u:\$\{install_user\}:r-X" \/var\/lib\/reporch-runtime/);
  assert.match(postinstall, /find \/var\/lib\/reporch-runtime -xdev -type d/);
  assert.match(postinstall, /"d:u:\$\{install_user\}:r-X"/);
});

test("RPM packaging preserves signed read-only runtime bytes", () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-rpm-spec-test-"));
  try {
    mkdirSync(join(root, "usr", "lib", "reporch"), { recursive: true });
    writeFileSync(join(root, "usr", "lib", "reporch", "vmlinux"), "signed runtime bytes");
    const spec = linuxRpmSpec(
      root,
      { target: "x86_64-unknown-linux-gnu" },
      "1.0.0-rc.8"
    );
    assert.match(spec, /^%global __os_install_post %\{nil\}$/m);
    assert.match(spec, /^\/usr\/lib\/reporch\/vmlinux$/m);
    assert.doesNotMatch(spec, /\bstrip\b/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("macOS release entitlement grants virtualization and nothing broader", () => {
  const path = "installers/macos/reporch.entitlements";
  const contents = readFileSync(path, "utf8");
  assert.match(contents, /com\.apple\.security\.virtualization/);
  assert.doesNotMatch(contents, /allow-jit|disable-library-validation|network\./);
  if (process.platform === "darwin") execFileSync("plutil", ["-lint", path]);
});
