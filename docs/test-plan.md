# Test Plan

The goal is to burn down uncertainty in Parallels before touching the real Windows machine.

## Local Tests

Run on macOS:

```bash
./scripts/test-local.sh
```

This checks:

- Rust formatting.
- macOS build.
- unit tests.
- Windows ARM cross-build through `cargo zigbuild`.

## Parallels Tests

Run on macOS with the Windows 11 VM logged in:

```bash
./scripts/test-parallels.sh
```

This runs two checks:

- `parallels-probe.sh`: Windows VM executes the binary and sends synthetic events to the Mac client.
- `parallels-host-smoke.sh`: Windows VM starts the real `host` command as the interactive Parallels user, keeps it alive briefly, and verifies that the Mac receives `role: "windows-host"` through the configured Mac sink. The default is the real `cg-event` sink.

Important: plain `prlctl exec` runs as `nt authority\system`, which is not an interactive desktop. Raw Input and `RegisterHotKey` can fail there with `This operation requires an interactive window station`. The host smoke uses `prlctl exec --current-user` on purpose.

## Automated Latency Lab

Run the combined local and Parallels latency lab from macOS:

```bash
./scripts/auto-latency-lab.sh
```

The lab defaults to the current low-jitter CGEvent diagnostic path:

- `SOFTKVM_LAB_SINK=cg-event`
- `SOFTKVM_CGEVENT_MOTION_METHOD=event`
- `SOFTKVM_CGEVENT_TAP=annotated-session`
- `SOFTKVM_MAC_MOTION_MODE=coalesced`
- `SOFTKVM_MAC_MOTION_FLUSH_MS=1`
- `SOFTKVM_UDP_SEND_MODE=coalesced`
- `SOFTKVM_LATENCY_LOG=1`
- `SOFTKVM_LATENCY_WARN_MS=1`

That means the Mac pointer can move during local and Parallels motion cases. For a non-moving baseline, run:

```bash
SOFTKVM_LAB_SINK=log ./scripts/auto-latency-lab.sh
```

Useful variants:

```bash
SOFTKVM_SKIP_PARALLELS=1 ./scripts/auto-latency-lab.sh
SOFTKVM_CGEVENT_MOTION_METHOD=warp ./scripts/auto-latency-lab.sh
SOFTKVM_CGEVENT_TAP=hid ./scripts/auto-latency-lab.sh
SOFTKVM_CGEVENT_TAP=session ./scripts/auto-latency-lab.sh
SOFTKVM_MAC_MOTION_MODE=direct ./scripts/auto-latency-lab.sh
SOFTKVM_UDP_SEND_MODE=immediate ./scripts/auto-latency-lab.sh
SOFTKVM_BENCH_HZ=240 SOFTKVM_BENCH_SECONDS=10 ./scripts/auto-latency-lab.sh
```

Reports are written under `target/softkvm-latency-lab/<timestamp>` with:

- `SUMMARY.txt`
- `*.client.log`
- `*.bench.log`
- `*.report.txt`

The newest run is linked from `target/softkvm-latency-lab/latest`.

For a safe command preview without building, starting Parallels, or moving the pointer:

```bash
SOFTKVM_LAB_DRY_RUN=1 ./scripts/auto-latency-lab.sh
```

`parallels-host-smoke.sh` uses the same real-sink defaults and writes to `target/softkvm-parallels-host-smoke/<timestamp>`, with `target/softkvm-parallels-host-smoke/latest` pointing at the newest run. Override it with `SOFTKVM_HOST_SMOKE_SINK=log` for a handshake-only smoke, or preview it safely with:

```bash
SOFTKVM_HOST_SMOKE_DRY_RUN=1 ./scripts/parallels-host-smoke.sh
```

If the `cg-event` client starts but events do not affect macOS, grant Accessibility permission to the terminal or Codex app that launches the script, then rerun the lab.

## Real Machine Minimal Test

Before moving the pointer to macOS, isolate the real Windows input path.

Run these from the Windows kit folder and move the real physical mouse in
continuous circles until each summary prints:

```powershell
.\softkvm.exe win-raw-cadence --seconds 30 --mode raw-only
.\softkvm.exe win-raw-cadence --seconds 30 --mode hooks-passive
.\softkvm.exe win-raw-cadence --seconds 30 --mode hooks-suppress
```

Read the `raw_gap_ms` lines first:

- If `raw-only` already has `p99.9` or `max` gaps above roughly 10-12 ms, the
  freeze is born before networking/macOS: Windows HID, Raw Input scheduling,
  USB, DPC, polling rate, or the current one-by-one `GetRawInputData` path.
- If `raw-only` is clean but `hooks-passive` or `hooks-suppress` is bad, the
  low-level mouse hook/suppression path is the primary suspect.
- If all three are clean, keep looking downstream: UDP/network, Mac scheduling,
  Karabiner, or visible cursor/compositor.

On the Mac, start a log receiver:

```bash
cd /Users/billy/repos/billymacwinkvm
./scripts/mac-log-client.sh
```

That receiver only prints events. It does not move the Mac pointer.

On the real Windows host, first run the preflight:

```powershell
.\scripts\windows-real-preflight.ps1 -Exe .\softkvm.exe -Peer <mac-lan-ip>:49321
```

Expected Mac log:

- `role: "probe"`
- many `MouseMotion` events
- one left click down/up
- `input reset`

Then run the real host:

On the Mac, switch from the log receiver to the real receiver. It defaults to
`cg-event`; use `SOFTKVM_MAC_SINK=karabiner` only for the VirtualHID A/B path:

```bash
cd /Users/billy/repos/billymacwinkvm
./scripts/mac-karabiner-client.sh
```

On Windows:

```powershell
.\scripts\windows-real-preflight.ps1 -Exe .\softkvm.exe -Peer <mac-lan-ip>:49321 -RunHost
```

Expected Mac log:

- `role: "windows-host"`
- process stays alive

Expected Windows log:

- `persistent Windows keyboard hook is ready for Ctrl+(Alt|Win)+\`

Manual input check:

- Press `Ctrl+Alt+\` to toggle remote mode.
- If using a Mac keyboard layout on Windows, also test `Ctrl+Win+\`.
- Or push the pointer into the Windows left edge with `--layout mac-left`.
- Move the mouse and press harmless keys while watching the Mac log receiver.
- On the Windows host log, keyboard forwarding should print `sending remote keyboard hook input`.
- On the Mac log, startup should print `using macOS modifier policy modifier_policy=SwapAltSuper`.

If keyboard events do not arrive on macOS, first check whether the Windows log shows `sending remote keyboard hook input`. If it does, debug the Mac receiver path; if it does not, debug the Windows hook path.

## Latency Log Analysis

When collecting latency logs, enable latency markers on both sides and feed the
combined or per-side log into:

```bash
./scripts/analyze-latency-log.py /path/to/softkvm.log
```

Read the `summary:` block first. It flags one or more likely sources using the
current marker set:

- `Windows send/capture`: slow `windows mouse raw input handling` or `windows udp motion send latency`.
- `network or Mac receive gap`: slow `mac udp motion receive gap`.
- `CGEvent/warp slow call`: slow `cgevent warp motion latency`, `cgevent mouse post latency`, or direct immediate Mac apply latency.
- `no in-process evidence`: the current parsed markers were present but stayed below the analyzer threshold, so look outside these in-process spans.
