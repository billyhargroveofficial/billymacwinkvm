#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION_FILE="${SOFTKVM_KIT_VERSION_FILE:-$ROOT/kit-version.txt}"
DIST_DIR="${SOFTKVM_DIST_DIR:-$ROOT/dist}"
X64_TARGET="${SOFTKVM_WINDOWS_X64_TARGET:-x86_64-pc-windows-gnullvm}"
ARM64_TARGET="${SOFTKVM_WINDOWS_ARM64_TARGET:-aarch64-pc-windows-gnullvm}"

cd "$ROOT"

current_version="0"
if [[ -f "$VERSION_FILE" ]]; then
  current_version="$(tr -dc '0-9' <"$VERSION_FILE")"
  current_version="${current_version:-0}"
fi

if [[ -n "${SOFTKVM_KIT_VERSION:-}" ]]; then
  next_version="$(printf '%d' "$SOFTKVM_KIT_VERSION")"
else
  next_version="$((current_version + 1))"
fi

if (( next_version <= 0 )); then
  echo "kit version must be positive, got: $next_version" >&2
  exit 2
fi

version_tag="$(printf 'v%04d' "$next_version")"
git_hash="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
git_dirty="$(git diff --quiet && git diff --cached --quiet && echo clean || echo dirty)"
kit_name="softkvm-windows-test-kit-${version_tag}-${git_hash}"
stage_dir="$DIST_DIR/$kit_name"
zip_path="$DIST_DIR/$kit_name.zip"
latest_zip="$DIST_DIR/softkvm-windows-test-kit-latest.zip"

echo "softkvm repo: $ROOT"
echo "kit version: $version_tag"
echo "git hash: $git_hash ($git_dirty)"
echo "dist: $DIST_DIR"

mkdir -p "$DIST_DIR"

echo
echo "== Build Windows binaries =="
rustup target add "$X64_TARGET" "$ARM64_TARGET" >/dev/null
cargo zigbuild --release --target "$X64_TARGET"
cargo zigbuild --release --target "$ARM64_TARGET"

rm -rf "$stage_dir"
mkdir -p "$stage_dir/scripts" "$stage_dir/docs"

cp "target/$X64_TARGET/release/softkvm.exe" "$stage_dir/softkvm.exe"
cp "target/$ARM64_TARGET/release/softkvm.exe" "$stage_dir/softkvm-arm64.exe"
cp scripts/windows-real-preflight.ps1 "$stage_dir/scripts/windows-real-preflight.ps1"
cp docs/test-plan.md "$stage_dir/docs/test-plan.md"

cat >"$stage_dir/VERSION.txt" <<EOF
softkvm_windows_test_kit=$version_tag
git_hash=$git_hash
git_dirty=$git_dirty
built_at_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)
x64_target=$X64_TARGET
arm64_target=$ARM64_TARGET
EOF

cat >"$stage_dir/README.txt" <<EOF
softkvm Windows test kit $version_tag ($git_hash)

Pick binary:
- softkvm.exe        Windows x64
- softkvm-arm64.exe  Windows ARM64 / Parallels ARM

Run from PowerShell in this folder:

Set-ExecutionPolicy -Scope Process Bypass
.\\scripts\\windows-real-preflight.ps1 -Exe .\\softkvm.exe -Peer "<mac-ip>:49321" -RunHost

For ARM64 Windows, use:

.\\scripts\\windows-real-preflight.ps1 -Exe .\\softkvm-arm64.exe -Peer "<mac-ip>:49321" -RunHost

Check the exact build:

.\\softkvm.exe build-info

Current default host path uses buffered Raw Input and immediate UDP motion.
Fallbacks:

\$env:SOFTKVM_RAW_INPUT_READER="lparam"
.\\scripts\\windows-real-preflight.ps1 -Exe .\\softkvm.exe -Peer "<mac-ip>:49321" -RunHost

\$env:SOFTKVM_MOTION_TRANSPORT="tcp"
.\\scripts\\windows-real-preflight.ps1 -Exe .\\softkvm.exe -Peer "<mac-ip>:49321" -RunHost

Diagnose real Windows mouse cadence before involving macOS:

.\\softkvm.exe win-raw-cadence --seconds 30 --mode raw-only
.\\softkvm.exe win-raw-cadence --seconds 30 --mode hooks-passive
.\\softkvm.exe win-raw-cadence --seconds 30 --mode hooks-suppress
EOF

rm -f "$zip_path" "$latest_zip"
(
  cd "$DIST_DIR"
  COPYFILE_DISABLE=1 zip -qr "$(basename "$zip_path")" "$(basename "$stage_dir")" -x '*.DS_Store' -x '__MACOSX/*'
)
cp "$zip_path" "$latest_zip"

printf '%d\n' "$next_version" >"$VERSION_FILE"

echo
echo "PASS: packaged $zip_path"
echo "latest: $latest_zip"
ls -lh "$zip_path" "$latest_zip"
