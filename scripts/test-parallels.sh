#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "== Parallels synthetic transport probe =="
./scripts/parallels-probe.sh

echo
echo "== Parallels Windows host startup smoke =="
./scripts/parallels-host-smoke.sh
