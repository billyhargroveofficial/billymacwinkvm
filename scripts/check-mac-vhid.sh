#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOCKET="/Library/Application Support/org.pqrs/tmp/rootonly/karabiner_virtual_hid_device_service.sock"

echo "== System extension =="
systemextensionsctl list | grep -i 'karabiner\|pqrs\|virtualhid' || true

echo
echo "== Karabiner processes =="
pgrep -afil 'Karabiner|VirtualHID|pqrs' || true

echo
echo "== Socket =="
if sudo -n test -S "$SOCKET" 2>/dev/null; then
  sudo -n ls -la "$SOCKET"
else
  echo "missing or not readable without sudo: $SOCKET"
  echo "run ./scripts/start-karabiner-daemon.sh to verify with sudo"
fi

echo
echo "== softkvm probe =="
cd "$ROOT"
cargo build
sudo -n target/debug/softkvm mac-hid-probe || true
