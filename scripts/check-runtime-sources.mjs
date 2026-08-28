import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const lock = JSON.parse(readFileSync("runtime/sources.lock.json", "utf8"));
assert.equal(lock.schema, "reporch.runtime-sources-lock.v1");
assert.ok(Number.isSafeInteger(lock.source_date_epoch) && lock.source_date_epoch > 0);
assert.match(lock.guest_kernel.version, /^6\.1\.\d+$/);
assert.match(lock.firecracker.version, /^1\.\d+\.\d+$/);
assert.match(lock.firecracker.tag_commit, /^[a-f0-9]{40}$/);
assert.equal(lock.rust.toolchain, "1.96.0");
assert.deepEqual(lock.rust.guest_targets, [
  "aarch64-unknown-linux-musl",
  "x86_64-unknown-linux-musl"
]);

for (const architecture of ["aarch64", "x86_64"]) {
  const kernel = lock.guest_kernel.artifacts[architecture];
  const firecracker = lock.firecracker.artifacts[architecture];
  for (const [label, value] of [
    ["kernel URL", kernel.url],
    ["kernel config URL", kernel.config_url],
    ["Firecracker URL", firecracker.url]
  ]) {
    const url = new URL(value);
    assert.equal(url.protocol, "https:", `${label} must use HTTPS`);
    assert.equal(url.username, "");
    assert.equal(url.password, "");
    assert.equal(url.search, "");
    assert.equal(url.hash, "");
  }
  for (const [label, digest] of [
    ["kernel", kernel.sha256],
    ["kernel config", kernel.config_sha256],
    ["Firecracker", firecracker.sha256]
  ]) {
    assert.match(digest, /^[a-f0-9]{64}$/, `${label} digest must be lowercase SHA-256`);
  }
  assert.match(kernel.url, new RegExp(`/vmlinux-${lock.guest_kernel.version}$`));
  assert.match(
    firecracker.url,
    new RegExp(`/v${lock.firecracker.version}/firecracker-v${lock.firecracker.version}-${architecture}\\.tgz$`)
  );
}

console.log("runtime source lock is complete and immutable");
