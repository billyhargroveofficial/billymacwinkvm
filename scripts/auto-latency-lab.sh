#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VM="${SOFTKVM_PARALLELS_VM:-Windows 11}"
TARGET="${SOFTKVM_WINDOWS_TARGET:-aarch64-pc-windows-gnullvm}"
MAC_PARALLELS_IP="${SOFTKVM_MAC_PARALLELS_IP:-10.211.55.2}"
HZ="${SOFTKVM_BENCH_HZ:-200}"
DURATION="${SOFTKVM_BENCH_SECONDS:-8}"
TIMINGS_RAW="${SOFTKVM_BENCH_TIMINGS:-sleep spin}"
BASE_PORT="${SOFTKVM_BENCH_BASE_PORT:-49431}"
SINK="${SOFTKVM_LAB_SINK:-cg-event}"
MAC_MOTION_MODE="${SOFTKVM_MAC_MOTION_MODE:-coalesced}"
MAC_MOTION_FLUSH_MS="${SOFTKVM_MAC_MOTION_FLUSH_MS:-1}"
CGEVENT_MOTION_METHOD="${SOFTKVM_CGEVENT_MOTION_METHOD:-event}"
CGEVENT_TAP="${SOFTKVM_CGEVENT_TAP:-session}"
CGEVENT_POINTER_SPEED="${SOFTKVM_CGEVENT_POINTER_SPEED:-1.0}"
UDP_SEND_MODE="${SOFTKVM_UDP_SEND_MODE:-immediate}"
LATENCY_LOG="${SOFTKVM_LATENCY_LOG:-1}"
LATENCY_WARN_MS="${SOFTKVM_LATENCY_WARN_MS:-1}"
LAB_RUST_LOG="${SOFTKVM_LAB_RUST_LOG:-softkvm=info,softkvm::latency=info}"
LAB_ROOT="${SOFTKVM_LAB_ROOT:-$ROOT/target/softkvm-latency-lab}"
OUT_DIR="${SOFTKVM_LAB_OUT:-$LAB_ROOT/$(date +%Y%m%d-%H%M%S)}"
LATEST_LINK="${SOFTKVM_LAB_LATEST:-$LAB_ROOT/latest}"
DRY_RUN="${SOFTKVM_LAB_DRY_RUN:-0}"
SUMMARY="$OUT_DIR/SUMMARY.txt"
EXE_MAC="$HOME/Documents/softkvm-win-arm64.exe"
EXE_WIN="\\\\Mac\\Home\\Documents\\softkvm-win-arm64.exe"
read -r -a TIMINGS <<<"$TIMINGS_RAW"

cd "$ROOT"
mkdir -p "$OUT_DIR"
mkdir -p "$(dirname "$LATEST_LINK")"
if [[ -L "$LATEST_LINK" || -f "$LATEST_LINK" ]]; then
  rm -f "$LATEST_LINK"
fi
ln -s "$OUT_DIR" "$LATEST_LINK" 2>/dev/null || true

PIDS=()
cleanup() {
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" >/dev/null 2>&1 || true
  done
}
trap cleanup EXIT

write_summary_header() {
  {
    echo "softkvm auto latency lab"
    echo "repo: $ROOT"
    echo "head: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    echo "output: $OUT_DIR"
    echo "latest: $LATEST_LINK"
    echo "vm: $VM"
    echo "mac parallels ip: $MAC_PARALLELS_IP"
    echo "bench: ${HZ}Hz for ${DURATION}s timings=${TIMINGS[*]}"
    echo "client sink: $SINK"
    echo "mac motion mode: $MAC_MOTION_MODE"
    echo "mac motion flush ms: $MAC_MOTION_FLUSH_MS"
    echo "cgevent motion method: $CGEVENT_MOTION_METHOD"
    echo "cgevent tap: $CGEVENT_TAP"
    echo "cgevent pointer speed: $CGEVENT_POINTER_SPEED"
    echo "windows udp send mode: $UDP_SEND_MODE"
    echo "latency log: $LATENCY_LOG"
    echo "latency warn ms: $LATENCY_WARN_MS"
    echo "lab rust log: $LAB_RUST_LOG"
    echo
    echo "case files:"
  } >"$SUMMARY"
}

append_case_summary() {
  local name="$1"
  local log="$2"
  local bench="$3"
  local report="$4"
  {
    echo
    echo "[$name]"
    echo "client_log=$log"
    echo "bench_log=$bench"
    echo "report=$report"
  } >>"$SUMMARY"
}

print_dry_run_plan() {
  echo
  echo "== Dry run =="
  echo "Would build macOS softkvm and start clients with:"
  echo "  RUST_LOG=$LAB_RUST_LOG"
  echo "  SOFTKVM_LATENCY_LOG=$LATENCY_LOG"
  echo "  SOFTKVM_LATENCY_WARN_MS=$LATENCY_WARN_MS"
  echo "  SOFTKVM_MAC_MOTION_MODE=$MAC_MOTION_MODE"
  echo "  SOFTKVM_MAC_MOTION_FLUSH_MS=$MAC_MOTION_FLUSH_MS"
  echo "  SOFTKVM_CGEVENT_MOTION_METHOD=$CGEVENT_MOTION_METHOD"
  echo "  SOFTKVM_CGEVENT_TAP=$CGEVENT_TAP"
  echo "  SOFTKVM_CGEVENT_POINTER_SPEED=$CGEVENT_POINTER_SPEED"
  echo "  SOFTKVM_UDP_SEND_MODE=$UDP_SEND_MODE"
  echo "  $ROOT/target/release/softkvm client --listen 0.0.0.0:<port> --sink $SINK"
  echo
  echo "Would run local cases:"
  local port="$BASE_PORT"
  local timing
  for timing in "${TIMINGS[@]}"; do
    echo "  local udp $timing on port $port"
    port="$((port + 1))"
    echo "  local tcp $timing on port $port"
    port="$((port + 1))"
  done
  if [[ "${SOFTKVM_SKIP_PARALLELS:-0}" != "1" ]]; then
    echo
    echo "Would prepare Parallels VM '$VM', copy $EXE_MAC, and run:"
    port="$((BASE_PORT + 20))"
    for timing in "${TIMINGS[@]}"; do
      echo "  parallels udp $timing on port $port via ${MAC_PARALLELS_IP}:$port"
      port="$((port + 1))"
      echo "  parallels tcp $timing on port $port via ${MAC_PARALLELS_IP}:$port"
      port="$((port + 1))"
    done
  fi
  echo
  echo "Dry-run summary: $SUMMARY"
  echo "Latest link: $LATEST_LINK"
}

wait_for_port() {
  local port="$1"
  local log="$2"
  for _ in {1..80}; do
    if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  echo "client did not listen on port $port" >&2
  tail -120 "$log" >&2 || true
  return 1
}

start_client() {
  local port="$1"
  local log="$2"
  rm -f "$log"
  env \
    RUST_LOG="$LAB_RUST_LOG" \
    SOFTKVM_LATENCY_LOG="$LATENCY_LOG" \
    SOFTKVM_LATENCY_WARN_MS="$LATENCY_WARN_MS" \
    SOFTKVM_MAC_MOTION_MODE="$MAC_MOTION_MODE" \
    SOFTKVM_MAC_MOTION_FLUSH_MS="$MAC_MOTION_FLUSH_MS" \
    SOFTKVM_CGEVENT_MOTION_METHOD="$CGEVENT_MOTION_METHOD" \
    SOFTKVM_CGEVENT_TAP="$CGEVENT_TAP" \
    SOFTKVM_CGEVENT_POINTER_SPEED="$CGEVENT_POINTER_SPEED" \
    "$ROOT/target/release/softkvm" client --listen "0.0.0.0:$port" --sink "$SINK" \
    >"$log" 2>&1 &
  local pid="$!"
  PIDS+=("$pid")
  wait_for_port "$port" "$log"
  CLIENT_PID="$pid"
}

stop_pid() {
  local pid="$1"
  kill "$pid" >/dev/null 2>&1 || true
  wait "$pid" >/dev/null 2>&1 || true
}

analyze_log() {
  local name="$1"
  local log="$2"
  local bench="$3"
  local report="$4"
  "$ROOT/scripts/analyze-latency-log.py" "$log" >"$report"
  cat "$report"
  append_case_summary "$name" "$log" "$bench" "$report"
}

run_local_case() {
  local transport="$1"
  local timing="$2"
  local port="$3"
  local name="local-$transport-$timing"
  local log="$OUT_DIR/$name.client.log"
  local bench="$OUT_DIR/$name.bench.log"
  local report="$OUT_DIR/$name.report.txt"

  echo
  echo "== $name =="
  local pid status
  start_client "$port" "$log"
  pid="$CLIENT_PID"
  set +e
  "$ROOT/target/release/softkvm" motion-bench \
    --peer "127.0.0.1:$port" \
    --transport "$transport" \
    --timing "$timing" \
    --hz "$HZ" \
    --seconds "$DURATION" \
    >"$bench" 2>&1
  status="$?"
  set -e
  sleep 0.4
  stop_pid "$pid"
  cat "$bench"
  analyze_log "$name" "$log" "$bench" "$report"
  if [[ "$status" -ne 0 ]]; then
    echo "Local $transport bench failed with status $status" >&2
    return "$status"
  fi
}

run_parallels_case() {
  local transport="$1"
  local timing="$2"
  local port="$3"
  local name="parallels-$transport-$timing"
  local log="$OUT_DIR/$name.client.log"
  local bench="$OUT_DIR/$name.bench.log"
  local report="$OUT_DIR/$name.report.txt"

  echo
  echo "== $name =="
  local pid
  start_client "$port" "$log"
  pid="$CLIENT_PID"
  set +e
  prlctl exec "$VM" powershell -NoProfile -ExecutionPolicy Bypass -Command \
    "\$env:SOFTKVM_UDP_SEND_MODE='$UDP_SEND_MODE'; & '$EXE_WIN' build-info; & '$EXE_WIN' motion-bench --peer ${MAC_PARALLELS_IP}:$port --transport $transport --timing $timing --hz $HZ --seconds $DURATION" \
    >"$bench" 2>&1
  local status="$?"
  set -e
  sleep 0.4
  stop_pid "$pid"
  cat "$bench"
  analyze_log "$name" "$log" "$bench" "$report"
  if [[ "$status" -ne 0 ]]; then
    echo "Parallels $transport bench failed with status $status" >&2
    return "$status"
  fi
}

echo "softkvm repo: $ROOT"
echo "softkvm head: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
echo "output: $OUT_DIR"
echo "latest: $LATEST_LINK"
echo "bench: ${HZ}Hz for ${DURATION}s timings=${TIMINGS[*]}"
echo "client sink: $SINK"
echo "mac motion mode: $MAC_MOTION_MODE"
echo "mac motion flush ms: $MAC_MOTION_FLUSH_MS"
echo "cgevent motion method: $CGEVENT_MOTION_METHOD"
echo "cgevent tap: $CGEVENT_TAP"
echo "cgevent pointer speed: $CGEVENT_POINTER_SPEED"
echo "windows udp send mode: $UDP_SEND_MODE"
echo "latency log: $LATENCY_LOG"
echo "latency warn ms: $LATENCY_WARN_MS"
echo "lab rust log: $LAB_RUST_LOG"

write_summary_header

if [[ "$DRY_RUN" == "1" ]]; then
  print_dry_run_plan
  exit 0
fi

echo
echo "== Build Mac binary =="
cargo build --release
"$ROOT/target/release/softkvm" build-info

port="$BASE_PORT"
for timing in "${TIMINGS[@]}"; do
  run_local_case udp "$timing" "$port"
  port="$((port + 1))"
  run_local_case tcp "$timing" "$port"
  port="$((port + 1))"
done

if [[ "${SOFTKVM_SKIP_PARALLELS:-0}" != "1" ]]; then
  echo
  echo "== Prepare Parallels Windows binary =="
  if ! prlctl list --all "$VM" | grep -q 'running'; then
    prlctl start "$VM"
    sleep 8
  fi
  prlctl list --all "$VM"
  rustup target add "$TARGET" >/dev/null
  cargo zigbuild --release --target "$TARGET"
  cp "target/$TARGET/release/softkvm.exe" "$EXE_MAC"
  ls -lh "$EXE_MAC"

  port="$((BASE_PORT + 20))"
  for timing in "${TIMINGS[@]}"; do
    run_parallels_case udp "$timing" "$port"
    port="$((port + 1))"
    run_parallels_case tcp "$timing" "$port"
    port="$((port + 1))"
  done
fi

echo
echo "PASS: auto latency lab complete"
echo "Summary: $SUMMARY"
echo "Latest link: $LATEST_LINK"
echo "Reports are in: $OUT_DIR"
