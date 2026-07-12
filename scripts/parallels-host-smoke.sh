#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VM="${SOFTKVM_PARALLELS_VM:-Windows 11}"
TARGET="aarch64-pc-windows-gnullvm"
PORT="${SOFTKVM_PORT:-49322}"
MAC_PARALLELS_IP="${SOFTKVM_MAC_PARALLELS_IP:-10.211.55.2}"
LAYOUT="${SOFTKVM_LAYOUT:-mac-left}"
HOST_SECONDS="${SOFTKVM_HOST_SMOKE_SECONDS:-3}"
SINK="${SOFTKVM_HOST_SMOKE_SINK:-${SOFTKVM_LAB_SINK:-cg-event}}"
MAC_MOTION_MODE="${SOFTKVM_MAC_MOTION_MODE:-coalesced}"
MAC_MOTION_FLUSH_MS="${SOFTKVM_MAC_MOTION_FLUSH_MS:-1}"
CGEVENT_MOTION_METHOD="${SOFTKVM_CGEVENT_MOTION_METHOD:-event}"
CGEVENT_TAP="${SOFTKVM_CGEVENT_TAP:-session}"
CGEVENT_POINTER_SPEED="${SOFTKVM_CGEVENT_POINTER_SPEED:-1.0}"
UDP_SEND_MODE="${SOFTKVM_UDP_SEND_MODE:-immediate}"
MOTION_TRANSPORT="${SOFTKVM_MOTION_TRANSPORT:-udp}"
LATENCY_LOG="${SOFTKVM_LATENCY_LOG:-1}"
LATENCY_WARN_MS="${SOFTKVM_LATENCY_WARN_MS:-1}"
HOST_RUST_LOG="${SOFTKVM_HOST_SMOKE_RUST_LOG:-softkvm=info,softkvm::latency=info}"
OUT_ROOT="${SOFTKVM_HOST_SMOKE_OUT_ROOT:-$ROOT/target/softkvm-parallels-host-smoke}"
OUT_DIR="${SOFTKVM_HOST_SMOKE_OUT:-$OUT_ROOT/$(date +%Y%m%d-%H%M%S)}"
LATEST_LINK="${SOFTKVM_HOST_SMOKE_LATEST:-$OUT_ROOT/latest}"
DRY_RUN="${SOFTKVM_HOST_SMOKE_DRY_RUN:-0}"
EXE_MAC="$HOME/Documents/softkvm-win-arm64.exe"
LOG="${SOFTKVM_HOST_SMOKE_CLIENT_LOG:-$OUT_DIR/client.log}"
WIN_LOG="$OUT_DIR/windows-host.log"
REPORT="$OUT_DIR/client.report.txt"
SUMMARY="$OUT_DIR/SUMMARY.txt"

cd "$ROOT"
mkdir -p "$OUT_DIR"
mkdir -p "$(dirname "$LOG")"
mkdir -p "$(dirname "$LATEST_LINK")"
if [[ -L "$LATEST_LINK" || -f "$LATEST_LINK" ]]; then
  rm -f "$LATEST_LINK"
fi
ln -s "$OUT_DIR" "$LATEST_LINK" 2>/dev/null || true

cleanup() {
  if [[ -n "${CLIENT_PID:-}" ]]; then
    kill "$CLIENT_PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

write_summary_header() {
  {
    echo "softkvm Parallels host smoke"
    echo "repo: $ROOT"
    echo "head: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    echo "output: $OUT_DIR"
    echo "latest: $LATEST_LINK"
    echo "vm: $VM"
    echo "peer: ${MAC_PARALLELS_IP}:$PORT"
    echo "layout: $LAYOUT"
    echo "host seconds: $HOST_SECONDS"
    echo "client sink: $SINK"
    echo "mac motion mode: $MAC_MOTION_MODE"
    echo "mac motion flush ms: $MAC_MOTION_FLUSH_MS"
    echo "cgevent motion method: $CGEVENT_MOTION_METHOD"
    echo "cgevent tap: $CGEVENT_TAP"
    echo "cgevent pointer speed: $CGEVENT_POINTER_SPEED"
    echo "windows udp send mode: $UDP_SEND_MODE"
    echo "windows motion transport: $MOTION_TRANSPORT"
    echo "latency log: $LATENCY_LOG"
    echo "latency warn ms: $LATENCY_WARN_MS"
    echo "host rust log: $HOST_RUST_LOG"
    echo "client_log=$LOG"
    echo "windows_log=$WIN_LOG"
    echo "report=$REPORT"
  } >"$SUMMARY"
}

print_config() {
  echo "softkvm repo: $ROOT"
  echo "softkvm head: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
  echo "output: $OUT_DIR"
  echo "latest: $LATEST_LINK"
  echo "vm: $VM"
  echo "peer: ${MAC_PARALLELS_IP}:$PORT"
  echo "layout: $LAYOUT"
  echo "client sink: $SINK"
  echo "mac motion mode: $MAC_MOTION_MODE"
  echo "mac motion flush ms: $MAC_MOTION_FLUSH_MS"
  echo "cgevent motion method: $CGEVENT_MOTION_METHOD"
  echo "cgevent tap: $CGEVENT_TAP"
  echo "cgevent pointer speed: $CGEVENT_POINTER_SPEED"
  echo "windows udp send mode: $UDP_SEND_MODE"
  echo "windows motion transport: $MOTION_TRANSPORT"
  echo "latency log: $LATENCY_LOG"
  echo "latency warn ms: $LATENCY_WARN_MS"
  echo "host rust log: $HOST_RUST_LOG"
}

print_dry_run_plan() {
  echo
  echo "== Dry run =="
  echo "Would start Parallels VM '$VM' if needed."
  echo "Would build macOS softkvm and Windows target $TARGET."
  echo "Would start Mac client:"
  echo "  RUST_LOG=$HOST_RUST_LOG SOFTKVM_LATENCY_LOG=$LATENCY_LOG SOFTKVM_LATENCY_WARN_MS=$LATENCY_WARN_MS SOFTKVM_MAC_MOTION_MODE=$MAC_MOTION_MODE SOFTKVM_MAC_MOTION_FLUSH_MS=$MAC_MOTION_FLUSH_MS SOFTKVM_CGEVENT_MOTION_METHOD=$CGEVENT_MOTION_METHOD SOFTKVM_CGEVENT_TAP=$CGEVENT_TAP SOFTKVM_CGEVENT_POINTER_SPEED=$CGEVENT_POINTER_SPEED SOFTKVM_UDP_SEND_MODE=$UDP_SEND_MODE SOFTKVM_MOTION_TRANSPORT=$MOTION_TRANSPORT"
  echo "  $ROOT/target/release/softkvm client --listen 0.0.0.0:$PORT --sink $SINK"
  echo "Would run Windows host as current user against ${MAC_PARALLELS_IP}:$PORT for ${HOST_SECONDS}s."
  echo
  echo "Dry-run summary: $SUMMARY"
  echo "Latest link: $LATEST_LINK"
}

print_config
write_summary_header

if [[ "$DRY_RUN" == "1" ]]; then
  print_dry_run_plan
  exit 0
fi

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
echo "== Building Mac binary =="
cargo build --release
"$ROOT/target/release/softkvm" build-info

echo
echo "== Building Windows ARM binary =="
rustup target add "$TARGET" >/dev/null
cargo zigbuild --release --target "$TARGET"
cp "target/$TARGET/release/softkvm.exe" "$EXE_MAC"
ls -lh "$EXE_MAC"

if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "Port $PORT is already in use on macOS; set SOFTKVM_PORT to another port." >&2
  exit 1
fi

echo
echo "== Starting Mac $SINK client =="
rm -f "$LOG"
env \
  RUST_LOG="$HOST_RUST_LOG" \
  SOFTKVM_LATENCY_LOG="$LATENCY_LOG" \
  SOFTKVM_LATENCY_WARN_MS="$LATENCY_WARN_MS" \
  SOFTKVM_MAC_MOTION_MODE="$MAC_MOTION_MODE" \
  SOFTKVM_MAC_MOTION_FLUSH_MS="$MAC_MOTION_FLUSH_MS" \
  SOFTKVM_CGEVENT_MOTION_METHOD="$CGEVENT_MOTION_METHOD" \
  SOFTKVM_CGEVENT_TAP="$CGEVENT_TAP" \
  SOFTKVM_CGEVENT_POINTER_SPEED="$CGEVENT_POINTER_SPEED" \
  "$ROOT/target/release/softkvm" client --listen "0.0.0.0:$PORT" --sink "$SINK" \
  >"$LOG" 2>&1 &
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
$env:SOFTKVM_MOTION_TRANSPORT = "__MOTION_TRANSPORT__"
$env:SOFTKVM_UDP_SEND_MODE = "__UDP_SEND_MODE__"
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
PS_SCRIPT="${PS_SCRIPT//__MOTION_TRANSPORT__/$MOTION_TRANSPORT}"
PS_SCRIPT="${PS_SCRIPT//__UDP_SEND_MODE__/$UDP_SEND_MODE}"
ENCODED_PS="$(printf '%s' "$PS_SCRIPT" | iconv -f UTF-8 -t UTF-16LE | base64 | tr -d '\n')"

echo
echo "== Running Windows host smoke as current user =="
set +e
WIN_OUTPUT="$(prlctl exec "$VM" --current-user powershell -NoProfile -ExecutionPolicy Bypass -EncodedCommand "$ENCODED_PS" 2>&1)"
WIN_STATUS=$?
set -e
printf '%s\n' "$WIN_OUTPUT" >"$WIN_LOG"
printf '%s\n' "$WIN_OUTPUT"

echo
echo "== Mac client log tail =="
tail -120 "$LOG"

echo
echo "== Latency report =="
"$ROOT/scripts/analyze-latency-log.py" "$LOG" >"$REPORT"
cat "$REPORT"

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
echo "Summary: $SUMMARY"
echo "Latest link: $LATEST_LINK"
echo "Reports are in: $OUT_DIR"
