# billymacwinkvm

Low-latency local software KVM experiment for Windows/Linux host -> macOS client.

Primary goal:

- Windows or Linux machine on the right is the main host.
- Mac display is on the left.
- Mouse/keyboard should cross the screen edge.
- `Ctrl+Alt+\` is the emergency/toggle hotkey.
- While controlling macOS, Windows `Alt` maps to macOS `Command`, and Windows `Win/Super` maps to macOS `Option`.
- macOS injection uses `cg-event`; the old external virtual-HID path was removed after it added visible activation lag.
- Shared clipboard (text + images) syncs both ways, and each side shows a tray/menu-bar icon (Linux host runs headless).

## Quick start (real two-machine setup)

The Mac is the **client** (receives control); the Windows PC or Linux box is
the **host** (has the physical mouse/keyboard). Both machines must run the
same kit version (v0019+ for clipboard). Start order does not matter — the
host auto-connects and keeps retrying.

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

Every later rebuild of this binary revokes that permission again, silently —
when the Mac stops responding, see
[The Mac stopped responding?](#the-mac-stopped-responding-accessibility-fell-off).

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

### 2-alt. Linux (host, Wayland/Hyprland)

The Linux host captures input straight from evdev and tracks the cursor
through Hyprland IPC (`.socket.sock`: `cursorpos`, `j/monitors`,
`dispatch movecursor`). It needs:

- membership in the `input` group: `sudo usermod -aG input $USER`, then log
  out and back in (reading/grabbing `/dev/input/event*` is required);
- `wl-clipboard` (`wl-copy`/`wl-paste`) for clipboard sync;
- a Hyprland session for edge activation and cursor restore (without the IPC
  socket only the `Ctrl+Alt+\` hotkey works).

Then:

```bash
./scripts/linux-host.sh 192.168.1.11:49321
# or directly:
RUST_LOG=softkvm=info ./target/release/softkvm host --peer 192.168.1.11:49321 --layout mac-left
```

To keep it running across reboots (see `docs/linux-host.md` for why the unit
goes through `sg input`):

```bash
cp systemd/softkvm-host.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now softkvm-host
journalctl --user -u softkvm-host -f
```

There is no tray icon on Linux; status goes to the log. Behavior notes:

- While the Mac is controlled, the captured devices are grabbed
  (`EVIOCGRAB`), so the local cursor stays parked at the left screen edge.
  When control returns, the cursor is warped back via `movecursor`.
- Keyboards already grabbed by another remapper (e.g. an hk-translator-style
  evdev/uinput tool) are not grabbed again — their events arrive through the
  remapper's virtual devices, which softkvm grabs instead.
- The `Ctrl+Alt+\` hotkey pressed while control is **local** cannot be
  swallowed (devices are only grabbed during remote control), so the focused
  window may also see the keypress.

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
| `SOFTKVM_MAC_SCROLL_MODE=line` | post line-unit wheel events instead of pixel units (default `pixel`: Qt apps such as Telegram ignore synthetic line-unit scrolling) |
| `SOFTKVM_MAC_SCROLL_PIXELS=32` | pixels per wheel notch in pixel mode |

### Windows won't connect? (`os error 10048 / AddrInUse`)

That is a local ephemeral-port problem, not the Mac refusing. The host works
around it automatically (explicit local bind), and you can diagnose the
OS-level cause with:

```powershell
.\softkvm.exe win-port-doctor --peer 192.168.1.11:49321
```

### The Mac stopped responding? (Accessibility fell off)

Control crosses the edge, the host log looks perfectly healthy, and nothing
moves on the Mac. Two causes, and the log says which:

```bash
tail -20 /tmp/softkvm-client.log
```

**`Accessibility permission MISSING`** — macOS ties the grant to the binary's
code signature and the client is signed ad-hoc, so **every rebuild of the Mac
client silently revokes it**. `CGEventPost` keeps returning success while
nothing moves. Toggling the existing switch off and on is not enough: the
entry still points at the old signature, it has to be removed and re-added.

On the Mac itself (this needs the physical machine — the panel cannot be
driven over ssh):

1.  Open **System Settings → Privacy & Security → Accessibility**.
2.  Select the old **softkvm** row and press **−** to remove it. Authenticate
    with Touch ID or your password when asked.
3.  Press **+**. In the file picker press **⇧⌘G**, paste
    `/Users/billy/billymacwinkvm/target/release/` and pick **softkvm**.
4.  Make sure the new row's toggle is **on**.

Then restart the client so it re-reads its permissions — TCC state is cached
per process, so this step is not optional. Over ssh is fine:

```bash
launchctl kickstart -k gui/$(id -u)/com.softkvm.client
tail -5 /tmp/softkvm-client.log     # expect: "macOS Accessibility permission present"
```

**No log, or no process at all** — the client is not running. It is a
LaunchAgent, so it only exists inside a logged-in desktop session: after a
reboot it stays down until someone logs into the Mac, and ssh alone will not
bring it up. Once a session exists:

```bash
pgrep -fl softkvm || launchctl kickstart -k gui/$(id -u)/com.softkvm.client
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
- `docs/linux-host.md` — threading model, the macOS Accessibility trap,
  running as a service
- `docs/linux-freeze-resolved.md` — the 5 s cursor freeze: measurements, root
  cause, fix, and how to re-run the measurements
- `docs/dev-setup.md`
- `docs/test-plan.md`
- `docs/freeze-tracing-guide.md` — the AWDL diagnosis and the tracing toolkit
