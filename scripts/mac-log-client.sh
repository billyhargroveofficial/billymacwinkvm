#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${SOFTKVM_PORT:-49321}"
SINK="${SOFTKVM_SINK:-log}"

cd "$ROOT"

cargo build
echo "Starting log-only receiver. This prints events but does not move the Mac pointer."
echo "For real macOS input, use ./scripts/mac-cgevent-client.sh"
RUST_LOG="${RUST_LOG:-info}" target/debug/softkvm client --listen "0.0.0.0:$PORT" --sink "$SINK"
