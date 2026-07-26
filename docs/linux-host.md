# Linux host (Wayland/Hyprland): threading model and the 5 s freeze

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

## The freeze: rescanning /dev/input in the hot path

Symptom: the Mac cursor hitched every few seconds while the Linux host drove
it. Movement between hitches was smooth, so it was never a rate problem.

Root cause: device discovery called `evdev::enumerate()` on a Tokio task every
5 seconds. `enumerate()` **opens every node** under `/dev/input` before the
caller can filter anything, and on this machine that is not cheap:

```text
27 nodes, 12 sweeps:  sweep 386 / 438 / 505 ms  (min / p50 / max)
worst single nodes:   ~36 ms  HD-Audio Generic Line Out Front
                      ~35 ms  HDA NVidia HDMI/DP,pcm=3
                      ~30 ms  PC Speaker
```

The expensive nodes are HD-Audio jack-detect and NVIDIA HDMI-audio inputs:
opening one wakes its codec, which is a real bus transaction, uninterruptible.
Sampling every thread of the running host through `/proc/<pid>/task/*/stat`
caught it directly:

```text
before:  524 D-state samples in 14 s, longest uninterruptible window 390 ms
         (all of them on a softkvm-runtime worker, recurring on the 5 s grid)
after:   0 in 20 s; 0 episodes >= 15 ms in 60 s
```

The same pathology, on the same machine, previously froze this user's keyboard
daemons — a periodic full rescan of `/dev/input` from a thread that also
forwards input is simply not viable here.

Fix, in `device_scan_thread`:

- discovery is a `read_dir` of `/dev/input` — dirents only, nothing is opened;
- each path is opened **at most once** and its verdict cached (captured /
  ignored); a path only gets reconsidered when it disappears from the
  directory, or when its reader task ends (unplug, read error), which the task
  reports back over a channel;
- the whole loop lives on its own OS thread, so even the one-time startup
  sweep never touches a runtime worker.

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
See `docs/freeze-tracing-guide.md` for the evidence and the A/B script.

Re-measured from the Mac on 2026-07-26, 600 pings to the gateway:

```text
today:      p50 3.50  p99 13.31  max 17.66 ms, 0 % loss,
            3 spikes >= 15 ms in 60 s, no 524 ms structure
2026-07-13: p50 3.4 ms but 9.9 % of replies at 20-91 ms,
            all inter-spike gaps multiples of ~0.5 s
```

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
