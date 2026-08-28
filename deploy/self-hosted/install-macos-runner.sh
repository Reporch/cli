#!/usr/bin/env bash
set -euo pipefail

repository=${REPORCH_RUNNER_REPOSITORY:-Reporch/cli}
runner_root=${REPORCH_RUNNER_ROOT:-"$HOME/.reporch/actions-runner-cli"}

if [[ $(uname -s) != Darwin || $(uname -m) != arm64 ]]; then
  echo "this installer requires an Apple silicon macOS host" >&2
  exit 64
fi
if [[ "$repository" != Reporch/cli ]]; then
  echo "runner repository must be Reporch/cli" >&2
  exit 64
fi
if [[ "$runner_root" =~ [[:space:]] ]]; then
  echo "runner path must not contain whitespace: $runner_root" >&2
  exit 64
fi
for command_name in awk curl gh jq sed shasum tar; do
  command -v "$command_name" >/dev/null 2>&1 || {
    echo "missing required command: $command_name" >&2
    exit 69
  }
done
gh auth status --hostname github.com >/dev/null

write_runner_path() {
  printf '%s\n' \
    "/opt/homebrew/bin:/usr/local/bin:$HOME/.cargo/bin:$HOME/.bun/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
    > "$runner_root/.path"
}

if [[ -f "$runner_root/.runner" ]]; then
  write_runner_path
  (
    cd "$runner_root"
    ./svc.sh stop >/dev/null 2>&1 || true
    ./svc.sh start
    ./svc.sh status
  )
  echo "runner is already configured at $runner_root"
  exit 0
fi

if [[ -d "$runner_root" ]] && find "$runner_root" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
  echo "refusing to overwrite non-empty runner directory: $runner_root" >&2
  exit 73
fi

release_json=$(mktemp)
download_dir=$(mktemp -d)
cleanup() {
  rm -f "$release_json"
  rm -rf "$download_dir"
}
trap cleanup EXIT

gh api repos/actions/runner/releases/latest > "$release_json"
runner_version=$(jq -er '.tag_name | sub("^v"; "") | select(test("^[0-9]+\\.[0-9]+\\.[0-9]+$"))' "$release_json")
asset="actions-runner-osx-arm64-$runner_version.tar.gz"
download_url=$(jq -er --arg asset "$asset" '.assets[] | select(.name == $asset) | .browser_download_url' "$release_json")
expected_sha=$(jq -r .body "$release_json" \
  | sed -n 's/.*BEGIN SHA osx-arm64 -->\([a-f0-9]\{64\}\)<!-- END SHA osx-arm64.*/\1/p')
[[ "$expected_sha" =~ ^[a-f0-9]{64}$ ]]

archive="$download_dir/$asset"
curl --fail --location --proto '=https' --tlsv1.2 "$download_url" --output "$archive"
actual_sha=$(shasum -a 256 "$archive" | awk '{print $1}')
if [[ "$actual_sha" != "$expected_sha" ]]; then
  echo "actions runner checksum mismatch" >&2
  exit 65
fi

mkdir -p "$runner_root"
tar -xzf "$archive" -C "$runner_root"
runner_name=$(hostname -s | tr '[:upper:]' '[:lower:]' | tr -cd 'a-z0-9-' | cut -c1-40)
runner_name="reporch-cli-${runner_name:-mac}-arm64"
registration_token=$(gh api --method POST "repos/$repository/actions/runners/registration-token" --jq .token)
[[ -n "$registration_token" ]]

(
  cd "$runner_root"
  ./config.sh \
    --url "https://github.com/$repository" \
    --token "$registration_token" \
    --name "$runner_name" \
    --labels cli-zero-cost,cli-macos-arm64 \
    --work _work \
    --unattended \
    --replace
  write_runner_path
  ./svc.sh install
  ./svc.sh start
  ./svc.sh status
)
registration_token=
echo "registered $runner_name for $repository"
