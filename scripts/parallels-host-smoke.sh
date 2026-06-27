#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VM="${SOFTKVM_PARALLELS_VM:-Windows 11}"
TARGET="aarch64-pc-windows-gnullvm"
PORT="${SOFTKVM_PORT:-49322}"
MAC_PARALLELS_IP="${SOFTKVM_MAC_PARALLELS_IP:-10.211.55.2}"
LAYOUT="${SOFTKVM_LAYOUT:-mac-left}"
HOST_SECONDS="${SOFTKVM_HOST_SMOKE_SECONDS:-3}"
EXE_MAC="$HOME/Documents/softkvm-win-arm64.exe"
LOG="/tmp/softkvm-parallels-host-smoke-client.log"

cd "$ROOT"

cleanup() {
  if [[ -n "${CLIENT_PID:-}" ]]; then
    kill "$CLIENT_PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

echo "== Starting Parallels VM =="
if ! prlctl list --all "$VM" | grep -q 'running'; then
  prlctl start "$VM"
  sleep 8
fi
prlctl list --all "$VM"

echo
echo "== Checking interactive Parallels user =="
if ! prlctl exec "$VM" --current-user cmd /c whoami; then
  echo "Parallels --current-user failed. The Windows host smoke needs an active logged-in desktop user." >&2
  exit 1
fi

echo
echo "== Building Windows ARM binary =="
rustup target add "$TARGET" >/dev/null
cargo zigbuild --target "$TARGET"
cp "target/$TARGET/debug/softkvm.exe" "$EXE_MAC"
ls -lh "$EXE_MAC"

if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "Port $PORT is already in use on macOS; set SOFTKVM_PORT to another port." >&2
  exit 1
fi

echo
echo "== Starting Mac log client =="
rm -f "$LOG"
RUST_LOG=info target/debug/softkvm client --listen "0.0.0.0:$PORT" --sink log >"$LOG" 2>&1 &
CLIENT_PID=$!

for _ in {1..40}; do
  if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    break
  fi
  sleep 0.25
done

if ! lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "Mac client did not start listening on $PORT" >&2
  tail -80 "$LOG" >&2 || true
  exit 1
fi

PS_SCRIPT=$(cat <<'PS'
$ErrorActionPreference = "Continue"
$ProgressPreference = "SilentlyContinue"
$src = "\\Mac\Home\Documents\softkvm-win-arm64.exe"
$exe = Join-Path $env:TEMP "softkvm-win-arm64.exe"
Copy-Item -Force $src $exe
$out = Join-Path $env:TEMP "softkvm-host-smoke.out"
$err = Join-Path $env:TEMP "softkvm-host-smoke.err"
Remove-Item -Force $out,$err -ErrorAction SilentlyContinue
$p = Start-Process -FilePath $exe -ArgumentList "host --peer __PEER__ --layout __LAYOUT__" -PassThru -RedirectStandardOutput $out -RedirectStandardError $err -WindowStyle Hidden
Start-Sleep -Seconds __SECONDS__
if ($p.HasExited) {
  Write-Output ("EXITED " + $p.ExitCode)
} else {
  Write-Output ("RUNNING " + $p.Id)
  Stop-Process -Id $p.Id -Force
  Start-Sleep -Milliseconds 500
}
Write-Output "---STDOUT---"
if (Test-Path $out) { Get-Content $out }
Write-Output "---STDERR---"
if (Test-Path $err) { Get-Content $err }
PS
)
PS_SCRIPT="${PS_SCRIPT//__PEER__/${MAC_PARALLELS_IP}:$PORT}"
PS_SCRIPT="${PS_SCRIPT//__LAYOUT__/$LAYOUT}"
PS_SCRIPT="${PS_SCRIPT//__SECONDS__/$HOST_SECONDS}"
ENCODED_PS="$(printf '%s' "$PS_SCRIPT" | iconv -f UTF-8 -t UTF-16LE | base64 | tr -d '\n')"

echo
echo "== Running Windows host smoke as current user =="
set +e
WIN_OUTPUT="$(prlctl exec "$VM" --current-user powershell -NoProfile -ExecutionPolicy Bypass -EncodedCommand "$ENCODED_PS" 2>&1)"
WIN_STATUS=$?
set -e
printf '%s\n' "$WIN_OUTPUT"

echo
echo "== Mac client log tail =="
tail -120 "$LOG"

if [[ "$WIN_STATUS" -ne 0 ]]; then
  echo "Windows host smoke command failed with status $WIN_STATUS" >&2
  exit "$WIN_STATUS"
fi

if ! grep -q 'RUNNING ' <<<"$WIN_OUTPUT"; then
  echo "Windows host exited before ${HOST_SECONDS}s; this is not a passing host smoke." >&2
  if grep -qi 'interactive window station' <<<"$WIN_OUTPUT"; then
    echo "It was launched in a non-interactive context. Use prlctl exec --current-user or run it from the desktop user session." >&2
  fi
  exit 1
fi

if ! grep -q 'role: "windows-host"' "$LOG"; then
  echo "Mac client did not receive windows-host hello." >&2
  exit 1
fi

echo
echo "PASS: Windows host stayed alive, connected to Mac, and sent windows-host hello."
