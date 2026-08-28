#!/bin/sh
set -eu

if [ "${1:-}" = remove ] && [ -d /run/systemd/system ]; then
  systemctl disable --now reporch-runtime.service || true
fi
