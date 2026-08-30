#!/bin/sh
set -eu

if [ "$#" -ne 4 ]; then
  printf '%s\n' 'usage: scripts/build-toolchain-release-resumable.sh <verified-source-cache> <persistent-checkpoint-root> <new-candidate-output> <new-reproducibility-report>' >&2
  exit 2
fi

source_cache=$1
checkpoint_root=$2
output=$3
report=$4
for path in "$source_cache" "$checkpoint_root" "$output" "$report"; do
  case "$path" in
    /*) ;;
    *) printf '%s\n' 'resumable toolchain release paths must be absolute' >&2; exit 2 ;;
  esac
done
test -d "$source_cache"
test ! -L "$source_cache"
test ! -e "$output"
test ! -e "$report"
test -z "$(git status --porcelain)"

source_revision=$(git rev-parse HEAD)
case "$source_revision" in
  *[!a-f0-9]*|'') printf '%s\n' 'source revision is invalid' >&2; exit 2 ;;
esac
test "${#source_revision}" -eq 40
lock_sha256=$(shasum -a 256 runtime/toolchains.lock.json | awk '{print $1}')
case "$lock_sha256" in
  *[!a-f0-9]*|'') printf '%s\n' 'toolchain lock digest is invalid' >&2; exit 2 ;;
esac
test "${#lock_sha256}" -eq 64

checkpoint_parent=$(dirname "$checkpoint_root")
mkdir -p "$checkpoint_parent"
test ! -L "$checkpoint_parent"
if [ ! -e "$checkpoint_root" ]; then
  mkdir "$checkpoint_root"
  chmod 700 "$checkpoint_root"
fi
test -d "$checkpoint_root"
test ! -L "$checkpoint_root"

identity="$checkpoint_root/identity.json"
expected_identity=$(mktemp "$checkpoint_root/.identity.XXXXXX")
cleanup_identity() {
  rm -f -- "$expected_identity"
}
trap cleanup_identity EXIT HUP INT TERM
jq -nS \
  --arg source_revision "$source_revision" \
  --arg toolchain_lock_sha256 "$lock_sha256" \
  '{schema:"reporch.toolchain-release-checkpoint.v1",source_revision:$source_revision,toolchain_lock_sha256:$toolchain_lock_sha256}' \
  > "$expected_identity"
if [ -e "$identity" ]; then
  test -f "$identity"
  test ! -L "$identity"
  cmp -s "$identity" "$expected_identity" || {
    printf '%s\n' 'persistent toolchain checkpoint identity does not match this source revision' >&2
    exit 2
  }
  rm "$expected_identity"
else
  chmod 600 "$expected_identity"
  mv "$expected_identity" "$identity"
fi
trap - EXIT HUP INT TERM

primary_root="$checkpoint_root/primary"
rebuild_root="$checkpoint_root/rebuild"
report_root="$checkpoint_root/reports"
mkdir -p "$primary_root" "$rebuild_root" "$report_root"
for path in "$primary_root" "$rebuild_root" "$report_root"; do
  test -d "$path"
  test ! -L "$path"
done

validate_checkpoint_candidate() {
  candidate=$1
  id=$2
  test -d "$candidate"
  test ! -L "$candidate"
  for name in \
    "$id-linux-arm64.ext4.zst" \
    "$id-linux-arm64.ext4.zst.build.json" \
    "$id-linux-arm64.source.spdx.json" \
    "$id-linux-x64.ext4.zst" \
    "$id-linux-x64.ext4.zst.build.json" \
    "$id-linux-x64.source.spdx.json" \
    "$id-windows-x64.vhdx.zst" \
    "$id-windows-x64.vhdx.zst.build.json"
  do
    path="$candidate/$name"
    test -f "$path"
    test ! -L "$path"
    test -s "$path"
  done
  test "$(find "$candidate" -mindepth 1 -maxdepth 1 -type f | wc -l | tr -d ' ')" = 8
  test "$(find "$candidate" -mindepth 1 -maxdepth 1 ! -type f | wc -l | tr -d ' ')" = 0
}

ids_file=$(mktemp "$checkpoint_root/.ids.XXXXXX")
work_parent=$(dirname "$output")
mkdir -p "$work_parent" "$(dirname "$report")"
work=$(mktemp -d "$work_parent/.toolchain-resumable.XXXXXX")
cleanup() {
  rm -f -- "$ids_file"
  if [ -d "$work" ]; then
    rm -rf -- "$work"
  fi
}
trap cleanup EXIT HUP INT TERM
jq -r '.entries[].id' runtime/toolchains.lock.json > "$ids_file"
test "$(wc -l < "$ids_file" | tr -d ' ')" = 12

while IFS= read -r id; do
  primary="$primary_root/$id"
  if [ ! -e "$primary" ]; then
    printf '%s\n' "building resumable primary toolchain checkpoint: $id"
    scripts/build-toolchain-candidates.sh "$source_cache" "$primary" "$id"
  else
    printf '%s\n' "reusing complete primary toolchain checkpoint: $id"
  fi
  validate_checkpoint_candidate "$primary" "$id"
done < "$ids_file"

candidates="$work/candidates"
mkdir "$candidates"
while IFS= read -r id; do
  primary="$primary_root/$id"
  for source in "$primary"/*; do
    test -f "$source"
    test ! -L "$source"
    name=$(basename "$source")
    case "$name" in
      "$id"-*) ;;
      *) printf '%s\n' "unexpected toolchain checkpoint file: $name" >&2; exit 2 ;;
    esac
    test ! -e "$candidates/$name"
    ln "$source" "$candidates/$name"
  done
done < "$ids_file"

cargo build --locked --release -p reporch-toolchain-release-builder
target/release/reporch-toolchain-release-builder \
  runtime/toolchains.lock.json "$candidates" "$source_revision" \
  https://github.com/Reporch/cli/releases/download/reporch-toolchains-v2-seq8/ \
  "$candidates/toolchains-v2-index.json"
rm "$candidates"/*.source.spdx.json
cp runtime/toolchains.lock.json "$candidates/toolchains.lock.json"

reports="$work/reports.jsonl"
: > "$reports"
while IFS= read -r id; do
  entry_report="$report_root/$id.json"
  if [ ! -e "$entry_report" ]; then
    rebuild="$rebuild_root/$id"
    if [ ! -e "$rebuild" ]; then
      printf '%s\n' "building resumable independent toolchain checkpoint: $id"
      scripts/build-toolchain-candidates.sh "$source_cache" "$rebuild" "$id"
    else
      printf '%s\n' "reusing complete independent toolchain checkpoint: $id"
    fi
    validate_checkpoint_candidate "$rebuild" "$id"
    temporary_report=$(mktemp "$report_root/.report-$id.XXXXXX")
    node scripts/compare-toolchain-entry.mjs "$candidates" "$rebuild" "$id" \
      > "$temporary_report"
    chmod 600 "$temporary_report"
    mv "$temporary_report" "$entry_report"
    rm -rf -- "$rebuild"
  else
    printf '%s\n' "reusing verified reproducibility evidence: $id"
  fi
  test -f "$entry_report"
  test ! -L "$entry_report"
  jq -e \
    --arg id "$id" \
    '.schema == "reporch.toolchain-entry-reproducibility.v2" and .id == $id and .files == 9 and (.tree_sha256 | test("^[a-f0-9]{64}$"))' \
    "$entry_report" >/dev/null
  jq -c . "$entry_report" >> "$reports"
done < "$ids_file"

aggregate="$work/toolchain-reproducibility.json"
node scripts/compare-toolchain-entry.mjs --aggregate "$reports" > "$aggregate"
jq -e '.schema == "reporch.toolchain-reproducibility.v2" and .toolchains == 12' \
  "$aggregate" >/dev/null

mv "$candidates" "$output"
mv "$aggregate" "$report"
trap - EXIT HUP INT TERM
rm -f -- "$ids_file"
rm -rf -- "$work"
printf '%s\n' "built and qualified resumable toolchain release at $output"
