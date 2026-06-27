#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export SOFTKVM_MAC_SINK="${SOFTKVM_MAC_SINK:-cg-event}"
exec "$SCRIPT_DIR/mac-karabiner-client.sh" "$@"
