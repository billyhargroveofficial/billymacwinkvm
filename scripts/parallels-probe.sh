#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VM="${SOFTKVM_PARALLELS_VM:-Windows 11}"
TARGET="aarch64-pc-windows-gnullvm"
PORT="${SOFTKVM_PORT:-49321}"
MAC_PARALLELS_IP="${SOFTKVM_MAC_PARALLELS_IP:-10.211.55.2}"
EXE_MAC="$HOME/Documents/softkvm-win-arm64.exe"
EXE_WIN="\\\\Mac\\Home\\Documents\\softkvm-win-arm64.exe"
LOG="/tmp/softkvm-parallels-client.log"

cd "$ROOT"

echo "== Starting Parallels VM =="
if ! prlctl list --all "$VM" | grep -q 'running'; then
  prlctl start "$VM"
  sleep 8
fi
prlctl list --all "$VM"

echo
echo "== Building Windows ARM binary =="
rustup target add "$TARGET" >/dev/null
cargo zigbuild --release --target "$TARGET"
cp "target/$TARGET/release/softkvm.exe" "$EXE_MAC"
ls -lh "$EXE_MAC"

echo
echo "== Starting Mac log client =="
rm -f "$LOG"
RUST_LOG=info cargo run -- client --listen "0.0.0.0:$PORT" --sink log >"$LOG" 2>&1 &
CLIENT_PID=$!
cleanup() {
  kill "$CLIENT_PID" >/dev/null 2>&1 || true
}
trap cleanup EXIT

for _ in {1..40}; do
  if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    break
  fi
  sleep 0.25
done

echo
echo "== Checking Windows access =="
prlctl exec "$VM" powershell -NoProfile -Command \
  "Test-Path '$EXE_WIN'; & '$EXE_WIN' --help | Select-Object -First 2; Test-NetConnection $MAC_PARALLELS_IP -Port $PORT | Select-Object ComputerName,RemoteAddress,TcpTestSucceeded | Format-List"

echo
echo "== Running Windows probe =="
prlctl exec "$VM" powershell -NoProfile -Command \
  "& '$EXE_WIN' probe --peer ${MAC_PARALLELS_IP}:$PORT"

sleep 0.5
echo
echo "== Mac client log tail =="
tail -80 "$LOG"

if ! grep -q 'role: "probe"' "$LOG"; then
  echo "Mac client did not receive probe hello." >&2
  exit 1
fi

if ! grep -q 'MouseButton { button: Left, state: Down }' "$LOG"; then
  echo "Mac client did not receive the synthetic left-click probe event." >&2
  exit 1
fi

echo
echo "PASS: Windows probe reached Mac and delivered synthetic mouse events."
