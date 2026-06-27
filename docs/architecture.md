# softkvm Architecture

Goal: local low-latency software KVM for Windows host -> macOS client.

Physical layout:

- macOS display is on the left.
- Windows display is on the right.
- Windows is the primary host with the physical mouse and keyboard.

Hard requirements:

- Edge transfer from Windows left edge into macOS, and macOS right edge back to Windows.
- Toggle/escape hotkey: `Ctrl+Alt+\`.
- Default macOS modifier profile uses PC physical order: Windows `Alt` -> macOS `Command`, Windows `Super/Win` -> macOS `Option`; `SOFTKVM_MAC_MODIFIER_POLICY=native` enables semantic Mac labels.
- The macOS side should use a virtual HID mouse/keyboard so device-tuning tools such as LinearMouse and Scroll Reverser can see it.
- Reliable reconnect after sleep/wake.
- Startup at login.

## Components

```text
Windows physical devices
  -> softkvm host, Win32 Raw Input
  -> split local transport
  -> softkvm macOS client
  -> Karabiner VirtualHID / DriverKit virtual HID
  -> macOS input stack
  -> LinearMouse / Scroll Reverser
```

## Latency Model

The current production transport is reliable TCP with motion coalescing because
it has shown better tail-latency in the local lab than UDP on this Mac. A split
UDP motion plane still exists as an opt-in A/B path:

- Default TCP frame stream: focus enter/leave, key events, mouse button events, scroll, all-up reset, and coalesced mouse motion.
- Optional binary UDP datagrams on the same host/port: mouse motion (`SKM1`, `seq`, `dx`, `dy`) with `SOFTKVM_MOTION_TRANSPORT=udp`.

Old mouse movement is disposable. Key/button/focus state is not disposable.
The first activation motion rides the reliable stream after `HostState` so the
macOS client cannot receive movement before it knows the remote side is active.
Normal movement uses TCP coalescing by default; set `SOFTKVM_MOTION_TRANSPORT=udp`
on Windows to test the disposable UDP path.

Before sending any ordering-sensitive event such as key down/up, mouse button,
wheel, focus enter, or focus leave, keep the reliable event on TCP. If the UDP
A/B path is enabled and movement is dropped, a later motion packet supersedes it;
stateful events do not use UDP.

For 200 Hz displays:

- Drain raw Windows mouse deltas at device rate.
- Accumulate motion in a nonblocking hot path.
- Coalesce movement at a small interval instead of replaying stale bursts.
- Include an absolute logical cursor position or pointer sequence in reliable button events so clicks do not land at stale positions after dropped motion datagrams.

## MVP Order

1. Prove macOS virtual HID device exists and is visible to LinearMouse.
2. Build macOS client against the virtual HID path.
3. Add dev transport and synthetic `probe` sender.
4. Build Windows Raw Input host.
5. Replace dev TCP/JSON mouse motion with a binary UDP motion plane.
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

- Default PC physical-order profile: Windows `Alt` -> macOS `Command`, Windows `Super/Win` -> macOS `Option`
- Optional semantic profile via `SOFTKVM_MAC_MODIFIER_POLICY=native`: Windows `Super/Win` -> macOS `Command`, Windows `Alt` -> macOS `Option`
- Windows `Ctrl` -> macOS `Control`
- Windows `Shift` -> macOS `Shift`
