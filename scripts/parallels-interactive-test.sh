#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VM="${SOFTKVM_PARALLELS_VM:-Windows 11}"
TARGET="${SOFTKVM_WINDOWS_TARGET:-aarch64-pc-windows-gnullvm}"
PORT="${SOFTKVM_PORT:-49321}"
MAC_PARALLELS_IP="${SOFTKVM_MAC_PARALLELS_IP:-10.211.55.2}"
LAYOUT="${SOFTKVM_LAYOUT:-mac-left}"
SINK="${SOFTKVM_MAC_SINK:-cg-event}"
MAC_MOTION_MODE="${SOFTKVM_MAC_MOTION_MODE:-coalesced}"
MAC_MOTION_FLUSH_MS="${SOFTKVM_MAC_MOTION_FLUSH_MS:-1}"
CGEVENT_MOTION_METHOD="${SOFTKVM_CGEVENT_MOTION_METHOD:-event}"
CGEVENT_TAP="${SOFTKVM_CGEVENT_TAP:-annotated-session}"
CGEVENT_POINTER_SPEED="${SOFTKVM_CGEVENT_POINTER_SPEED:-1.0}"
MAC_RIGHT_EDGE_RELEASE="${SOFTKVM_MAC_RIGHT_EDGE_RELEASE:-0}"
MOTION_TRANSPORT="${SOFTKVM_MOTION_TRANSPORT:-tcp}"
UDP_SEND_MODE="${SOFTKVM_UDP_SEND_MODE:-coalesced}"
WIN_ACTIVATE_ON_START="${SOFTKVM_WIN_ACTIVATE_ON_START:-1}"
WIN_ENTRY_X_RATIO="${SOFTKVM_WIN_ENTRY_X_RATIO:-0.5}"
WIN_ENTRY_Y_RATIO="${SOFTKVM_WIN_ENTRY_Y_RATIO:-0.5}"
WIN_NO_LOCAL_CAPTURE="${SOFTKVM_WIN_NO_LOCAL_CAPTURE:-1}"
LATENCY_LOG="${SOFTKVM_LATENCY_LOG:-1}"
LATENCY_WARN_MS="${SOFTKVM_LATENCY_WARN_MS:-1}"
MAC_RUST_LOG="${SOFTKVM_MAC_RUST_LOG:-softkvm=info,softkvm::latency=info}"
WIN_RUST_LOG="${SOFTKVM_WIN_RUST_LOG:-softkvm=info,softkvm::latency=info}"
OUT_ROOT="${SOFTKVM_INTERACTIVE_OUT_ROOT:-$ROOT/target/softkvm-parallels-interactive}"
OUT_DIR="${SOFTKVM_INTERACTIVE_OUT:-$OUT_ROOT/$(date +%Y%m%d-%H%M%S)}"
LATEST_LINK="${SOFTKVM_INTERACTIVE_LATEST:-$OUT_ROOT/latest}"
EXE_MAC="$HOME/Documents/softkvm-win-arm64.exe"
WIN_DIR_NAME="${SOFTKVM_WIN_DIR_NAME:-softkvm-vm-interactive}"
WIN_EXE_NAME="softkvm.exe"
WIN_OUT_NAME="softkvm-host.out.log"
WIN_ERR_NAME="softkvm-host.err.log"
MAC_LOG="$OUT_DIR/mac-client.log"
COMMANDS="$OUT_DIR/COMMANDS.txt"

usage() {
  cat <<EOF
Usage: $(basename "$0") <command>

Commands:
  prepare      Build macOS + Windows ARM binary, copy exe into Parallels VM, write commands.
  mac          Run macOS receiver in the foreground on port $PORT.
  win          Start Windows host inside the interactive Parallels user session.
  stop         Kill softkvm host processes inside the Windows VM.
  logs         Print Windows host stdout/stderr and latest macOS log path.
  capture-on   Configure Parallels to keep mouse/keyboard focus inside the VM.
  capture-off  Restore normal Parallels shared/seamless mouse behavior.
  capture-status
              Print current Parallels mouse/keyboard/focus settings.
  all          Alias for prepare, then print the two manual restart commands.

Environment knobs:
  SOFTKVM_PARALLELS_VM="$VM"
  SOFTKVM_PORT="$PORT"
  SOFTKVM_MAC_PARALLELS_IP="$MAC_PARALLELS_IP"
  SOFTKVM_MOTION_TRANSPORT="$MOTION_TRANSPORT"
  SOFTKVM_MAC_SINK="$SINK"
  SOFTKVM_MAC_RIGHT_EDGE_RELEASE="$MAC_RIGHT_EDGE_RELEASE"
EOF
}

cd "$ROOT"

ensure_output_dir() {
  mkdir -p "$OUT_DIR"
  mkdir -p "$(dirname "$LATEST_LINK")"
  if [[ -L "$LATEST_LINK" || -f "$LATEST_LINK" ]]; then
    rm -f "$LATEST_LINK"
  fi
  ln -s "$OUT_DIR" "$LATEST_LINK" 2>/dev/null || true
}

ensure_vm_running() {
  if ! prlctl list --all "$VM" | grep -q 'running'; then
    prlctl start "$VM"
    sleep 8
  fi
}

open_vm_window() {
  prlctl set "$VM" --startup-view window >/dev/null || true
  local home
  home="$(prlctl list -i "$VM" | awk -F': ' '/^Home:/{print $2; exit}')"
  if [[ -n "$home" && -e "$home" ]]; then
    open "$home" >/dev/null 2>&1 || true
  fi
  open -a "Parallels Desktop" >/dev/null 2>&1 || true
}

capture_on() {
  prlctl set "$VM" \
    --smart-mouse-optimize on \
    --sticky-mouse on \
    --keyboard-optimize on \
    --fullscreen-optimize-for-games on \
    --startup-view window
}

capture_off() {
  prlctl set "$VM" \
    --smart-mouse-optimize auto \
    --sticky-mouse off \
    --keyboard-optimize auto \
    --fullscreen-optimize-for-games off \
    --startup-view window
}

capture_status() {
  prlctl list -i "$VM" | sed -n '/Mouse and Keyboard:/,/Print Management:/p;/Fullscreen:/,/Coherence:/p;/Startup and Shutdown:/,/Optimization:/p'
}

build_and_copy() {
  ensure_output_dir
  ensure_vm_running

  echo "== Configuring Parallels capture mode =="
  capture_on
  open_vm_window

  echo
  echo "== Checking interactive Parallels user =="
  prlctl exec "$VM" --current-user cmd /c whoami

  echo
  echo "== Building macOS binary =="
  cargo build
  "$ROOT/target/debug/softkvm" build-info

  echo
  echo "== Building Windows ARM binary =="
  rustup target add "$TARGET" >/dev/null
  cargo zigbuild --target "$TARGET"
  cp "target/$TARGET/debug/softkvm.exe" "$EXE_MAC"
  ls -lh "$EXE_MAC"

  echo
  echo "== Copying binary into Windows VM =="
  prlctl exec "$VM" --current-user powershell -NoProfile -ExecutionPolicy Bypass -Command "\
\$ErrorActionPreference = 'Stop'; \
\$dir = Join-Path \$env:TEMP '$WIN_DIR_NAME'; \
\$exe = Join-Path \$dir '$WIN_EXE_NAME'; \
New-Item -ItemType Directory -Force -Path \$dir | Out-Null; \
Copy-Item -Force '\\\\Mac\\Home\\Documents\\softkvm-win-arm64.exe' \$exe; \
& \$exe build-info"

  write_commands

  echo
  echo "PASS: prepared interactive Parallels test"
  echo "Commands: $COMMANDS"
  echo "Latest: $LATEST_LINK"
}

write_commands() {
  cat >"$COMMANDS" <<EOF
softkvm Parallels interactive test
repo: $ROOT
head: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)
vm: $VM
peer from Windows VM to Mac: ${MAC_PARALLELS_IP}:$PORT
output: $OUT_DIR

1. In terminal A on Mac, run:

cd "$ROOT"
SOFTKVM_INTERACTIVE_OUT="$OUT_DIR" SOFTKVM_PORT="$PORT" ./scripts/parallels-interactive-test.sh mac

2. In terminal B on Mac, start/restart the Windows host inside the VM:

cd "$ROOT"
SOFTKVM_INTERACTIVE_OUT="$OUT_DIR" SOFTKVM_PORT="$PORT" ./scripts/parallels-interactive-test.sh win

3. VM mode starts remote macOS control immediately in the center of the Mac
   display. Do not test the left edge inside Parallels; Parallels re-injects
   the pointer into Windows and creates a host/guest edge loop. Use Ctrl+Alt+\\
   to return from macOS remote mode during VM testing.

4. Stop Windows host:

cd "$ROOT"
SOFTKVM_INTERACTIVE_OUT="$OUT_DIR" ./scripts/parallels-interactive-test.sh stop

5. Restore normal Parallels shared cursor:

cd "$ROOT"
SOFTKVM_INTERACTIVE_OUT="$OUT_DIR" ./scripts/parallels-interactive-test.sh capture-off

Useful status/log commands:

SOFTKVM_INTERACTIVE_OUT="$OUT_DIR" ./scripts/parallels-interactive-test.sh capture-status
SOFTKVM_INTERACTIVE_OUT="$OUT_DIR" ./scripts/parallels-interactive-test.sh logs
./scripts/analyze-latency-log.py "$MAC_LOG"
EOF
}

run_mac() {
  ensure_output_dir
  if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    echo "Port $PORT is already in use on macOS; stop the old receiver first." >&2
    exit 1
  fi

  echo "softkvm repo: $ROOT"
  echo "softkvm head: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
  echo "listen: 0.0.0.0:$PORT"
  echo "sink: $SINK"
  echo "log: $MAC_LOG"
  echo
  cargo build
  env \
    RUST_LOG="$MAC_RUST_LOG" \
    SOFTKVM_LATENCY_LOG="$LATENCY_LOG" \
    SOFTKVM_LATENCY_WARN_MS="$LATENCY_WARN_MS" \
    SOFTKVM_MAC_MODIFIER_POLICY="${SOFTKVM_MAC_MODIFIER_POLICY:-swap-alt-super}" \
    SOFTKVM_MAC_MOTION_MODE="$MAC_MOTION_MODE" \
    SOFTKVM_MAC_MOTION_FLUSH_MS="$MAC_MOTION_FLUSH_MS" \
    SOFTKVM_CGEVENT_MOTION_METHOD="$CGEVENT_MOTION_METHOD" \
    SOFTKVM_CGEVENT_TAP="$CGEVENT_TAP" \
    SOFTKVM_CGEVENT_POINTER_SPEED="$CGEVENT_POINTER_SPEED" \
    SOFTKVM_MAC_RIGHT_EDGE_RELEASE="$MAC_RIGHT_EDGE_RELEASE" \
    "$ROOT/target/debug/softkvm" client --listen "0.0.0.0:$PORT" --sink "$SINK" \
    2>&1 | tee "$MAC_LOG"
}

run_win() {
  ensure_vm_running
  open_vm_window
  stop_win >/dev/null 2>&1 || true

  echo "== Starting Windows host inside VM =="
  prlctl exec "$VM" --current-user powershell -NoProfile -ExecutionPolicy Bypass -Command "\
\$ErrorActionPreference = 'Stop'; \
\$dir = Join-Path \$env:TEMP '$WIN_DIR_NAME'; \
\$exe = Join-Path \$dir '$WIN_EXE_NAME'; \
if (!(Test-Path \$exe)) { throw \"Missing \$exe; run prepare first\" }; \
\$out = Join-Path \$dir '$WIN_OUT_NAME'; \
\$err = Join-Path \$dir '$WIN_ERR_NAME'; \
Remove-Item -Force \$out,\$err -ErrorAction SilentlyContinue; \
\$script = Join-Path \$dir 'run-host.ps1'; \
@'
\$ErrorActionPreference = 'Stop'
\$dir = Join-Path \$env:TEMP '$WIN_DIR_NAME'
\$exe = Join-Path \$dir '$WIN_EXE_NAME'
\$out = Join-Path \$dir '$WIN_OUT_NAME'
\$err = Join-Path \$dir '$WIN_ERR_NAME'
\$env:RUST_LOG = '$WIN_RUST_LOG'
\$env:SOFTKVM_LATENCY_LOG = '$LATENCY_LOG'
\$env:SOFTKVM_LATENCY_WARN_MS = '$LATENCY_WARN_MS'
\$env:SOFTKVM_MOTION_TRANSPORT = '$MOTION_TRANSPORT'
\$env:SOFTKVM_UDP_SEND_MODE = '$UDP_SEND_MODE'
\$softkvmArgs = @('host', '--peer', '${MAC_PARALLELS_IP}:$PORT', '--layout', '$LAYOUT')
if ('$WIN_ACTIVATE_ON_START' -ne '0') {
    \$softkvmArgs += @('--activate-on-start', '--entry-x-ratio', '$WIN_ENTRY_X_RATIO', '--entry-y-ratio', '$WIN_ENTRY_Y_RATIO')
}
if ('$WIN_NO_LOCAL_CAPTURE' -ne '0') {
    \$softkvmArgs += @('--no-local-capture')
}
& \$exe @softkvmArgs > \$out 2> \$err
'@ | Set-Content -Path \$script -Encoding UTF8; \
Write-Output ('wrote ' + \$script)"

  prlctl exec "$VM" --current-user powershell -NoProfile -ExecutionPolicy Bypass -Command "\
\$dir = Join-Path \$env:TEMP '$WIN_DIR_NAME'; \
\$script = Join-Path \$dir 'run-host.ps1'; \
\$shell = New-Object -ComObject Shell.Application; \
\$shell.ShellExecute('powershell.exe', '-NoProfile -ExecutionPolicy Bypass -File \"' + \$script + '\"', \$dir, 'open', 7) | Out-Null; \
Write-Output ('launched ' + \$script)"
  sleep 1
  prlctl exec "$VM" --current-user powershell -NoProfile -ExecutionPolicy Bypass -Command "\
\$p = Get-Process softkvm -ErrorAction SilentlyContinue | Select-Object -First 1; \
if (\$p) { Write-Output ('softkvm host pid=' + \$p.Id + ' peer=${MAC_PARALLELS_IP}:$PORT layout=$LAYOUT') } \
else { Write-Output 'softkvm host process was not found yet; check logs' }"

  echo
  echo "Windows host started. Click the Windows VM window and test edge/hotkey."
  echo "Windows logs: %TEMP%\\$WIN_DIR_NAME\\$WIN_OUT_NAME / %TEMP%\\$WIN_DIR_NAME\\$WIN_ERR_NAME"
}

stop_win() {
  prlctl exec "$VM" --current-user powershell -NoProfile -ExecutionPolicy Bypass -Command "\
Get-Process softkvm -ErrorAction SilentlyContinue | Stop-Process -Force; \
Get-Process softkvm-win-arm64 -ErrorAction SilentlyContinue | Stop-Process -Force; \
Write-Output 'stopped softkvm processes if any'"
}

show_logs() {
  echo "== Windows stdout =="
  prlctl exec "$VM" --current-user powershell -NoProfile -ExecutionPolicy Bypass -Command "\$dir = Join-Path \$env:TEMP '$WIN_DIR_NAME'; \$out = Join-Path \$dir '$WIN_OUT_NAME'; if (Test-Path \$out) { Get-Content \$out -Tail 120 } else { Write-Output \"missing \$out\" }"
  echo
  echo "== Windows stderr =="
  prlctl exec "$VM" --current-user powershell -NoProfile -ExecutionPolicy Bypass -Command "\$dir = Join-Path \$env:TEMP '$WIN_DIR_NAME'; \$err = Join-Path \$dir '$WIN_ERR_NAME'; if (Test-Path \$err) { Get-Content \$err -Tail 120 } else { Write-Output \"missing \$err\" }"
  echo
  echo "Latest macOS interactive output: $LATEST_LINK"
}

cmd="${1:-}"
case "$cmd" in
  prepare)
    build_and_copy
    ;;
  mac)
    run_mac
    ;;
  win)
    run_win
    ;;
  stop)
    ensure_vm_running
    stop_win
    ;;
  logs)
    ensure_vm_running
    show_logs
    ;;
  capture-on)
    ensure_vm_running
    capture_on
    capture_status
    ;;
  capture-off)
    ensure_vm_running
    capture_off
    capture_status
    ;;
  capture-status)
    capture_status
    ;;
  all)
    build_and_copy
    echo
    echo "Prepared. Use two terminals:"
    echo "  A: cd \"$ROOT\" && SOFTKVM_INTERACTIVE_OUT=\"$OUT_DIR\" SOFTKVM_PORT=\"$PORT\" ./scripts/parallels-interactive-test.sh mac"
    echo "  B: cd \"$ROOT\" && SOFTKVM_INTERACTIVE_OUT=\"$OUT_DIR\" SOFTKVM_PORT=\"$PORT\" ./scripts/parallels-interactive-test.sh win"
    ;;
  ""|-h|--help|help)
    usage
    ;;
  *)
    echo "unknown command: $cmd" >&2
    usage >&2
    exit 2
    ;;
esac
