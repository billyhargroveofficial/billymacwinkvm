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
- The macOS side currently uses CGEvent injection because it had the lowest activation latency in local testing.
- Reliable reconnect after sleep/wake.
- Startup at login.

## Components

```text
Windows physical devices
  -> softkvm host, Win32 Raw Input
  -> immediate UDP motion + reliable TCP control
  -> softkvm macOS client
  -> native receive thread -> thread unpark -> native CGEvent thread
  -> macOS input stack
```

## Latency Model

The production path is split by semantics:

- TCP frame stream: focus enter/leave, key events, mouse button events, scroll, and all-up reset.
- Binary UDP datagrams on the same host/port: disposable mouse motion (`SKM1`, `seq`, `dx`, `dy`).

Old mouse movement is disposable. Key/button/focus state is not disposable.
The first activation motion rides the reliable stream after `HostState` so the
macOS client cannot receive movement before it knows the remote side is active.
Normal movement uses immediate UDP by default. `SOFTKVM_MOTION_TRANSPORT=tcp`
keeps the old coalesced path available as a diagnostic fallback.

Before sending any ordering-sensitive event such as key down/up, mouse button,
wheel, focus enter, or focus leave, keep the reliable event on TCP. If the UDP
A/B path is enabled and movement is dropped, a later motion packet supersedes it;
stateful events do not use UDP.

For 200 Hz displays:

- Drain raw Windows mouse deltas at device rate.
- Accumulate motion in a nonblocking hot path.
- Keep UDP receive independent from `CGEventPost`; if injection stalls, merge pending deltas and apply the newest accumulated movement once.
- Wake the injector from packet arrival instead of a periodic Tokio/macOS timer.
- Include an absolute logical cursor position or pointer sequence in reliable button events so clicks do not land at stale positions after dropped motion datagrams.

## MVP Order

1. Keep the CGEvent macOS receiver stable and measurable.
2. Add dev transport and synthetic `probe` sender.
3. Build Windows Raw Input host.
4. Keep TCP motion as an explicit diagnostic fallback.
5. Add startup installers and rollback commands.

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
