# macOS Virtual HID Plan

The ShareMouse failure mode was architectural: synthetic app events are not a real per-device input source, so LinearMouse and Scroll Reverser cannot tune them as a separate mouse.

The target path is Karabiner's DriverKit virtual HID device:

- Upstream: https://github.com/pqrs-org/Karabiner-DriverKit-VirtualHIDDevice
- Pinned proof version: `7.3.0`
- It provides a DriverKit virtual keyboard/mouse device.
- A userspace service/client feeds HID events into the virtual device.
- IPC socket: `/Library/Application Support/org.pqrs/tmp/rootonly/karabiner_virtual_hid_device_service.sock`
- Client protocol version: `6`

The package has already been downloaded and signature-checked here:

```bash
/Users/billy/Documents/Codex/2026-06-27/ye-y/work/Karabiner-DriverKit-VirtualHIDDevice-7.3.0.pkg
```

## Proof Checklist

1. Install Karabiner VirtualHID or Karabiner-Elements.
2. Approve the system extension in macOS System Settings when prompted.
3. Run:

```bash
cargo run -- mac-hid-probe
```

4. Confirm a virtual pointing device appears in macOS and in LinearMouse.
5. Only then wire `softkvm client --sink karabiner`.

## Current Probe

The current code checks common install paths and intentionally refuses to send events until the upstream service protocol is wired.

```bash
cargo run -- client --listen 0.0.0.0:49321 --sink log
cargo run -- probe --peer 127.0.0.1:49321
```

This tests protocol flow only. It does not create a virtual HID device.

## Install Commands

These need an admin password:

```bash
sudo installer -pkg /Users/billy/Documents/Codex/2026-06-27/ye-y/work/Karabiner-DriverKit-VirtualHIDDevice-7.3.0.pkg -target /
/Applications/.Karabiner-VirtualHIDDevice-Manager.app/Contents/MacOS/Karabiner-VirtualHIDDevice-Manager activate
```

If macOS asks to approve a system extension, approve it in System Settings ->
Privacy & Security, then run:

```bash
systemextensionsctl list | grep -i karabiner
cargo run -- mac-hid-probe
```

For a direct daemon smoke test:

```bash
sudo '/Library/Application Support/org.pqrs/Karabiner-DriverKit-VirtualHIDDevice/Applications/Karabiner-VirtualHIDDevice-Daemon.app/Contents/MacOS/Karabiner-VirtualHIDDevice-Daemon'
```

## Risks

- DriverKit/system extension approval requires manual user action.
- A root helper or privileged LaunchDaemon may be required for the virtual HID service client.
- The Karabiner service socket is root-only; daily architecture should split an unprivileged network/UI agent from a small privileged injector helper.
- TCC and system extension state can break after bundle id/signing/path changes.
- If Karabiner protocol changes, pin the upstream version and adapt the client.

## Fallback

CGEvent injection is acceptable only as a temporary debug sink. It is not the final product because it repeats the ShareMouse limitation.
