#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${SOFTKVM_PORT:-49321}"

cd "$ROOT"

echo "softkvm repo: $ROOT"
echo "softkvm head: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
echo "listen port: $PORT"
echo "sink: cg-event"
if [[ "${SOFTKVM_LATENCY_LOG:-0}" != "0" ]]; then
  echo "latency log: enabled"
fi
echo

cargo build --release

echo
echo "Starting real macOS input receiver through cg-event."
echo "Leave this terminal open. Stop with Ctrl+C."
env \
  RUST_LOG="${RUST_LOG:-softkvm=info}" \
  SOFTKVM_MAC_MODIFIER_POLICY="${SOFTKVM_MAC_MODIFIER_POLICY:-swap-alt-super}" \
  SOFTKVM_CGEVENT_POINTER_SPEED="${SOFTKVM_CGEVENT_POINTER_SPEED:-1.0}" \
  SOFTKVM_CGEVENT_MOTION_METHOD="${SOFTKVM_CGEVENT_MOTION_METHOD:-event}" \
  SOFTKVM_CGEVENT_TAP="${SOFTKVM_CGEVENT_TAP:-session}" \
  SOFTKVM_LATENCY_LOG="${SOFTKVM_LATENCY_LOG:-0}" \
  SOFTKVM_LATENCY_WARN_MS="${SOFTKVM_LATENCY_WARN_MS:-8}" \
  "$ROOT/target/release/softkvm" client --listen "0.0.0.0:$PORT" --sink cg-event
