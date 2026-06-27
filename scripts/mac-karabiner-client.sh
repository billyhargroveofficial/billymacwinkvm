#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${SOFTKVM_PORT:-49321}"
SOCKET="/Library/Application Support/org.pqrs/tmp/rootonly/karabiner_virtual_hid_device_service.sock"

cd "$ROOT"

if [[ ! -S "$SOCKET" ]]; then
  echo "Karabiner VirtualHID socket is missing; starting daemon first."
  ./scripts/start-karabiner-daemon.sh
fi

cargo build
cargo run -- mac-hid-probe

echo
echo "Starting real macOS input receiver through Karabiner VirtualHID."
echo "Leave this terminal open. Stop with Ctrl+C."
RUST_LOG="${RUST_LOG:-info}" target/debug/softkvm client --listen "0.0.0.0:$PORT" --sink karabiner
