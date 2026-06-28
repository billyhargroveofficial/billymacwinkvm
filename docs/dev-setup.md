# Dev Setup

## macOS

Build:

```bash
cargo build
```

Run a protocol-only client:

```bash
cargo run -- client --listen 0.0.0.0:49321 --sink log
```

Send synthetic input:

```bash
cargo run -- probe --peer 127.0.0.1:49321
```

Run the Windows ARM smoke test through Parallels:

```bash
./scripts/parallels-probe.sh
```

Keep ShareMouse off while testing:

```bash
launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/com.bartelsmedia.ShareMouse.login.plist 2>/dev/null || true
launchctl disable gui/$(id -u)/com.bartelsmedia.ShareMouse.login 2>/dev/null || true
pkill -x ShareMouse 2>/dev/null || true
```

## Windows

Run the host as a user process:

```powershell
softkvm.exe host --peer <mac-ip>:49321 --layout mac-left
```

Create a firewall rule:

```powershell
New-NetFirewallRule -DisplayName "softkvm" -Direction Outbound -Program "C:\path\to\softkvm.exe" -Action Allow
```

Task Scheduler is preferred for login startup during development. A Windows service is not the right place for input capture because the host needs the logged-in desktop session.

Current host MVP:

- Captures mouse and keyboard through Raw Input.
- `Ctrl+Alt+\` toggles remote macOS control.
- With `--layout mac-left`, pushing left at the Windows virtual-screen edge enables remote control.
- Parks/clips the Windows cursor while remote control is active.
- Suppresses local Windows mouse/keyboard input while remote control is active.
- Restores cursor position by monitor-relative height when crossing back.
- Keeps a persistent keyboard hook for `Ctrl+(Alt|Win)+\` entry/exit.
- Flushes mouse motion to macOS every 4 ms instead of sending every raw input packet.

Not implemented yet:

- Wake/reconnect policy.
- QUIC/datagram transport.
