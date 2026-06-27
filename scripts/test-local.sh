#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "== Rust fmt =="
cargo fmt -- --check

echo
echo "== macOS build =="
cargo build

echo
echo "== Rust tests =="
cargo test

if [[ "${SOFTKVM_SKIP_WINDOWS_BUILD:-0}" == "1" ]]; then
  echo
  echo "== Windows ARM build skipped =="
  exit 0
fi

if command -v cargo-zigbuild >/dev/null 2>&1 || cargo zigbuild --version >/dev/null 2>&1; then
  echo
  echo "== Windows ARM cross-build =="
  rustup target add aarch64-pc-windows-gnullvm >/dev/null
  cargo zigbuild --target aarch64-pc-windows-gnullvm
else
  echo
  echo "== Windows ARM cross-build skipped: cargo-zigbuild is not installed =="
fi
