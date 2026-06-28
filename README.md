# billymacwinkvm

Low-latency local software KVM experiment for Windows host -> macOS client.

Primary goal:

- Windows machine on the right is the main host.
- Mac display is on the left.
- Mouse/keyboard should cross the screen edge.
- `Ctrl+Alt+\` is the emergency/toggle hotkey.
- While controlling macOS, Windows `Alt` maps to macOS `Command`, and Windows `Win/Super` maps to macOS `Option`.
- macOS injection uses `cg-event`; the old external virtual-HID path was removed after it added visible activation lag.

## Current Status

The repo currently contains:

- Rust CLI scaffold.
- Protocol-only `client` and `probe` commands.
- Windows Raw Input host MVP with `Ctrl+Alt+\` toggle and `mac-left` edge activation.
- Motion transport defaults to TCP coalescing for better tail-latency; binary UDP `SKM1` remains available with `SOFTKVM_MOTION_TRANSPORT=udp`.
- Native macOS `IOHIDUserDevice` probe/backend scaffold; current unsigned dev build fails without Apple's `com.apple.developer.hid.virtual.device` entitlement.
- Setup docs under `docs/`.

Still missing for the final "feels native on a 200 Hz monitor" version:

- Real startup installers for macOS launchd and Windows Task Scheduler.

## Dev Commands

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

macOS receiver coalesces high-rate mouse deltas before posting input.
Default flush interval is `1ms`; for real-machine tuning:

```bash
SOFTKVM_MAC_MOTION_FLUSH_MS=1 ./scripts/mac-cgevent-client.sh
SOFTKVM_MAC_MOTION_FLUSH_MS=4 ./scripts/mac-cgevent-client.sh
```

## Docs

- `docs/architecture.md`
- `docs/windows-host.md`
- `docs/dev-setup.md`
- `docs/test-plan.md`
