#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PEER="${1:-${SOFTKVM_PEER:?pass the Mac address as arg 1 or set SOFTKVM_PEER, e.g. 192.168.1.6:49321}}"

cd "$ROOT"

if ! id -nG | tr ' ' '\n' | grep -qx input; then
  echo "error: $USER is not in the 'input' group, /dev/input is unreadable." >&2
  echo "fix: sudo usermod -aG input $USER  (then log out and back in)" >&2
  exit 1
fi
if [[ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]]; then
  echo "warning: HYPRLAND_INSTANCE_SIGNATURE is unset; edge activation and" >&2
  echo "cursor restore are disabled (Ctrl+Alt+\\ hotkey still works)." >&2
fi

echo "softkvm repo: $ROOT"
echo "softkvm head: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
echo "peer (mac): $PEER"
echo

cargo build --release

echo
echo "Starting Linux host capture (evdev + Hyprland IPC)."
echo "Cross the LEFT screen edge to control the Mac; Ctrl+Alt+\\ toggles."
echo "Leave this terminal open. Stop with Ctrl+C."
env \
  RUST_LOG="${RUST_LOG:-softkvm=info}" \
  SOFTKVM_LATENCY_LOG="${SOFTKVM_LATENCY_LOG:-0}" \
  SOFTKVM_LATENCY_WARN_MS="${SOFTKVM_LATENCY_WARN_MS:-8}" \
  "$ROOT/target/release/softkvm" host --peer "$PEER" --layout mac-left
