#!/usr/bin/env python3
import re
import sys
from collections import defaultdict
from datetime import datetime
from pathlib import Path

ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
TS_RE = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+)Z")
FLOAT_RE = re.compile(r"([a-zA-Z_]+)=(-?\d+(?:\.\d+)?)")
SLOW_P95_MS = 8.0
SLOW_MAX_MS = 20.0


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


def summarize(values):
    if not values:
        return None
    return {
        "n": len(values),
        "min": min(values),
        "p50": percentile(values, 0.50),
        "p95": percentile(values, 0.95),
        "p99": percentile(values, 0.99),
        "max": max(values),
    }


def print_stats(name, values):
    stat = summarize(values)
    if not stat:
        return
    print(
        f"{name}: n={stat['n']} "
        f"min={stat['min']:.3f} p50={stat['p50']:.3f} "
        f"p95={stat['p95']:.3f} p99={stat['p99']:.3f} "
        f"max={stat['max']:.3f}"
    )


def format_stat(name, stat):
    return (
        f"{name} n={stat['n']} p95={stat['p95']:.3f}ms "
        f"max={stat['max']:.3f}ms"
    )


def slow_score(stat):
    return max(stat["p95"] / SLOW_P95_MS, stat["max"] / SLOW_MAX_MS)


def is_slow(stat):
    return stat["p95"] >= SLOW_P95_MS or stat["max"] >= SLOW_MAX_MS


def strongest_metric(metrics, names):
    strongest = None
    for name in names:
        stat = summarize(metrics.get(name, []))
        if not stat:
            continue
        candidate = (slow_score(stat), name, stat)
        if strongest is None or candidate[0] > strongest[0]:
            strongest = candidate
    return strongest


def print_likely_source_summary(metrics, series):
    source_rules = [
        (
            "Windows send/capture",
            [
                "windows_raw_input_ms",
                "windows_udp_send_ms",
            ],
        ),
        (
            "network or Mac receive gap",
            [
                "mac_udp_receive_gap_ms",
            ],
        ),
        (
            "CGEvent/warp slow call",
            [
                "mac_cgevent_warp_ms",
                "mac_cgevent_mouse_post_post_ms",
                "mac_cgevent_mouse_post_total_ms",
                "mac_direct_immediate_apply_ms",
            ],
        ),
        (
            "visible Mac apply/cadence gap",
            [
                "mac_apply_motion_gap_ms",
                "mac_cgevent_mouse_post_gap_ms",
                "log_sink_mouse_motion_gap_ms",
                "mac_motion_coalesced_flush_gap_ms",
                "mac_tcp_motion_gap_ms",
            ],
        ),
    ]

    evidence = []
    for source, names in source_rules:
        strongest = strongest_metric(metrics, names)
        if not strongest:
            continue
        score, metric_name, stat = strongest
        if is_slow(stat):
            evidence.append((score, source, metric_name, stat))

    print("summary:")
    print(
        f"  thresholds: p95>={SLOW_P95_MS:.1f}ms or max>={SLOW_MAX_MS:.1f}ms"
    )
    if evidence:
        print("  likely_sources:")
        for _, source, metric_name, stat in sorted(evidence, reverse=True):
            print(f"    - {source}: {format_stat(metric_name, stat)}")
            if (
                source == "CGEvent/warp slow call"
                and metric_name == "mac_direct_immediate_apply_ms"
                and not series.get("mac_cgevent_warp")
                and not series.get("mac_cgevent_mouse_post")
            ):
                print(
                    "      note: direct Mac apply was slow, but no CGEvent marker "
                    "was present in this log"
                )
    else:
        print(
            "  likely_source: no in-process evidence "
            "(no parsed current marker crossed the threshold)"
        )

    for name in [
        "windows_raw_input_ms",
        "windows_udp_send_ms",
        "mac_udp_receive_gap_ms",
        "mac_direct_immediate_apply_ms",
        "mac_cgevent_warp_ms",
        "mac_cgevent_mouse_post_total_ms",
        "mac_apply_motion_gap_ms",
        "mac_cgevent_mouse_post_gap_ms",
        "log_sink_mouse_motion_gap_ms",
    ]:
        stat = summarize(metrics.get(name, []))
        if stat:
            print(f"  evidence: {format_stat(name, stat)}")


def add_gap_report(name, entries, metrics):
    if len(entries) < 2:
        return
    gaps = []
    for (prev_t, prev_line), (cur_t, cur_line) in zip(entries, entries[1:]):
        gap = (cur_t - prev_t).total_seconds() * 1000.0
        gaps.append((gap, prev_line, cur_line, prev_t.time(), cur_t.time()))
    values = [gap for gap, *_ in gaps]
    metrics[f"{name}_gap_ms"].extend(values)
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
        if "mac motion coalesced flush" in line:
            series["mac_motion_coalesced_flush"].append((ts, line_no))
        if "mac udp motion receive gap" in line:
            series["mac_udp_receive_gap"].append((ts, line_no))
        if "mac direct immediate motion apply latency" in line:
            series["mac_direct_immediate_apply"].append((ts, line_no))
        if "cgevent warp motion latency" in line:
            series["mac_cgevent_warp"].append((ts, line_no))
        if "cgevent mouse post latency" in line:
            series["mac_cgevent_mouse_post"].append((ts, line_no))
        if "Karabiner write latency" in line:
            series["karabiner_write"].append((ts, line_no))
        if "windows udp motion send latency" in line:
            series["windows_udp_send"].append((ts, line_no))
        if "windows motion flush batch" in line:
            series["windows_tcp_motion_flush"].append((ts, line_no))
        if "windows mouse raw input handling" in line:
            series["windows_raw_mouse"].append((ts, line_no))
            if "remote_active=true" in line:
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
                elif "mac motion coalesced flush" in line and key == "queued_ms":
                    label = "mac_motion_coalesced_queue_ms"
                elif "mac udp motion receive gap" in line and key == "gap_ms":
                    label = "mac_udp_receive_gap_ms"
                elif "mac direct immediate motion apply latency" in line and key == "elapsed_ms":
                    label = "mac_direct_immediate_apply_ms"
                elif "cgevent warp motion latency" in line and key == "elapsed_ms":
                    label = "mac_cgevent_warp_ms"
                elif "cgevent mouse post latency" in line:
                    label = f"mac_cgevent_mouse_post_{key}"
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
        add_gap_report(name, series[name], metrics)
    for name in sorted(metrics):
        print_stats(name, metrics[name])
    print_likely_source_summary(metrics, series)
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
