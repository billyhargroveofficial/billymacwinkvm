# Freeze tracing guide (kit v0016+)

Toolkit for pinning each visible ~0.5 s cursor freeze to one pipeline segment:

```
Win Raw Input -> UDP send -> Wi-Fi -> Mac recv -> wakeup -> CGEventPost -> WindowServer
```

## Evidence collected so far (2026-07-13, on the target Mac)

1. **The Mac's Wi-Fi link itself stalls on a 524 ms grid.** 90 s of
   `ping -i 0.1` from the Mac (192.168.1.11) to the gateway over the real
   link: p50 3.4 ms, but 9.9 % of replies spike to 20–91 ms, and **all 88**
   inter-spike gaps under 6 s are multiples of ~0.5 s (70 of them exactly
   one period). 512 TU = **524.288 ms is the AWDL availability-window
   period** — AirDrop/Handoff/Universal Control duty-cycling the radio off
   the infrastructure channel. `awdl0` was UP/active during the test,
   AirDrop discoverability was "Everyone", infra channel 48 (5 GHz) vs AWDL
   social channel 44 — every window is a real re-tune.
2. **The CGEvent writer is clean.** `softkvm mac-cg-bench --seconds 15
   --hz 250 --amp 1` on the same Mac: 3750 real posts (verified to move the
   cursor), p99 8.8 ms, zero 524 ms periodicity, no recurring stalls.
   A/B test #4 of the investigation matrix: **CGEventPost/WindowServer
   exonerated** for the periodic component.
3. **The Mac UDP receive path is clean off-air.** 250 Hz loopback stream for
   12 s through the real client: recv gaps p50 = 4.000 ms, max 4.9 ms,
   send→recv delay variation ≤ 0.7 ms.

Working hypothesis, to be confirmed end-to-end with one traced session:
**packets leave Windows on time and sit in the air/AP queue for 20–100 ms
every ~524 ms while the Mac radio services AWDL.**

## Running a traced session

Windows (kit folder):

```powershell
$env:SOFTKVM_TRACE = "1"
.\softkvm.exe host --peer 192.168.1.11:49321 --layout mac-left
```

Mac:

```bash
SOFTKVM_TRACE=1 ./target/release/softkvm client --listen 0.0.0.0:49321 --sink cg-event
# optional, adds a WindowServer-side observation stage (needs Input Monitoring):
# SOFTKVM_TRACE=1 SOFTKVM_TRACE_OBSERVER=1 ...
```

Move continuously on the Mac side for 1–3 minutes so several freezes occur,
then **flick control back to Windows (right edge or Ctrl+Alt+\\)** — the
active→inactive transition flushes a `session-end` dump on *both* machines.
Freezes ≥ 250 ms additionally auto-dump on the machine that saw them, with
2 s of post-context (`SOFTKVM_TRACE_FREEZE_MS` to tune).

Dumps: macOS `/tmp/softkvm-trace/*.sktrace` (or `$TMPDIR/softkvm-trace`),
Windows `%TEMP%\softkvm-trace\*.sktrace`. Override with `SOFTKVM_TRACE_DIR`.

Tracing switches the motion datagram from SKM1 (24 B) to SKM2 (32 B, adds a
sender monotonic stamp); both peers must run v0016+ while tracing. With
tracing off the wire format is byte-identical to v0015.

Overhead: stamps are wait-free writes into a preallocated 4 MB ring (one
relaxed load + fetch_add + 4 relaxed stores; nothing allocates, locks, or
touches disk on hot threads). Disabled, it is one predicted branch.

## Analyzing

```bash
# copy the Windows dump over, then:
softkvm trace-analyze mac.sktrace win.sktrace --stall-ms 100
```

Per dump: per-stage gap percentiles, worst stalls, stall periodicity with a
524.288 ms fold, SKM2 send→recv delay variation (clock offset cancels in
differences; drift over the 2 s window is negligible). With both dumps it
joins every mac_recv freeze to the same sequence range on the Windows side
and prints one of:

- `sender kept sending -> network/receive side`
- `sender paused too -> windows/input side`
- `sender sent nothing -> windows/input side`

## Decision tree

| Measurement result | Conclusion | Next change |
|---|---|---|
| `mac_recv` stalls ~N×524 ms, cross-join says *sender kept sending*, delay-variation spikes match | AWDL tune-away (or other radio duty-cycling) | Disable AWDL users on the Mac: AirDrop → No One, Handoff off, Universal Control ("Cursor and keyboard") off, AirPlay Receiver off; or `sudo ifconfig awdl0 down` during sessions; or Mac on Ethernet. Re-run `scripts/mac-awdl-ab-test.sh` for the ping-level A/B. No softkvm code change fixes this; optional cosmetic: cap post-gap catch-up jumps. |
| `mac_recv` stalls, *sender kept sending*, but **no** 524 ms structure | Other network delay (AP queueing, interference, powersave) | Test Ethernet on the Mac; check AP settings (DTIM, airtime fairness); `wdutil` / sniffer on channel 48. |
| `win_raw_in` gaps match the freezes | Input stops reaching the host process | Run `win-raw-cadence` in a *second* console during a session (independent process): if it also gaps → USB/HID/G HUB/driver level (try G HUB off, another USB port, 2.4 GHz dongle vs Bluetooth); if it does not gap → the host message loop is stalling: capture ETW (`wpr -start GeneralProfile`) and look at the softkvm thread. |
| `win_raw_in` flows, `win_udp_pre→win_udp_post` occasionally ≥ tens of ms | Blocking UDP `send` stalls the pump (WFP/Defender/driver backpressure) | Move sends off the Raw Input thread: publish into the existing atomic accumulator (`SOFTKVM_UDP_SEND_MODE=coalesced` already ships) and A/B Defender exclusion for the kit folder; if exclusion fixes it, keep exclusion and file the WFP latency with the AV vendor. |
| `mac_recv` fine, `mac_recv→mac_take` large | Writer wake-up latency (scheduler/QoS) | Verify QoS applied (log line at start); test `SOFTKVM_TRACE` run with writer pinned: escalate thread to `thread_time_constraint_policy` (code change), or temporarily poll at 1 ms instead of park to compare. |
| `mac_take` fine, `mac_post_pre→mac_post_post` large | `CGEventPost` blocks | Compare `SOFTKVM_CGEVENT_TAP=hid` vs `session`; check WindowServer CPU; try `SOFTKVM_CGEVENT_MOTION_METHOD=warp` as an isolation (loses buttons-drag semantics only if pressed) — and keep the bench numbers as the healthy baseline. |
| `mac_post_post` fine, `mac_observed` (observer tap) delayed/bursty | WindowServer queues after post returns | Instruments "System Trace" on WindowServer during a session; compare `hid` tap; check display sleep/ProMotion settings of the target display. |
| Freezes vanish whenever tracing is on | Wire-format/observer effect (unlikely: SKM2 is +8 B) | Re-run with `SOFTKVM_TRACE=1` on one side only; fall back to ping evidence. |

## The separate Windows 10048 startup failure

`softkvm probe` failing with `os error 10048 (AddrInUse)` on an **outbound**
connect is a local ephemeral-port allocation failure, not the Mac port being
busy (that would be 10061/10060) and not related to the 0.5 s freezes.
Run `scripts/diagnose-addrinuse.ps1` (checks duplicate processes, TIME_WAIT
floods, `netsh dynamicport`, Hyper-V/WSL `excludedportrange`, a 100-socket
allocation self-test, Defender detections, and optional WFP state). The
in-app retry loop now logs a targeted hint on the first AddrInUse.

## References

- AWDL protocol & 512 TU availability windows: "One Billion Apples' Secret
  Sauce: Recipe for the Apple Wireless Direct Link Ad hoc Protocol"
  (Stute et al., MobiCom 2018) — the OWL project's protocol analysis.
- Apple: `CGEventPost`, `CGEventTapCreate`, `CGEventSourceSetLocalEventsSuppressionInterval`
  (Core Graphics, Quartz Event Services documentation).
- Microsoft: `GetRawInputBuffer` / `RegisterRawInputDevices` (multiple
  processes may register independently — basis of the second-console
  cadence check), `WSAEADDRINUSE` (Windows Sockets error codes), `netsh int
  ipv4 show excludedportrange` + WinNAT port reservations.
- InputLeap/Barrier ship the same CGEvent injection (`CGEventCreateMouseEvent`
  + `CGEventPost` with delta fields) — no lower-latency public injection API
  exists short of a DriverKit HID driver, which requires the
  `com.apple.developer.driverkit.family.hid.device` entitlement Apple grants
  per-request; not worth it while the network is the bottleneck.
