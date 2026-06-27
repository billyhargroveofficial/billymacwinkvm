#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${SOFTKVM_PORT:-49321}"
SINK="${SOFTKVM_SINK:-log}"

cd "$ROOT"

cargo build
RUST_LOG="${RUST_LOG:-info}" target/debug/softkvm client --listen "0.0.0.0:$PORT" --sink "$SINK"
