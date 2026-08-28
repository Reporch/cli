#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
  printf '%s\n' 'usage: scripts/materialize-toolchain-sources.sh <new-cache-directory>' >&2
  exit 2
fi

output=$1
case "$output" in
  /*) ;;
  *) printf '%s\n' 'toolchain source cache must be absolute' >&2; exit 2 ;;
esac
if [ -e "$output" ]; then
  printf '%s\n' 'toolchain source cache already exists' >&2
  exit 2
fi

: "${SKOPEO:?SKOPEO must name the pinned absolute skopeo executable}"
case "$SKOPEO" in
  /*) ;;
  *) printf '%s\n' 'SKOPEO must be absolute' >&2; exit 2 ;;
esac
test -f "$SKOPEO"
test ! -L "$SKOPEO"
test -x "$SKOPEO"
test "$("$SKOPEO" --version)" = 'skopeo version 1.24.0'

node scripts/check-toolchain-sources.mjs runtime/toolchains.lock.json >/dev/null
parent=$(dirname "$output")
mkdir -p "$parent"
staging="$output.partial"
if [ -e "$staging" ]; then
  test -d "$staging"
  test ! -L "$staging"
else
  mkdir -m 0700 "$staging"
fi
if [ -e "$staging/toolchains.lock.json" ]; then
  cmp runtime/toolchains.lock.json "$staging/toolchains.lock.json"
else
  cp runtime/toolchains.lock.json "$staging/toolchains.lock.json"
fi
mkdir -p "$staging/shared-blobs"

digests_file="$staging/digests.txt"
jq -r '.entries[].image | split("sha256:")[1]' runtime/toolchains.lock.json | sort -u > "$digests_file"
while IFS= read -r digest; do
  image=$(jq -er --arg digest "$digest" '.entries[] | select(.image | endswith("sha256:" + $digest)) | .image' runtime/toolchains.lock.json | head -1)
  tagged_name=${image%@*}
  repository=${tagged_name%:*}
  digest_reference="$repository@sha256:$digest"
  for architecture in amd64 arm64; do
    destination="$staging/layouts/$digest/$architecture"
    mkdir -p "$(dirname "$destination")"
    if [ -d "$destination" ]; then
      node --input-type=module -e \
        'import {hydrateOciLayout,verifyOciLayout} from "./scripts/verify-toolchain-layouts.mjs"; hydrateOciLayout(process.argv[1], process.argv[3]); await verifyOciLayout(process.argv[1], process.argv[2]);' \
        "$destination" "$architecture" "$staging/shared-blobs" && continue
    fi
    if [ -e "$destination" ]; then
      test ! -L "$destination"
      rm -rf -- "$destination"
    fi
    "$SKOPEO" copy --src-no-creds --dest-no-creds \
      --dest-shared-blob-dir "$staging/shared-blobs" \
      --override-os linux --override-arch "$architecture" --preserve-digests \
      "docker://$digest_reference" "oci:$destination"
    node --input-type=module -e \
      'import {hydrateOciLayout,verifyOciLayout} from "./scripts/verify-toolchain-layouts.mjs"; hydrateOciLayout(process.argv[1], process.argv[3]); await verifyOciLayout(process.argv[1], process.argv[2]);' \
      "$destination" "$architecture" "$staging/shared-blobs"
  done
done < "$digests_file"
rm "$digests_file"

node scripts/verify-toolchain-layouts.mjs runtime/toolchains.lock.json "$staging" >/dev/null
rm -rf -- "$staging/shared-blobs"
mv "$staging" "$output"
printf '%s\n' "materialized verified toolchain sources at $output"
