#!/usr/bin/env python3
import re
import sys
from collections import defaultdict
from datetime import datetime
from pathlib import Path

ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
TS_RE = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+)Z")
FLOAT_RE = re.compile(r"([a-zA-Z_]+)=(-?\d+(?:\.\d+)?)")


def clean(line: str) -> str:
    return ANSI_RE.sub("", line)


def timestamp(line: str):
    match = TS_RE.match(line)
    if not match:
        return None
    return datetime.fromisoformat(match.group(1))


def percentile(values, p):
    if not values:
        return None
    values = sorted(values)
    idx = min(len(values) - 1, round((len(values) - 1) * p))
    return values[idx]


def print_stats(name, values):
    if not values:
        return
    print(
        f"{name}: n={len(values)} "
        f"min={min(values):.3f} p50={percentile(values, 0.50):.3f} "
        f"p95={percentile(values, 0.95):.3f} p99={percentile(values, 0.99):.3f} "
        f"max={max(values):.3f}"
    )


def add_gap_report(name, entries):
    if len(entries) < 2:
        return
    gaps = []
    for (prev_t, prev_line), (cur_t, cur_line) in zip(entries, entries[1:]):
        gap = (cur_t - prev_t).total_seconds() * 1000.0
        gaps.append((gap, prev_line, cur_line, prev_t.time(), cur_t.time()))
    values = [gap for gap, *_ in gaps]
    print_stats(f"{name}.gap_ms", values)
    slow = [gap for gap in gaps if gap[0] >= 20.0]
    if slow:
        print(f"{name}.slow_gaps>=20ms: {len(slow)}")
        for gap, prev_line, cur_line, prev_time, cur_time in sorted(slow, reverse=True)[:10]:
            print(
                f"  {gap:8.1f}ms lines {prev_line}->{cur_line} "
                f"{prev_time}->{cur_time}"
            )


def analyze(path: Path):
    series = defaultdict(list)
    metrics = defaultdict(list)
    markers = defaultdict(int)

    for line_no, raw in enumerate(path.read_text(errors="replace").splitlines(), 1):
        line = clean(raw)
        ts = timestamp(line)
        if ts is None:
            continue

        if "udp motion transport confirmed by macOS client" in line:
            markers["windows_udp_confirmed"] += 1
        if "acknowledged udp motion transport" in line:
            markers["mac_udp_ack"] += 1
        if "using udp/binary motion transport" in line:
            markers["windows_udp_selected"] += 1
        if "using tcp/json motion transport" in line:
            markers["windows_tcp_forced"] += 1

        if "input event" in line and "MouseMotion" in line:
            series["log_sink_mouse_motion"].append((ts, line_no))
        if "mac udp motion queue latency" in line:
            series["mac_udp_motion"].append((ts, line_no))
        if "mac frame queue latency" in line and "mouse_motion" in line:
            series["mac_tcp_motion"].append((ts, line_no))
        if "mac input sink apply latency" in line and 'event="mouse_motion"' in line:
            series["mac_apply_motion"].append((ts, line_no))
        if "Karabiner write latency" in line:
            series["karabiner_write"].append((ts, line_no))
        if "windows udp motion send latency" in line:
            series["windows_udp_send"].append((ts, line_no))
        if "windows motion flush batch" in line:
            series["windows_tcp_motion_flush"].append((ts, line_no))
        if "windows mouse raw input handling" in line and "remote_active=true" in line:
            series["windows_raw_mouse_active"].append((ts, line_no))

        for key, value in FLOAT_RE.findall(line):
            if key.endswith("_ms") or key in {"queue_ms", "write_ms", "lock_ms", "elapsed_ms"}:
                label = None
                if "mac udp motion queue latency" in line and key == "queue_ms":
                    label = "mac_udp_queue_ms"
                elif "mac frame queue latency" in line and key == "queue_ms":
                    label = "mac_tcp_queue_ms"
                elif "mac motion apply latency" in line and key == "elapsed_ms":
                    label = "mac_motion_apply_ms"
                elif "mac input sink apply latency" in line and key == "elapsed_ms":
                    label = "mac_input_apply_ms"
                elif "Karabiner write latency" in line:
                    label = f"karabiner_{key}"
                elif "windows udp motion send latency" in line and key == "elapsed_ms":
                    label = "windows_udp_send_ms"
                elif "windows tcp write latency" in line and key == "elapsed_ms":
                    label = "windows_tcp_write_ms"
                elif "windows mouse raw input handling" in line and key == "elapsed_ms":
                    label = "windows_raw_input_ms"
                if label:
                    metrics[label].append(float(value))

    print(f"== {path} ==")
    if markers:
        print("markers:", " ".join(f"{key}={value}" for key, value in sorted(markers.items())))
    for name in sorted(series):
        print(f"{name}.events: {len(series[name])}")
        add_gap_report(name, series[name])
    for name in sorted(metrics):
        print_stats(name, metrics[name])
    print()


def main():
    if len(sys.argv) < 2:
        print("usage: analyze-latency-log.py <log> [<log>...]", file=sys.stderr)
        return 2
    for arg in sys.argv[1:]:
        analyze(Path(arg))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
