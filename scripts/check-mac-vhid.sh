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
if [[ -S "$SOCKET" ]]; then
  ls -la "$SOCKET"
else
  echo "missing: $SOCKET"
fi

echo
echo "== softkvm probe =="
cd "$ROOT"
cargo run -- mac-hid-probe || true
