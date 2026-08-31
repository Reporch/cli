#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
  printf '%s\n' 'usage: scripts/build-runtime-candidates.sh <new-output-directory>' >&2
  exit 2
fi

output=$1
case "$output" in
  /*) ;;
  *) printf '%s\n' 'runtime candidate output must be absolute' >&2; exit 2 ;;
esac
if [ -e "$output" ]; then
  printf '%s\n' 'runtime candidate output already exists' >&2
  exit 2
fi

for command_name in cargo git jq shasum zig; do
  command -v "$command_name" >/dev/null
done
cargo zigbuild --help >/dev/null
cargo xwin --help >/dev/null

source_revision=$(git rev-parse HEAD)
case "$source_revision" in
  *[!a-f0-9]*|'') printf '%s\n' 'source revision is invalid' >&2; exit 2 ;;
esac
test "${#source_revision}" -eq 40
test -z "$(git status --porcelain)"

parent=$(dirname "$output")
mkdir -p "$parent"
staging=$(mktemp -d "$parent/.runtime-candidates.XXXXXX")
cleanup() {
  if [ -d "$staging" ]; then
    rm -rf -- "$staging"
  fi
}
trap cleanup EXIT HUP INT TERM

source_date_epoch=$(jq -er '.source_date_epoch | select(type == "number" and . > 0)' runtime/sources.lock.json)
export SOURCE_DATE_EPOCH="$source_date_epoch"

cargo zigbuild --locked --release --target aarch64-unknown-linux-musl -p reporch-guestd
cargo zigbuild --locked --release --target x86_64-unknown-linux-musl -p reporch-guestd
cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p reporch-runtime-service
cargo zigbuild --locked --release --target x86_64-unknown-linux-gnu -p reporch-runtime-service
cargo xwin build --locked --release --target x86_64-pc-windows-msvc -p reporch-runtime-service
cargo build --locked --release \
  -p reporch-runtime-source-fetcher \
  -p reporch-runtime-image-builder \
  -p reporch-runtime-evidence-builder \
  -p reporch-runtime-bundle-builder

for target in darwin-arm64 darwin-x64 linux-arm64-gnu linux-x64-gnu windows-x64-msvc; do
  target/release/reporch-runtime-source-fetcher "$target" "$staging/sources-$target"
done

assemble() {
  target=$1
  architecture=$2
  guest_target=$3
  source_root="$staging/sources-$target"
  artifact_root="$staging/artifacts/$target"
  mkdir -p "$artifact_root"
  if [ "$target" = windows-x64-msvc ]; then
    install -m 0444 "$source_root/kernel" "$artifact_root/kernel"
  else
    install -m 0444 "$source_root/vmlinux" "$artifact_root/vmlinux"
  fi
  install -m 0555 "target/$guest_target/release/reporch-guestd" "$artifact_root/reporch-guestd"
  target/release/reporch-runtime-image-builder \
    "$artifact_root/reporch-guestd" "$artifact_root/rootfs.cpio" "$architecture"

  case "$target" in
    linux-arm64-gnu)
      install -m 0555 "$source_root/firecracker" "$artifact_root/firecracker"
      install -m 0555 "$source_root/jailer" "$artifact_root/jailer"
      install -m 0555 target/aarch64-unknown-linux-gnu/release/reporch-runtime-service \
        "$artifact_root/reporch-runtime-service"
      ;;
    linux-x64-gnu)
      install -m 0555 "$source_root/firecracker" "$artifact_root/firecracker"
      install -m 0555 "$source_root/jailer" "$artifact_root/jailer"
      install -m 0555 target/x86_64-unknown-linux-gnu/release/reporch-runtime-service \
        "$artifact_root/reporch-runtime-service"
      ;;
    windows-x64-msvc)
      install -m 0555 target/x86_64-pc-windows-msvc/release/reporch-runtime-service.exe \
        "$artifact_root/reporch-runtime-service.exe"
      ;;
  esac
  target/release/reporch-runtime-evidence-builder \
    "$target" "$artifact_root" "$source_root/sources.json" "$source_revision"
}

assemble darwin-arm64 aarch64 aarch64-unknown-linux-musl
assemble darwin-x64 x86_64 x86_64-unknown-linux-musl
assemble linux-arm64-gnu aarch64 aarch64-unknown-linux-musl
assemble linux-x64-gnu x86_64 x86_64-unknown-linux-musl
assemble windows-x64-msvc x86_64 x86_64-unknown-linux-musl

mkdir -p "$staging/manifests"
manifest() {
  target=$1
  minimum_os=$2
  target/release/reporch-runtime-bundle-builder \
    "$target" 15 1.0.0-rc.8 "$minimum_os" \
    https://github.com/Reporch/cli/releases/download/reporch-runtime-v1-seq15/ \
    "$staging/artifacts/$target" "$staging/manifests/runtime-$target-manifest.json"
}
manifest darwin-arm64 13.0
manifest darwin-x64 13.0
manifest linux-arm64-gnu 5.10
manifest linux-x64-gnu 5.10
manifest windows-x64-msvc 10.0.19041.0

cp runtime/sources.lock.json "$staging/sources.lock.json"
mv "$staging" "$output"
trap - EXIT HUP INT TERM
printf '%s\n' "built unsigned runtime candidates at $output"
