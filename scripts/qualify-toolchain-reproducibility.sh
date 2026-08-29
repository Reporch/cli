#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
  printf '%s\n' 'usage: scripts/qualify-toolchain-reproducibility.sh <verified-source-cache> <primary-candidate> <new-report>' >&2
  exit 2
fi
source_cache=$1
primary=$2
report=$3
for path in "$source_cache" "$primary" "$report"; do
  case "$path" in
    /*) ;;
    *) printf '%s\n' 'qualification paths must be absolute' >&2; exit 2 ;;
  esac
done
test -d "$source_cache"
test -d "$primary"
test ! -L "$source_cache"
test ! -L "$primary"
test ! -e "$report"
node scripts/check-toolchain-sources.mjs runtime/toolchains.lock.json >/dev/null

report_parent=$(dirname "$report")
mkdir -p "$report_parent"
work=$(mktemp -d "$report_parent/.toolchain-repro.XXXXXX")
cleanup() {
  if [ -d "$work" ]; then
    rm -rf -- "$work"
  fi
}
trap cleanup EXIT HUP INT TERM
reports="$work/reports.jsonl"
: > "$reports"

ids="$work/ids.txt"
jq -r '.entries[].id' runtime/toolchains.lock.json > "$ids"
while IFS= read -r id; do
  rebuild="$work/rebuild-$id"
  scripts/build-toolchain-candidates.sh "$source_cache" "$rebuild" "$id"
  node scripts/compare-toolchain-entry.mjs "$primary" "$rebuild" "$id" >> "$reports"
  rm -rf -- "$rebuild"
done < "$ids"
rm "$ids"

node scripts/compare-toolchain-entry.mjs --aggregate "$reports" > "$work/report.json"
mv "$work/report.json" "$report"
trap - EXIT HUP INT TERM
rm -rf -- "$work"
printf '%s\n' "qualified byte-identical toolchain rebuilds at $report"
