import { createHash } from "node:crypto";
import { chmodSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

function digest(bytes) {
  return `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
}

export function createRuntimeTreeFixture(root, target, version = "1.0.0-rc.8") {
  const sequence = 18;
  const bundle = join(root, "bundles", `${sequence}-${version}`);
  mkdirSync(bundle, { recursive: true });
  const definitions = [
    ["kernel", target === "windows-x64-msvc" ? "kernel" : "vmlinux", Buffer.from(`kernel ${target}\n`)],
    ["rootfs", "rootfs.cpio", Buffer.from(`rootfs ${target}\n`)],
    ["guest_agent", "reporch-guestd", Buffer.from(`guestd ${target}\n`)]
  ];
  if (target.startsWith("linux")) {
    definitions.push(
      ["virtual_machine_monitor", "firecracker", Buffer.from(`firecracker ${target}\n`)],
      ["jailer", "jailer", Buffer.from(`jailer ${target}\n`)],
      ["host_service", "reporch-runtime-service", Buffer.from(`service ${target}\n`)]
    );
  } else if (target === "windows-x64-msvc") {
    definitions.push([
      "host_service",
      "reporch-runtime-service.exe",
      Buffer.from(`service ${target}\n`)
    ]);
  }
  const artifacts = definitions.map(([kind, file_name, bytes]) => {
    const path = join(bundle, file_name);
    writeFileSync(path, bytes);
    chmodSync(
      path,
      ["guest_agent", "host_service", "virtual_machine_monitor", "jailer"].includes(kind)
        ? 0o555
        : 0o444
    );
    return {
      kind,
      file_name,
      sha256: digest(bytes),
      size: bytes.length,
      source_url: `https://example.test/${file_name}`,
      sbom_url: `https://example.test/${file_name}.spdx.json`,
      provenance_url: `https://example.test/${file_name}.intoto.jsonl`
    };
  });
  const backend = target.startsWith("darwin")
    ? "apple_virtualization"
    : target.startsWith("linux")
      ? "firecracker"
      : "hyper_v_hcs";
  const manifest = Buffer.from(
    `${JSON.stringify({
      schema: "reporch.runtime-bundle-manifest.v1",
      sequence,
      version,
      target,
      backend,
      minimum_os_version: "1",
      protocol_min: 1,
      protocol_max: 1,
      generated_at: "2026-08-29T00:00:00Z",
      expires_at: "2026-10-03T00:00:00Z",
      signing_key_id: "FF2F931B66DAA966",
      artifacts
    }, null, 2)}\n`
  );
  const manifestDigest = digest(manifest);
  writeFileSync(join(bundle, "manifest.json"), manifest);
  writeFileSync(join(bundle, "manifest.json.minisig"), "fixture signature\n");
  writeFileSync(join(bundle, ".complete"), `${manifestDigest}\n`);
  for (const name of ["manifest.json", "manifest.json.minisig", ".complete"]) {
    chmodSync(join(bundle, name), 0o444);
  }
  writeFileSync(
    join(root, "current.json"),
    `${JSON.stringify({
      schema: "reporch.runtime-installation.v1",
      sequence,
      version,
      target,
      bundle_sha256: manifestDigest,
      installed_at: "2026-08-29T00:00:00Z"
    }, null, 2)}\n`
  );
  return root;
}
