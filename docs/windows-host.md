# Windows Host Plan

The Windows host must be a per-user desktop process, not a Windows service. Services cannot reliably capture and suppress interactive user input in the logged-in desktop session.

## Win32 APIs

- `RegisterRawInputDevices`
- `GetRawInputData`, later `GetRawInputBuffer` for high polling mice
- `RegisterHotKey`
- `GetCursorPos`
- `GetSystemMetrics(SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN)`
- `ClipCursor`
- `SetWindowsHookExW(WH_KEYBOARD_LL, WH_MOUSE_LL)`
- `WM_INPUT`, `WM_HOTKEY`, `WM_DISPLAYCHANGE`, `WM_POWERBROADCAST`

## Input Devices

Register these raw input top-level collections:

- Mouse: usage page `0x01`, usage `0x02`
- Keyboard: usage page `0x01`, usage `0x06`

Flags:

- `RIDEV_INPUTSINK`
- `RIDEV_DEVNOTIFY`

## Current MVP

Implemented in `src/platform/windows.rs`:

- Hidden message-only `HWND`.
- `RegisterRawInputDevices` for mouse and keyboard.
- `GetRawInputData` parsing for relative mouse motion, buttons, wheel, keyboard keys, and modifiers.
- `Ctrl+Alt+\` via `RegisterHotKey`.
- `mac-left` edge activation when the cursor is at the Windows virtual-screen left edge and the mouse moves further left.
- Cursor parking/clipping while remote macOS control is active.
- Low-level mouse/keyboard hooks suppress local Windows input while remote macOS control is active.
- Remote keyboard forwarding from `WH_KEYBOARD_LL`, so the same hook that suppresses Windows input sends key down/up events to macOS.
- Persistent `WH_KEYBOARD_LL` hotkey hook while host-active, so `Ctrl+(Alt|Win)+\` can enter macOS even when `RegisterHotKey` is blocked by Windows/layout quirks.
- Mouse motion batching at 4 ms before serialization; key/button/wheel/state messages flush pending motion first.

This is enough to run real Windows-host capture. The next polish area is high-polling mouse smoothness under real LAN load.

## Target State Machine

```text
HostActive:
  observe raw input
  leave local input alone
  if cursor crosses Windows left edge -> RemoteActive

RemoteActive:
  forward raw input to macOS
  suppress local input with low-level hooks
  park and clip Windows cursor at left edge
  Ctrl+Alt+\ -> return to HostActive
  disconnect/sleep -> release ClipCursor and return HostActive
```

## Hotkey

`Ctrl+Alt+\` is registered as a fallback hotkey with `MOD_CONTROL | MOD_ALT | MOD_NOREPEAT` and `VK_OEM_5` on common US-like layouts.

The persistent low-level keyboard hook detects `Ctrl+(Alt|Win)+\` in both host-active and remote-active states. Do not let the chord leak to Windows apps or the Mac.

## Modifier Mapping

When controlling macOS:

- Default semantic profile: Windows `Super/Win` -> macOS `Command`, Windows `Alt` -> macOS `Option`
- Optional PC physical-order profile via `SOFTKVM_MAC_MODIFIER_POLICY=swap-alt-super`: Windows `Alt` -> macOS `Command`, Windows `Super/Win` -> macOS `Option`
- Windows `Ctrl` -> macOS `Control`
- Windows `Shift` -> macOS `Shift`

Mapping belongs in the normalized event layer before serialization.

## Startup

Use Task Scheduler at user logon for dev builds. Avoid Windows service for the capture process.
