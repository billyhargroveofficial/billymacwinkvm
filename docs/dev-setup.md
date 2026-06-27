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

Check Karabiner VirtualHID presence:

```bash
cargo run -- mac-hid-probe
```

Keep ShareMouse off while testing:

```bash
launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/com.bartelsmedia.ShareMouse.login.plist 2>/dev/null || true
launchctl disable gui/$(id -u)/com.bartelsmedia.ShareMouse.login 2>/dev/null || true
pkill -x ShareMouse 2>/dev/null || true
```

## Windows

Run the future host as a user process:

```powershell
softkvm.exe host --peer <mac-ip>:49321 --layout mac-left
```

Create a firewall rule:

```powershell
New-NetFirewallRule -DisplayName "softkvm" -Direction Outbound -Program "C:\path\to\softkvm.exe" -Action Allow
```

Task Scheduler is preferred for login startup during development. A Windows service is not the right place for input capture.
