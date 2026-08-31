#!/bin/sh
set -eu

runtime_group=reporch-runtime
runtime_vm_user=reporch-runtime-vm
if ! getent group "$runtime_group" >/dev/null 2>&1; then
  groupadd --system "$runtime_group"
fi
if ! getent passwd "$runtime_vm_user" >/dev/null 2>&1; then
  useradd --system --gid "$runtime_group" --home-dir /nonexistent --no-create-home \
    --shell /usr/sbin/nologin "$runtime_vm_user"
fi

runtime_vm_uid=$(id -u "$runtime_vm_user")
runtime_vm_gid=$(id -g "$runtime_vm_user")
case "$runtime_vm_uid:$runtime_vm_gid" in
  *[!0-9:]*) printf '%s\n' 'runtime VM identity is invalid' >&2; exit 1 ;;
esac
install -d -o root -g root -m 0755 /etc/reporch
runtime_environment=$(mktemp /etc/reporch/.runtime.env.XXXXXX)
cleanup_runtime_environment() {
  rm -f -- "$runtime_environment"
}
trap cleanup_runtime_environment EXIT HUP INT TERM
{
  printf 'REPORCH_RUNTIME_VM_UID=%s\n' "$runtime_vm_uid"
  printf 'REPORCH_RUNTIME_VM_GID=%s\n' "$runtime_vm_gid"
} > "$runtime_environment"
chown root:root "$runtime_environment"
chmod 0644 "$runtime_environment"
mv -f -- "$runtime_environment" /etc/reporch/runtime.env
trap - EXIT HUP INT TERM

install_user=${REPORCH_INSTALL_USER:-${SUDO_USER:-}}
case "$install_user" in
  ''|*[!A-Za-z0-9._-]*) install_user= ;;
esac
if [ -n "$install_user" ] && [ "$(id -u "$install_user" 2>/dev/null || printf 0)" -ne 0 ]; then
  usermod --append --groups "$runtime_group" "$install_user"
fi

if [ -d /run/systemd/system ]; then
  systemctl daemon-reload
  systemctl enable --now reporch-runtime.service
  # Type=simple reports the unit active as soon as the broker process starts,
  # but the broker verifies/imports the packaged runtime seed before binding
  # its socket.  Wait for that readiness boundary before granting the
  # installing user an immediate ACL; otherwise the socket may not exist yet
  # and the first CLI command incorrectly reports the active service broken
  # until the user's next login refreshes supplementary groups.
  runtime_socket=/run/reporch-runtime/service-v1.sock
  runtime_socket_attempt=0
  while [ ! -S "$runtime_socket" ] && [ "$runtime_socket_attempt" -lt 300 ]; do
    sleep 0.1
    runtime_socket_attempt=$((runtime_socket_attempt + 1))
  done
  if [ ! -S "$runtime_socket" ]; then
    printf '%s\n' 'runtime service did not become ready within 30 seconds' >&2
    systemctl status --no-pager reporch-runtime.service >&2 || true
    exit 1
  fi
  if [ -n "$install_user" ] && command -v setfacl >/dev/null 2>&1; then
    setfacl -m "u:${install_user}:r-x" -m "d:u:${install_user}:r-x" /run/reporch-runtime
    setfacl -m "u:${install_user}:rw" "$runtime_socket"
    if [ -d /var/lib/reporch-runtime ]; then
      setfacl -R -m "u:${install_user}:r-X" /var/lib/reporch-runtime
      find /var/lib/reporch-runtime -xdev -type d \
        -exec setfacl -m "d:u:${install_user}:r-X" {} +
    fi
  fi
fi
