# billymacwinkvm

Low-latency local software KVM experiment for Windows host -> macOS client.

Primary goal:

- Windows machine on the right is the main host.
- Mac display is on the left.
- Mouse/keyboard should cross the screen edge.
- `Ctrl+Alt+\` is the emergency/toggle hotkey.
- While controlling macOS, Windows `Alt` maps to macOS `Command`, and Windows `Win/Super` maps to macOS `Option`.
- macOS injection uses `cg-event`; the old external virtual-HID path was removed after it added visible activation lag.
- Shared clipboard (text + images) syncs both ways, and each side shows a tray/menu-bar icon.

## Quick start (real two-machine setup)

The Mac is the **client** (receives control); the Windows PC is the **host**
(has the physical mouse/keyboard). Both machines must run the same kit version
(v0019+ for clipboard). Start order does not matter — the host auto-connects
and keeps retrying.

Default port is `49321`. In the examples the Mac's LAN IP is `192.168.1.11`;
replace it with your Mac's actual address (`ipconfig getifaddr en0` on the Mac).

### 1. Mac (client)

From the repo root:

```bash
cargo build --release
RUST_LOG=softkvm=info ./target/release/softkvm client --listen 0.0.0.0:49321 --sink cg-event
```

Run it in the background instead (survives closing the terminal):

```bash
RUST_LOG=softkvm=info nohup ./target/release/softkvm client \
  --listen 0.0.0.0:49321 --sink cg-event > /tmp/softkvm-client.log 2>&1 &
```

A `⌘⇄` icon appears in the menu bar (Quit from there). macOS will prompt for
**Accessibility** permission on first run — grant it so mouse/keyboard
injection works. `--listen 0.0.0.0:49321` is required so the Windows host can
reach it over the LAN (not `127.0.0.1`).

### 2. Windows (host)

Unzip the latest kit (`dist/softkvm-windows-test-kit-latest.zip`) and, in
PowerShell from that folder:

```powershell
# confirm it is the current build
.\softkvm.exe build-info

# run the host (peer = the Mac's IP:port)
.\softkvm.exe host --peer 192.168.1.11:49321 --layout mac-left
```

Do **not** launch via the `.ps1` script unless you first run
`Set-ExecutionPolicy -Scope Process Bypass` (or `Unblock-File .\scripts\*.ps1`)
— running `softkvm.exe` directly avoids the execution-policy block entirely.
A softkvm icon appears in the system tray (right-click -> Quit); the tooltip
shows `connected` / `connecting`.

### 3. Use it

- Move the cursor into the **left screen edge** on Windows to cross onto the Mac;
  the **right edge** on the Mac releases control back to Windows.
- `Ctrl+Alt+\` toggles control from either side.
- Copy on one machine, paste on the other — plain text and images (screenshots)
  sync automatically.

### Toggles / fallbacks (env vars, set before launching)

| Variable | Effect |
| --- | --- |
| `SOFTKVM_CLIPBOARD=0` | disable clipboard sync |
| `SOFTKVM_TRAY=0` | hide the tray / menu-bar icon (headless) |
| `SOFTKVM_TRACE=1` | enable freeze tracing (see `docs/freeze-tracing-guide.md`) |
| `SOFTKVM_MOTION_TRANSPORT=tcp` | fall back to TCP/JSON motion instead of UDP |
| `SOFTKVM_RAW_INPUT_READER=lparam` | use `GetRawInputData` instead of the buffered reader |

### Windows won't connect? (`os error 10048 / AddrInUse`)

That is a local ephemeral-port problem, not the Mac refusing. The host works
around it automatically (explicit local bind), and you can diagnose the
OS-level cause with:

```powershell
.\softkvm.exe win-port-doctor --peer 192.168.1.11:49321
```

## Current Status

The repo currently contains:

- Rust CLI scaffold.
- Protocol-only `client` and `probe` commands.
- Windows Raw Input host MVP with `Ctrl+Alt+\` toggle and `mac-left` edge activation.
- Mouse motion defaults to immediate binary UDP `SKM1`; reliable state, keyboard, buttons, wheel, and focus remain on TCP.
- The macOS UDP hot path uses a blocking receive thread plus an event-driven `CGEvent` writer thread, both at user-interactive QoS. There is no Tokio timer in the production mouse path.
- Native macOS `IOHIDUserDevice` probe/backend scaffold; current unsigned dev build fails without Apple's `com.apple.developer.hid.virtual.device` entitlement.
- Setup docs under `docs/`.

Still missing for the final "feels native on a 200 Hz monitor" version:

- Real startup installers for macOS launchd and Windows Task Scheduler.

## Dev Commands

For the real two-machine launch commands see **Quick start** above; the
commands below are for local development and diagnostics.

```bash
cargo build
cargo test
./scripts/test-local.sh
cargo run -- mac-native-hid-probe
cargo run -- client --listen 127.0.0.1:49321 --sink log
cargo run -- probe --peer 127.0.0.1:49321
./scripts/parallels-probe.sh
./scripts/parallels-host-smoke.sh
./scripts/test-parallels.sh
./scripts/mac-log-client.sh
./scripts/mac-cgevent-client.sh
```

Real Windows preflight:

```powershell
.\scripts\windows-real-preflight.ps1 -Exe .\softkvm.exe -Peer <mac-lan-ip>:49321
```

Package a versioned Windows transfer zip:

```bash
./scripts/package-windows-kit.sh
```

Each run bumps `kit-version.txt` and writes `dist/softkvm-windows-test-kit-vNNNN-<git>.zip`
plus `dist/softkvm-windows-test-kit-latest.zip`.

The normal launcher builds and runs the optimized macOS binary. Latency markers
are completely disabled in the hot path unless explicitly requested:

```bash
SOFTKVM_LATENCY_LOG=1 RUST_LOG=softkvm=info,softkvm::latency=info ./scripts/mac-cgevent-client.sh
```

## Docs

- `docs/architecture.md`
- `docs/windows-host.md`
- `docs/dev-setup.md`
- `docs/test-plan.md`
