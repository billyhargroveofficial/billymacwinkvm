#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${SOFTKVM_PORT:-49321}"
SOCKET="/Library/Application Support/org.pqrs/tmp/rootonly/karabiner_virtual_hid_device_service.sock"

cd "$ROOT"

echo "softkvm repo: $ROOT"
echo "softkvm head: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
echo "listen port: $PORT"
echo

if [[ ! -S "$SOCKET" ]]; then
  echo "Karabiner VirtualHID socket is missing; starting daemon first."
  ./scripts/start-karabiner-daemon.sh
fi

cargo build
sudo "$ROOT/target/debug/softkvm" mac-hid-probe

if [[ "${SOFTKVM_SKIP_SMOKE:-0}" != "1" ]]; then
  echo
  echo "Running no-click Karabiner input smoke before accepting Windows."
  sudo "$ROOT/target/debug/softkvm" mac-hid-smoke
fi

echo
echo "Starting real macOS input receiver through Karabiner VirtualHID."
echo "Leave this terminal open. Stop with Ctrl+C."
sudo env RUST_LOG="${RUST_LOG:-info}" "$ROOT/target/debug/softkvm" client --listen "0.0.0.0:$PORT" --sink karabiner
