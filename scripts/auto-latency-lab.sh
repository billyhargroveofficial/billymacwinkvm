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
OUT_DIR="${SOFTKVM_LAB_OUT:-/tmp/softkvm-auto-latency-lab/$(date +%Y%m%d-%H%M%S)}"
EXE_MAC="$HOME/Documents/softkvm-win-arm64.exe"
EXE_WIN="\\\\Mac\\Home\\Documents\\softkvm-win-arm64.exe"
read -r -a TIMINGS <<<"$TIMINGS_RAW"

cd "$ROOT"
mkdir -p "$OUT_DIR"

PIDS=()
cleanup() {
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" >/dev/null 2>&1 || true
  done
}
trap cleanup EXIT

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

start_log_client() {
  local port="$1"
  local log="$2"
  rm -f "$log"
  env RUST_LOG=info SOFTKVM_LATENCY_LOG=1 SOFTKVM_LATENCY_WARN_MS=1 \
    "$ROOT/target/debug/softkvm" client --listen "0.0.0.0:$port" --sink log \
    >"$log" 2>&1 &
  local pid="$!"
  PIDS+=("$pid")
  wait_for_port "$port" "$log"
  echo "$pid"
}

stop_pid() {
  local pid="$1"
  kill "$pid" >/dev/null 2>&1 || true
  wait "$pid" >/dev/null 2>&1 || true
}

analyze_log() {
  local log="$1"
  local report="$2"
  "$ROOT/scripts/analyze-latency-log.py" "$log" >"$report"
  cat "$report"
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
  local pid
  pid="$(start_log_client "$port" "$log")"
  "$ROOT/target/debug/softkvm" motion-bench \
    --peer "127.0.0.1:$port" \
    --transport "$transport" \
    --timing "$timing" \
    --hz "$HZ" \
    --seconds "$DURATION" \
    >"$bench" 2>&1
  sleep 0.4
  stop_pid "$pid"
  cat "$bench"
  analyze_log "$log" "$report"
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
  pid="$(start_log_client "$port" "$log")"
  set +e
  prlctl exec "$VM" powershell -NoProfile -ExecutionPolicy Bypass -Command \
    "& '$EXE_WIN' build-info; & '$EXE_WIN' motion-bench --peer ${MAC_PARALLELS_IP}:$port --transport $transport --timing $timing --hz $HZ --seconds $DURATION" \
    >"$bench" 2>&1
  local status="$?"
  set -e
  sleep 0.4
  stop_pid "$pid"
  cat "$bench"
  analyze_log "$log" "$report"
  if [[ "$status" -ne 0 ]]; then
    echo "Parallels $transport bench failed with status $status" >&2
    return "$status"
  fi
}

echo "softkvm repo: $ROOT"
echo "softkvm head: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
echo "output: $OUT_DIR"
echo "bench: ${HZ}Hz for ${DURATION}s timings=${TIMINGS[*]}"

echo
echo "== Build Mac binary =="
cargo build
"$ROOT/target/debug/softkvm" build-info

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
  cargo zigbuild --target "$TARGET"
  cp "target/$TARGET/debug/softkvm.exe" "$EXE_MAC"
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
echo "Reports are in: $OUT_DIR"
