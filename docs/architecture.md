# softkvm Architecture

Goal: local low-latency software KVM for Windows host -> macOS client.

Physical layout:

- macOS display is on the left.
- Windows display is on the right.
- Windows is the primary host with the physical mouse and keyboard.

Hard requirements:

- Edge transfer from Windows left edge into macOS, and macOS right edge back to Windows.
- Toggle/escape hotkey: `Ctrl+Alt+\`.
- While controlling macOS, map Windows `Alt` to macOS `Command` and Windows `Super/Win` to macOS `Option`.
- The macOS side should use a virtual HID mouse/keyboard so device-tuning tools such as LinearMouse and Scroll Reverser can see it.
- Reliable reconnect after sleep/wake.
- Startup at login.

## Components

```text
Windows physical devices
  -> softkvm host, Win32 Raw Input
  -> QUIC transport
  -> softkvm macOS client
  -> Karabiner VirtualHID / DriverKit virtual HID
  -> macOS input stack
  -> LinearMouse / Scroll Reverser
```

## Latency Model

The final transport should use QUIC, not a single TCP stream:

- Reliable stream: focus enter/leave, key events, mouse button events, scroll, all-up reset.
- QUIC datagrams: mouse motion, coalesced to display tick rate.

Old mouse movement is disposable. Key/button/focus state is not disposable.
Before sending any ordering-sensitive event such as key down/up, mouse button,
wheel, focus enter, or focus leave, flush the pending coalesced pointer motion or
attach the latest pointer sequence. Otherwise a click can land at a stale cursor
position after motion datagrams are dropped.

For 200 Hz displays:

- Drain raw Windows mouse deltas at device rate.
- Accumulate motion in a nonblocking hot path.
- Flush at 200 Hz or lower if network pressure rises.
- Include an absolute logical cursor position or pointer sequence in reliable button events so clicks do not land at stale positions after dropped motion datagrams.

## MVP Order

1. Prove macOS virtual HID device exists and is visible to LinearMouse.
2. Build macOS client against the virtual HID path.
3. Add dev transport and synthetic `probe` sender.
4. Build Windows Raw Input host.
5. Replace dev TCP/JSON with QUIC protocol v0.
6. Add startup installers and rollback commands.

Do not spend serious time on Windows capture until the macOS virtual HID proof is green.

## Input State

Keep explicit state for:

- pressed keys
- pressed mouse buttons
- left/right modifiers
- pointer sequence
- active focus epoch

On leave, disconnect, heartbeat timeout, sleep, panic, or hotkey escape, emit a
local all-up reset and clear all state. Modifier mapping should be per target,
not baked into raw capture.

For macOS target profile:

- Windows `Alt` -> macOS `Command`
- Windows `Super/Win` -> macOS `Option`
- Windows `Ctrl` -> macOS `Control`
- Windows `Shift` -> macOS `Shift`
