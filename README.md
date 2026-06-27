# billymacwinkvm

Low-latency local software KVM experiment for Windows host -> macOS client.

Primary goal:

- Windows machine on the right is the main host.
- Mac display is on the left.
- Mouse/keyboard should cross the screen edge.
- `Ctrl+Alt+\` is the emergency/toggle hotkey.
- While controlling macOS, Windows `Alt` maps to macOS `Command`, and Windows `Win/Super` maps to macOS `Option`.
- macOS injection should go through Karabiner DriverKit VirtualHID so LinearMouse/Scroll Reverser can see a real virtual device.

## Current Status

The repo currently contains:

- Rust CLI scaffold.
- Protocol-only `client` and `probe` commands.
- Karabiner VirtualHID wire-format encoder.
- Windows Raw Input host MVP with `Ctrl+Alt+\` toggle and `mac-left` edge activation.
- Split transport: TCP for reliable state/keyboard/buttons/wheel, binary UDP `SKM1` datagrams for mouse motion.
- Setup docs under `docs/`.

Still missing for the final "feels native on a 200 Hz monitor" version:

- Real startup installers for macOS launchd and Windows Task Scheduler.

## Mac Check

```bash
./scripts/check-mac-vhid.sh
```

## Dev Commands

```bash
cargo build
cargo test
./scripts/test-local.sh
cargo run -- mac-hid-probe
cargo run -- client --listen 127.0.0.1:49321 --sink log
cargo run -- probe --peer 127.0.0.1:49321
./scripts/parallels-probe.sh
./scripts/parallels-host-smoke.sh
./scripts/test-parallels.sh
./scripts/mac-log-client.sh
./scripts/mac-karabiner-client.sh
```

Real Windows preflight:

```powershell
.\scripts\windows-real-preflight.ps1 -Exe .\softkvm.exe -Peer <mac-lan-ip>:49321
```

## Docs

- `docs/architecture.md`
- `docs/mac-virtual-hid.md`
- `docs/windows-host.md`
- `docs/dev-setup.md`
- `docs/test-plan.md`
