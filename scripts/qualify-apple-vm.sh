#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 4 ]]; then
  echo "usage: scripts/qualify-apple-vm.sh <kernel> <initramfs> <toolchain-image> <evidence-directory>" >&2
  exit 2
fi

KERNEL="$1"
INITRAMFS="$2"
TOOLCHAIN="$3"
EVIDENCE="$4"

for path in "$KERNEL" "$INITRAMFS" "$TOOLCHAIN" "$EVIDENCE"; do
  [[ "$path" == /* ]] || {
    echo "qualification paths must be absolute" >&2
    exit 2
  }
done
[[ "$(uname -s)" == "Darwin" && "$(uname -m)" == "arm64" ]] || {
  echo "Apple VM qualification requires a macOS arm64 host" >&2
  exit 2
}
for command_name in cargo codesign file git jq shasum; do
  command -v "$command_name" >/dev/null 2>&1 || {
    echo "required command is unavailable: $command_name" >&2
    exit 2
  }
done
for artifact in "$KERNEL" "$INITRAMFS" "$TOOLCHAIN"; do
  [[ -f "$artifact" && ! -L "$artifact" ]] || {
    echo "qualification artifact must be a regular non-symlink file: $artifact" >&2
    exit 2
  }
done
if [[ -e "$EVIDENCE" ]]; then
  [[ -d "$EVIDENCE" && ! -L "$EVIDENCE" ]] || {
    echo "evidence path must be a non-symlink directory" >&2
    exit 2
  }
else
  mkdir -p "$EVIDENCE"
fi
chmod 0700 "$EVIDENCE"

REPOSITORY_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$REPOSITORY_ROOT"
SOURCE_REVISION="$(git rev-parse HEAD)"
[[ "$SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]]
test -z "$(git status --porcelain)"

CARGO_MESSAGES="$EVIDENCE/cargo-test-build.jsonl"
cargo test --locked -p reporch-runtime-host --no-run --message-format=json \
  > "$CARGO_MESSAGES"
TEST_BINARY="$(
  jq -r '
    select(
      .reason == "compiler-artifact"
      and .target.name == "reporch_runtime_host"
      and .profile.test == true
      and (.executable | type) == "string"
    )
    | .executable
  ' "$CARGO_MESSAGES" | sort -u
)"
[[ -n "$TEST_BINARY" && "$TEST_BINARY" != *$'\n'* ]] || {
  echo "expected exactly one reporch-runtime-host test binary" >&2
  exit 1
}
[[ "$TEST_BINARY" == "$REPOSITORY_ROOT/target/"* && -f "$TEST_BINARY" && ! -L "$TEST_BINARY" ]] || {
  echo "runtime-host test binary escaped the repository target directory" >&2
  exit 1
}
file "$TEST_BINARY" | grep -Eq 'Mach-O 64-bit.*arm64'
codesign --force --sign - \
  --entitlements installers/macos/reporch.entitlements \
  --options runtime "$TEST_BINARY"
codesign --verify --strict --verbose=2 "$TEST_BINARY" \
  > "$EVIDENCE/codesign-verify.txt" 2>&1

LIFECYCLE_LOG="$EVIDENCE/apple-vm-lifecycle.log"
REPORCH_TEST_KERNEL="$KERNEL" \
REPORCH_TEST_INITRAMFS="$INITRAMFS" \
REPORCH_TEST_ITERATIONS=100 \
  "$TEST_BINARY" --ignored --exact \
    apple_backend::tests::real_apple_vm_boots_handshakes_executes_and_stops \
    --nocapture > "$LIFECYCLE_LOG" 2>&1

TOOLCHAIN_LOG="$EVIDENCE/apple-vm-toolchain.log"
REPORCH_TEST_KERNEL="$KERNEL" \
REPORCH_TEST_INITRAMFS="$INITRAMFS" \
REPORCH_TEST_TOOLCHAIN="$TOOLCHAIN" \
  "$TEST_BINARY" --ignored --exact \
    apple_backend::tests::real_apple_vm_mounts_and_executes_toolchain_image \
    --nocapture > "$TOOLCHAIN_LOG" 2>&1

METRICS="$(grep -E '^Apple VM lifecycle:' "$LIFECYCLE_LOG")"
[[ "$METRICS" =~ iterations=([0-9]+)[[:space:]]p50=([^[:space:]]+)[[:space:]]p95=([^[:space:]]+)[[:space:]]p99=([^[:space:]]+)[[:space:]]max=([^[:space:]]+) ]] || {
  echo "Apple VM lifecycle metrics are missing" >&2
  exit 1
}
ITERATIONS="${BASH_REMATCH[1]}"
P50="${BASH_REMATCH[2]}"
P95="${BASH_REMATCH[3]}"
P99="${BASH_REMATCH[4]}"
MAXIMUM="${BASH_REMATCH[5]}"
test "$ITERATIONS" = 100
grep -Fq 'test result: ok. 1 passed; 0 failed' "$LIFECYCLE_LOG"
grep -Fq 'test result: ok. 1 passed; 0 failed' "$TOOLCHAIN_LOG"

KERNEL_SHA256="$(shasum -a 256 "$KERNEL" | awk '{print $1}')"
INITRAMFS_SHA256="$(shasum -a 256 "$INITRAMFS" | awk '{print $1}')"
TOOLCHAIN_SHA256="$(shasum -a 256 "$TOOLCHAIN" | awk '{print $1}')"
COMPLETED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
jq -n \
  --arg source_revision "$SOURCE_REVISION" \
  --arg completed_at "$COMPLETED_AT" \
  --arg kernel_sha256 "sha256:$KERNEL_SHA256" \
  --arg initramfs_sha256 "sha256:$INITRAMFS_SHA256" \
  --arg toolchain_sha256 "sha256:$TOOLCHAIN_SHA256" \
  --arg p50 "$P50" \
  --arg p95 "$P95" \
  --arg p99 "$P99" \
  --arg maximum "$MAXIMUM" \
  --argjson iterations "$ITERATIONS" \
  '{
    schema:"reporch.apple-vm-qualification.v1",
    source_revision:$source_revision,
    completed_at:$completed_at,
    target:"darwin-arm64",
    backend:"apple_virtualization",
    kernel_sha256:$kernel_sha256,
    initramfs_sha256:$initramfs_sha256,
    toolchain_sha256:$toolchain_sha256,
    iterations:$iterations,
    p50:$p50,
    p95:$p95,
    p99:$p99,
    maximum:$maximum,
    lifecycle:true,
    handshake:true,
    guest_workload:true,
    cleanup:true,
    file_descriptor_leak:false,
    read_only_toolchain:true,
    passed:true
  }' > "$EVIDENCE/result.json"
(cd "$EVIDENCE" && shasum -a 256 ./* > SHA256SUMS)
jq -e '.passed and .iterations == 100 and .read_only_toolchain' "$EVIDENCE/result.json" >/dev/null
cat "$EVIDENCE/result.json"
