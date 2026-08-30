#!/bin/sh
set -eu

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
  printf '%s\n' 'usage: scripts/build-toolchain-candidates.sh <verified-source-cache> <new-output-directory> [one-toolchain-id]' >&2
  exit 2
fi

source_cache=$1
output=$2
only_id=${3:-}
for path in "$source_cache" "$output"; do
  case "$path" in
    /*) ;;
    *) printf '%s\n' 'toolchain candidate paths must be absolute' >&2; exit 2 ;;
  esac
done
test -d "$source_cache"
test ! -L "$source_cache"
if [ -e "$output" ]; then
  printf '%s\n' 'toolchain candidate output already exists' >&2
  exit 2
fi

: "${QEMU_IMG:?QEMU_IMG must name the pinned absolute qemu-img executable}"
: "${SYFT:?SYFT must name the pinned absolute syft executable}"
case "$QEMU_IMG" in
  /*) ;;
  *) printf '%s\n' 'QEMU_IMG must be absolute' >&2; exit 2 ;;
esac
test -f "$QEMU_IMG"
test ! -L "$QEMU_IMG"
test -x "$QEMU_IMG"
test "$("$QEMU_IMG" --version | head -1)" = 'qemu-img version 11.1.1'
case "$SYFT" in
  /*) ;;
  *) printf '%s\n' 'SYFT must be absolute' >&2; exit 2 ;;
esac
test -f "$SYFT"
test ! -L "$SYFT"
test -x "$SYFT"
test "$("$SYFT" version -o json | jq -r .version)" = '1.51.0'

node scripts/check-toolchain-sources.mjs runtime/toolchains.lock.json >/dev/null
node scripts/verify-toolchain-layouts.mjs runtime/toolchains.lock.json "$source_cache" >/dev/null
if [ -n "$only_id" ]; then
  jq -e --arg id "$only_id" '[.entries[] | select(.id == $id)] | length == 1' \
    runtime/toolchains.lock.json >/dev/null
fi
source_revision=$(git rev-parse HEAD)
case "$source_revision" in
  *[!a-f0-9]*|'') printf '%s\n' 'source revision is invalid' >&2; exit 2 ;;
esac
test "${#source_revision}" -eq 40
test -z "$(git status --porcelain)"

parent=$(dirname "$output")
mkdir -p "$parent"
staging=$(mktemp -d "$parent/.toolchain-candidates.XXXXXX")
cleanup() {
  if [ -d "$staging" ]; then
    rm -rf -- "$staging"
  fi
}
trap cleanup EXIT HUP INT TERM

source_date_epoch=$(jq -er '.source_date_epoch | select(type == "number" and . > 0)' runtime/toolchains.lock.json)
export SOURCE_DATE_EPOCH="$source_date_epoch"
created=$(node -e 'process.stdout.write(new Date(Number(process.argv[1]) * 1000).toISOString().replace(".000Z", "Z"))' "$source_date_epoch")
cargo build --locked --release -p reporch-toolchain-bundle-builder -p reporch-toolchain-release-builder

entries_file="$staging/entries.jsonl"
jq -c --arg id "$only_id" '.entries[] | select($id == "" or .id == $id)' \
  runtime/toolchains.lock.json > "$entries_file"
while IFS= read -r entry; do
  id=$(printf '%s' "$entry" | jq -er .id)
  image_mib=$(printf '%s' "$entry" | jq -er .image_mib)
  identity=$(printf '%s' "$entry" | jq -er '.image | split("@")[1]')
  digest=${identity#sha256:}
  arm_layout="$source_cache/layouts/$digest/arm64"
  x64_layout="$source_cache/layouts/$digest/amd64"

  for architecture in arm64 amd64; do
    if [ "$architecture" = arm64 ]; then
      layout=$arm_layout
      suffix=linux-arm64
    else
      layout=$x64_layout
      suffix=linux-x64
    fi
    raw_sbom="$staging/.$id-$architecture.syft.spdx.json"
    normalized_sbom="$staging/$id-$suffix.source.spdx.json"
    SYFT_CHECK_FOR_APP_UPDATE=false "$SYFT" scan "oci-dir:$layout" \
      -o "spdx-json=$raw_sbom" -q
    node scripts/normalize-toolchain-sbom.mjs \
      "$raw_sbom" "$identity" "$architecture" "$created" "$normalized_sbom" >/dev/null
    rm "$raw_sbom"
  done

  target/release/reporch-toolchain-bundle-builder \
    "$arm_layout" arm64 "$staging/$id-linux-arm64.ext4.zst" "$image_mib" "$identity" \
    > "$staging/$id-linux-arm64.ext4.zst.build.json"
  target/release/reporch-toolchain-bundle-builder \
    "$x64_layout" amd64 "$staging/$id-linux-x64.ext4.zst" "$image_mib" "$identity" \
    > "$staging/$id-linux-x64.ext4.zst.build.json"
  target/release/reporch-toolchain-bundle-builder \
    "$x64_layout" amd64 "$staging/$id-windows-x64.vhdx.zst" "$image_mib" "$identity" "$QEMU_IMG" \
    > "$staging/$id-windows-x64.vhdx.zst.build.json"
done < "$entries_file"
rm "$entries_file"

if [ -n "$only_id" ]; then
  mv "$staging" "$output"
  trap - EXIT HUP INT TERM
  printf '%s\n' "built independent $only_id toolchain candidate at $output"
  exit 0
fi

target/release/reporch-toolchain-release-builder \
  runtime/toolchains.lock.json "$staging" "$source_revision" \
  https://github.com/Reporch/cli/releases/download/reporch-toolchains-v2-seq8/ \
  "$staging/toolchains-v2-index.json"
rm "$staging"/*.source.spdx.json
cp runtime/toolchains.lock.json "$staging/toolchains.lock.json"
mv "$staging" "$output"
trap - EXIT HUP INT TERM
printf '%s\n' "built unsigned toolchain candidates at $output"
