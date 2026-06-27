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

On the Mac, start a log receiver:

```bash
cd /Users/billy/repos/billymacwinkvm
./scripts/mac-log-client.sh
```

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

```powershell
.\scripts\windows-real-preflight.ps1 -Exe .\softkvm.exe -Peer <mac-lan-ip>:49321 -RunHost
```

Expected Mac log:

- `role: "windows-host"`
- process stays alive

Manual input check:

- Press `Ctrl+Alt+\` to toggle remote mode.
- Or push the pointer into the Windows left edge with `--layout mac-left`.
- Move the mouse and press harmless keys while watching the Mac log receiver.

Current limitation: local suppression hooks are not implemented yet, so Windows apps may still receive input while remote mode is active. Use a harmless empty desktop/window for this first real-machine test.
