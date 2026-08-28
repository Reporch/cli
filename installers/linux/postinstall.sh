#!/bin/sh
set -eu

runtime_group=reporch-runtime
if ! getent group "$runtime_group" >/dev/null 2>&1; then
  groupadd --system "$runtime_group"
fi

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
  if [ -n "$install_user" ] && command -v setfacl >/dev/null 2>&1; then
    setfacl -m "u:${install_user}:r-x" -m "d:u:${install_user}:r-x" /run/reporch-runtime
    if [ -S /run/reporch-runtime/service-v1.sock ]; then
      setfacl -m "u:${install_user}:rw" /run/reporch-runtime/service-v1.sock
    fi
  fi
fi
