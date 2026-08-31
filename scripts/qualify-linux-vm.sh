#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 2 ]]; then
  echo "usage: scripts/qualify-linux-vm.sh <reporch-binary> <evidence-directory>" >&2
  exit 2
fi

REPORCH_BINARY="$1"
EVIDENCE="$2"
[[ "$REPORCH_BINARY" == /* && "$EVIDENCE" == /* ]] || {
  echo "qualification paths must be absolute" >&2
  exit 2
}
[[ "$(uname -s)" == Linux ]] || {
  echo "Linux VM qualification requires a Linux host" >&2
  exit 2
}
case "$(uname -m)" in
  x86_64) EXPECTED_TARGET=linux-x64-gnu ;;
  aarch64|arm64) EXPECTED_TARGET=linux-arm64-gnu ;;
  *) echo "unsupported Linux qualification architecture: $(uname -m)" >&2; exit 2 ;;
esac
for command_name in find jq pgrep sha256sum stat sudo systemctl; do
  command -v "$command_name" >/dev/null 2>&1 || {
    echo "required command is unavailable: $command_name" >&2
    exit 2
  }
done
[[ -f "$REPORCH_BINARY" && ! -L "$REPORCH_BINARY" && -x "$REPORCH_BINARY" ]] || {
  echo "qualification binary must be an executable regular non-symlink file" >&2
  exit 2
}
if [[ -e "$EVIDENCE" ]]; then
  [[ -d "$EVIDENCE" && ! -L "$EVIDENCE" ]] || {
    echo "evidence path must be a non-symlink directory" >&2
    exit 2
  }
else
  mkdir -p "$EVIDENCE"
fi
chmod 0700 "$EVIDENCE"

systemctl is-active --quiet reporch-runtime.service
SERVICE_PID="$(systemctl show --property MainPID --value reporch-runtime.service)"
[[ "$SERVICE_PID" =~ ^[1-9][0-9]*$ && -d "/proc/$SERVICE_PID" ]]
SERVICE_CGROUP="$(systemctl show --property ControlGroup --value reporch-runtime.service)"
[[ "$SERVICE_CGROUP" == /* && -d "/sys/fs/cgroup$SERVICE_CGROUP" ]]

snapshot_backend() {
  local prefix="$1"
  {
    pgrep -x firecracker || true
    pgrep -x jailer || true
  } | sort -n -u > "$EVIDENCE/$prefix-processes.txt"
  if [[ -d /var/lib/reporch-runtime/jailer ]]; then
    sudo --non-interactive find /var/lib/reporch-runtime/jailer \
      -type d -name 'rp-*' -print \
      | sort > "$EVIDENCE/$prefix-jails.txt"
  else
    : > "$EVIDENCE/$prefix-jails.txt"
  fi
  find "/sys/fs/cgroup$SERVICE_CGROUP" -mindepth 1 -maxdepth 1 \
    -type d -name 'rp-*' -print | sort > "$EVIDENCE/$prefix-cgroups.txt"
  sudo --non-interactive find "/proc/$SERVICE_PID/fd" \
    -mindepth 1 -maxdepth 1 -print \
    | wc -l | tr -d ' ' > "$EVIDENCE/$prefix-service-fds.txt"
}

snapshot_backend before
"$REPORCH_BINARY" --format json runtime status > "$EVIDENCE/status.json"
jq -e --arg target "$EXPECTED_TARGET" '
  .schema == "reporch.cli-result.v1"
  and .command == "runtime status"
  and .data.target == $target
  and .data.installed_sequence == 14
  and .data.backend == "firecracker"
  and .data.availability == "ready"
  and .data.virtualization_available
  and .data.service_available
' "$EVIDENCE/status.json" >/dev/null

"$REPORCH_BINARY" --format json runtime qualification \
  --iterations 100 --toolchain bash-5.3 > "$EVIDENCE/qualification.json"
jq -e --arg target "$EXPECTED_TARGET" '
  .schema == "reporch.cli-result.v1"
  and .command == "runtime qualification"
  and .data.schema == "reporch.native-runtime-qualification.v1"
  and .data.target == $target
  and .data.backend == "firecracker"
  and .data.iterations == 100
  and .data.p95_ms <= 2000
  and .data.lifecycle
  and .data.handshake
  and .data.guest_workload
  and .data.cleanup
  and .data.signed_toolchain_unchanged
  and .data.passed
' "$EVIDENCE/qualification.json" >/dev/null

for _ in $(seq 1 50); do
  current_processes="$(pgrep -x firecracker || true)$(pgrep -x jailer || true)"
  current_jails="$(sudo --non-interactive find /var/lib/reporch-runtime/jailer -type d -name 'rp-*' -print 2>/dev/null || true)"
  [[ -z "$current_processes$current_jails" ]] && break
  sleep 0.1
done
snapshot_backend after
cmp "$EVIDENCE/before-processes.txt" "$EVIDENCE/after-processes.txt"
cmp "$EVIDENCE/before-jails.txt" "$EVIDENCE/after-jails.txt"
cmp "$EVIDENCE/before-cgroups.txt" "$EVIDENCE/after-cgroups.txt"
BEFORE_FDS="$(<"$EVIDENCE/before-service-fds.txt")"
AFTER_FDS="$(<"$EVIDENCE/after-service-fds.txt")"
(( AFTER_FDS <= BEFORE_FDS + 2 ))

jq -n \
  --arg completed_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg target "$EXPECTED_TARGET" \
  --argjson service_pid "$SERVICE_PID" \
  --argjson before_fds "$BEFORE_FDS" \
  --argjson after_fds "$AFTER_FDS" \
  '{
    schema:"reporch.linux-vm-host-qualification.v1",
    completed_at:$completed_at,
    target:$target,
    backend:"firecracker",
    service_pid:$service_pid,
    before_service_fds:$before_fds,
    after_service_fds:$after_fds,
    orphan_processes:0,
    orphan_jails:0,
    passed:true
  }' > "$EVIDENCE/host-result.json"
(cd "$EVIDENCE" && sha256sum ./* > SHA256SUMS)
jq -e '.passed and .orphan_processes == 0 and .orphan_jails == 0' \
  "$EVIDENCE/host-result.json" >/dev/null
cat "$EVIDENCE/host-result.json"
