# Linux host (Wayland/Hyprland): threading model and traps

The Linux host mirrors the Windows one: physical input is captured locally,
motion goes out as binary UDP, everything else as length-prefixed JSON over
TCP, and the Mac client injects through CGEvent. What differs is the capture
layer — evdev instead of Raw Input, Hyprland IPC instead of Win32 cursor
calls — and that layer is where the host's own freeze lived.

## Threads

| Thread | What runs there | Why not on the runtime |
| --- | --- | --- |
| `softkvm-evdev-scan` | discovery of `/dev/input/event*`, `Device::open`, classification | `open()` on an input node is a slow, uninterruptible syscall (below) |
| `softkvm-dispatch` | edge/hotkey decisions, Hyprland IPC, forwarding | every edge decision is a blocking unix-socket roundtrip |
| `softkvm-clipboard` | `wl-paste --watch` | blocking child process |
| `softkvm-runtime` ×2 | TCP writer/reader, per-device evdev readers, UDP motion | — |

The two runtime workers carry the actual motion path, so anything that parks
one of them for longer than a frame is visible on the Mac as a cursor stall.

## Why discovery looks the way it does

Device discovery never calls `evdev::enumerate()`, and the rule is: **open a
node at most once per process lifetime, and never from a thread that forwards
input.** `enumerate()` opens every node under `/dev/input` before the caller
can filter anything, and a full sweep of this machine's 27 nodes costs 300-500
ms of uninterruptible time, because the audio jack-detect and HDMI-audio nodes
wake their codec on open. Running that on a timer inside the runtime is what
froze the Mac cursor every 5 seconds — the whole story, with the measurements
and how to repeat them, is in `docs/linux-freeze-resolved.md`.

So `device_scan_thread` lists the directory (dirents only, nothing opened),
opens only paths it has not judged before, caches the verdict, and re-judges a
path only when it vanishes from the directory or its reader task ends. Open
*failures* are not cached — a node can appear before udev has applied its
permissions.

Opening a node that another process has grabbed is fine; grabbing it is not,
which is why `EVIOCGRAB` failures with `EBUSY` are logged and ignored (a
remapper like hk-translator owns the physical keyboard, and its virtual device
is what we capture instead).

## Hyprland IPC is cheap, but it moved anyway

Measured over 300 `cursorpos` roundtrips on the real socket: p50 25 µs, p99
145 µs, max 469 µs. That is not a freeze source. It is still blocking
unix-socket I/O on every pointer frame during edge arming, so dispatch runs on
its own thread rather than parking a runtime worker for it.

## The other freeze, on the Mac side: AWDL

The `~0.5 s` freeze investigated during the Windows era was never a softkvm
bug: the Mac's Wi-Fi radio tunes away from the infrastructure channel every
512 TU = 524.288 ms to service AWDL (AirDrop / Handoff / Universal Control).
See `docs/freeze-tracing-guide.md` for the evidence and the A/B script, and
`docs/linux-freeze-resolved.md` for why the 5 s freeze was not this.

So `awdl0` is up but idle right now — nothing is driving an availability
window. If the periodic-on-a-half-second stall ever comes back, that is the
first thing to check (`scripts/mac-awdl-ab-test.sh`), and no host-side change
will fix it.

## The Accessibility trap on the Mac client

macOS binds the Accessibility grant to the binary's code signature. The client
is signed ad-hoc, so **every `cargo build` invalidates it**, and the failure is
silent: `CGEventPost` returns normally and nothing moves — indistinguishable
from a dead network path.

The client now calls `AXIsProcessTrusted()` at startup and says so:

```text
ERROR macOS Accessibility permission MISSING: injected events will be silently
      ignored and the cursor will not move.
```

After rebuilding the Mac client: System Settings → Privacy & Security →
Accessibility → remove the old `softkvm` entry, `+`, pick the rebuilt binary,
then `launchctl kickstart -k gui/$(id -u)/com.softkvm.client` (TCC state is
cached per process, so the restart is required).

A stable self-signed certificate would survive rebuilds, but importing and
trusting one needs an interactive session — `security import` and
`add-trusted-cert` both fail over ssh with "User interaction is not allowed".

## Running as a service

`systemd/softkvm-host.service` is a user unit:

```bash
cp systemd/softkvm-host.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now softkvm-host
```

It launches through `sg input` on purpose. The per-user systemd manager keeps
the group set it was started with, and with `Linger=yes` it survives logout —
so `usermod -aG input` does not reach it until the next reboot, and the
service would start with no access to `/dev/input` at all. Taking the group
explicitly makes it work now and after a reboot alike. The symptom, if this is
ever removed, is a log full of:

```text
WARN cannot open evdev node path=/dev/input/event20 err=PermissionDenied
WARN no readable evdev keyboard/pointer devices
```
