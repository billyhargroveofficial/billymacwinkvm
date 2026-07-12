#!/usr/bin/env bash
# A/B test: does AWDL (AirDrop/Handoff/Universal Control Wi-Fi co-channel
# duty cycling) cause the periodic ~0.5 s latency spikes on this Mac's Wi-Fi?
#
# AWDL availability windows repeat every 512 TU = 524.288 ms. While awdl0 is
# active the Wi-Fi radio periodically tunes away from the infrastructure
# channel, delaying all traffic (including softkvm UDP motion) in ~20-100 ms
# bursts locked to that period.
#
# Phase A: ping the gateway for 60 s with awdl0 as-is.
# Phase B: bring awdl0 down (sudo), ping again for 60 s.
# Phase C: restore awdl0, print verdict.
#
# Needs: sudo (for ifconfig awdl0 down/up), python3.
# Note: AirDrop/AirPlay/Handoff/Universal Control are unavailable during
# phase B and the system may re-arm awdl0 on its own; the script restores it
# regardless on exit.

set -euo pipefail

SECONDS_PER_PHASE="${1:-60}"
GATEWAY="${2:-$(route -n get default 2>/dev/null | awk '/gateway/ {print $2}')}"
WORK="$(mktemp -d /tmp/softkvm-awdl-ab.XXXXXX)"
trap 'sudo ifconfig awdl0 up 2>/dev/null || true' EXIT

if [[ -z "$GATEWAY" ]]; then
  echo "no default gateway found; pass one: $0 60 192.168.1.1" >&2
  exit 2
fi

count=$((SECONDS_PER_PHASE * 10))

echo "== softkvm AWDL A/B test =="
echo "gateway=$GATEWAY seconds_per_phase=$SECONDS_PER_PHASE work=$WORK"
echo
echo "awdl0 before:"
ifconfig awdl0 | sed -n '1p;/status/p'
echo

echo "-- phase A: awdl0 as-is, pinging $GATEWAY for ${SECONDS_PER_PHASE}s --"
ping -i 0.1 -c "$count" "$GATEWAY" > "$WORK/a.txt" || true

echo "-- phase B: awdl0 down (sudo), pinging again --"
sudo ifconfig awdl0 down
sleep 2
ifconfig awdl0 | sed -n '1p;/status/p'
ping -i 0.1 -c "$count" "$GATEWAY" > "$WORK/b.txt" || true

echo "-- phase C: restoring awdl0 --"
sudo ifconfig awdl0 up
sleep 1
ifconfig awdl0 | sed -n '1p;/status/p'
echo

python3 - "$WORK/a.txt" "$WORK/b.txt" <<'EOF'
import re, sys

def load(path):
    rtts = []
    for line in open(path):
        m = re.search(r'icmp_seq=(\d+).*time=([\d.]+)', line)
        if m:
            rtts.append((int(m.group(1)) * 0.1, float(m.group(2))))
    return rtts

def stats(rtts, label):
    vals = sorted(v for _, v in rtts)
    n = len(vals)
    if n == 0:
        print(f"{label}: no replies"); return None
    pct = lambda p: vals[int((n - 1) * p)]
    spikes = [(t, v) for t, v in rtts if v >= 20.0]
    bursts = []
    prev = -10.0
    for t, v in spikes:
        if t - prev > 0.25:
            bursts.append(t)
        prev = t
    gaps = [b - a for a, b in zip(bursts, bursts[1:])]
    period = 0.524288
    near = sum(1 for g in gaps if g < 6 and abs(g / period - round(g / period)) < 0.15)
    considered = sum(1 for g in gaps if g < 6)
    print(f"{label}: n={n} p50={pct(.5):.1f}ms p95={pct(.95):.1f}ms p99={pct(.99):.1f}ms "
          f"max={vals[-1]:.1f}ms spikes>=20ms={len(spikes)} ({100*len(spikes)/n:.1f}%)")
    if considered:
        print(f"  spike periodicity: {near}/{considered} inter-spike gaps are multiples of 524ms "
              f"({100*near/considered:.0f}%)")
    return len(spikes) / n, (near / considered if considered else 0.0)

a = stats(load(sys.argv[1]), "phase A (awdl0 up) ")
b = stats(load(sys.argv[2]), "phase B (awdl0 down)")
print()
if a and b:
    a_rate, a_period = a
    b_rate, _ = b
    if a_rate >= 0.02 and a_period >= 0.6 and b_rate < a_rate / 4:
        print("VERDICT: AWDL CONFIRMED as the periodic-spike source.")
        print("Fix options (pick one):")
        print("  - System Settings > General > AirDrop & Handoff: AirDrop=No One, Handoff=off,")
        print("    disable 'Cursor and keyboard' (Universal Control), AirPlay Receiver=off")
        print("  - or keep 'sudo ifconfig awdl0 down' while using softkvm (re-arms itself)")
        print("  - or move the softkvm link to Ethernet on the Mac")
    elif a_rate >= 0.02 and b_rate >= a_rate / 2:
        print("VERDICT: spikes persist with awdl0 down -> NOT (only) AWDL.")
        print("Next: run the traced softkvm session (docs/freeze-tracing-guide.md) and check")
        print("windows-side dumps; also test Ethernet on the Mac to rule the radio out.")
    else:
        print("VERDICT: inconclusive (few spikes in phase A). Re-run while actively moving")
        print("the mouse over softkvm, or increase phase duration.")
EOF

echo
echo "raw pings kept in $WORK"
