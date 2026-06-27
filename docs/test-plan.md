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
- `parallels-host-smoke.sh`: Windows VM starts the real `host` command as the interactive Parallels user, keeps it alive briefly, and verifies that the Mac receives `role: "windows-host"`.

Important: plain `prlctl exec` runs as `nt authority\system`, which is not an interactive desktop. Raw Input and `RegisterHotKey` can fail there with `This operation requires an interactive window station`. The host smoke uses `prlctl exec --current-user` on purpose.

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

On the Mac, switch from the log receiver to the real Karabiner receiver:

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
