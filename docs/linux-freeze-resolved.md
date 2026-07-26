# Resolved: the Linux host froze the Mac cursor every few seconds

**Verdict.** Device discovery called `evdev::enumerate()` from a Tokio task
every 5 seconds. That call opens *every* node under `/dev/input` before the
caller can filter anything, and on this machine a full sweep costs 386-505 ms
of uninterruptible time, because opening an idle HD-Audio or HDMI-audio node
wakes its codec. The runtime has two workers and they carry the motion path,
so once every 5 seconds one of them was gone for up to 390 ms. Discovery is
now a `read_dir`, each node is opened at most once per process lifetime, and
the loop runs on its own OS thread. Measured on the live host: 524 samples in
uninterruptible sleep in 14 s before, 0 in 20 s after.

Fixed in `dbfec84`; the code is `device_scan_thread` in
`src/platform/linux.rs`.

## Symptom, and what it was not

The Mac cursor hitched every few seconds while the Linux box drove it.
Movement between hitches was smooth and responsive, so it was never a low
update rate — the same distinction that mattered in the Windows-era
investigation.

It was tempting to reuse that investigation's answer. It does not fit:

| | Windows era (`docs/freeze-tracing-guide.md`) | This one |
| --- | --- | --- |
| Period | ~524 ms, locked to the AWDL availability window | ~5 s |
| Where | the Mac's Wi-Fi radio tuning away from the channel | the Linux host process |
| Fix | environmental (AirDrop/Handoff off, or Ethernet) | code |

The period is the tell. AWDL duty-cycles on a 512 TU = 524.288 ms grid;
nothing about it produces a 5 second beat. A 5 second beat, on the other hand,
is exactly the interval a rescan timer was running at.

Re-measured from the Mac on 2026-07-26 to be sure the old cause was not also
present — 600 pings to the gateway, `p50 3.50 / p99 13.31 / max 17.66 ms`, 0 %
loss, 3 spikes ≥ 15 ms in 60 s and no 524 ms structure. The radio was clean
that day; `awdl0` is up but nothing is driving an availability window.

## Why it hid

Nothing in the host's own instrumentation showed it. The latency logs measure
work the host *does* — `linux mouse evdev handling`, `linux udp motion send
latency`, `linux tcp write latency` — and all of them were microseconds. A
thread that is not running does not log slowly; it does not log at all. The
freeze lived in the gap between log lines, which is precisely the shape the
freeze-tracing ring buffer was built for, and also the shape that a thread
state sampler catches for free.

## Measurement 1: what `enumerate()` costs

Open every `/dev/input/event*`, read its name with `EVIOCGNAME`, close it —
what `evdev::enumerate()` does — and time each node. 27 nodes, 12 sweeps:

```text
sweep_ms  min=385.9  p50=437.9  max=505.0

worst per-node open+ioctl:
   35.97 ms  /dev/input/event22   HD-Audio Generic Line Out Front
   35.00 ms  /dev/input/event15   HDA NVidia HDMI/DP,pcm=3
   34.00 ms  /dev/input/event17   HDA NVidia HDMI/DP,pcm=8
   33.99 ms  /dev/input/event20   HD-Audio Generic Front Mic
   32.99 ms  /dev/input/event25   HD-Audio Generic Front Headphone
   29.99 ms  /dev/input/event13   PC Speaker
```

Every expensive node is an audio jack-detect input. Opening one calls into the
codec driver, which is a real bus transaction and is not interruptible — the
NVIDIA HDMI ones go through the same driver stack that the display path uses.
The actual keyboards and mice are cheap; the cost is entirely in nodes softkvm
does not even want.

## Measurement 2: catching it in the live process

`/proc/<pid>/task/*/stat` exposes each thread's state letter, and it is
readable without any special privilege. Sampling every thread of the running
host at ~2 ms and counting `D` (uninterruptible sleep) points at the stall
directly:

```text
before (old binary, 14 s, sample period 2.22 ms)
  total D hits: 524
       tid  thread              D hits   longest D window
   1039129  softkvm-runtime        524             390 ms

after (fixed binary, 20 s)
  total D hits: 0

after (as a service, 60 s, softkvm + Hyprland sampled together)
  episodes >= 15 ms: 0
```

`scripts/linux-d-state-sample.py` is that sampler.

Note on attribution: `/proc/<pid>/task/<tid>/wchan`, `.../syscall` and
`.../stack` all require `PTRACE_MODE_READ`, and with
`kernel.yama.ptrace_scope=1` a process that is not an ancestor cannot read
them — they come back as `0` / empty even for your own uid. The state letter
in `stat` is not gated, which is why the sampler leans on it. `stat`'s
`majflt` and `delayacct_blkio_ticks` are not gated either, and they are enough
to rule out the two most common causes of `D` (see below).

## Why a parked worker reaches the Mac

The runtime is built with `worker_threads(2)`, and those two workers run the
TCP writer and reader, the per-device evdev readers, and the UDP motion sends.
Discovery ran there too. Tokio can steal a queued task off a blocked worker,
but it cannot move the task that worker is *currently executing*, and a worker
sitting in an uninterruptible syscall is also not polling the I/O driver.

The precise path from "a worker is parked" to "the cursor visibly stops" was
not instrumented — the honest statement is that a worker was measurably
uninterruptible for up to 390 ms on a 5 second grid, that this matches the
reported period and duration, and that removing the sweep removed both the
D-state and the complaint.

## The fix

In `device_scan_thread`:

- discovery is a `read_dir` of `/dev/input` — dirents only, nothing is opened;
- each path is opened **at most once** and its verdict is cached (captured /
  ignored). A path is only reconsidered when it disappears from the directory,
  or when its reader task ends — unplug, read error — which the task reports
  back over a channel so a dead device does not stay captured-but-silent until
  restart;
- open failures are *not* cached, because a node can appear before udev has
  applied its permissions; they are retried, and warned about once per path;
- the whole loop lives on its own OS thread, so even the one-time startup
  sweep never touches a runtime worker.

Dispatch moved to its own thread in the same commit. Hyprland IPC is not slow
— 300 `cursorpos` roundtrips measured `p50 25 µs / p99 145 µs / max 469 µs` —
but it is blocking unix-socket I/O on every pointer frame during edge arming,
and there is no reason for it to sit in front of the motion writer.

## What is left

Occasional 80-145 ms uninterruptible windows still show up on both runtime
workers at once, but only while control is actively being handed across, never
at idle (75 s of idle sampling with no journal activity: zero episodes). They
are not disk and not paging — the process reports `delayacct_blkio_ticks = 0`
and `majflt = 0` since start — which leaves a wait inside a driver or on a
kernel mutex. Grabbing ten evdev devices at once on every transition is the
obvious suspect (`input_grab_device` takes `dev->mutex`, and `mutex_lock` in
the kernel sleeps uninterruptibly), but confirming that needs the ptrace-gated
files above, i.e. root, and the transitions themselves measure 0.15 ms
host-side and drew no complaint. Left as an observation, not a fix.

If a hitch ever becomes noticeable *at the moment of crossing*, run a traced
session — `SOFTKVM_TRACE=1` on both machines, `softkvm trace-analyze` — per
`docs/freeze-tracing-guide.md`.

## Reproducing the measurements

```bash
# thread states of the running host, before/after any change
python3 scripts/linux-d-state-sample.py "$(systemctl --user show softkvm-host -p MainPID --value)" 20

# what a full /dev/input sweep costs on this machine
python3 scripts/linux-d-state-sample.py --enumerate-cost 12

# is the Mac's radio doing the 524 ms thing again?
ssh mac 'ping -i 0.1 -c 600 "$(route -n get default | awk "/gateway/ {print \$2}")"'
```

## Also resolved in the same session

**Scroll did nothing in Telegram, worked everywhere else.** Qt applications on
macOS ignore synthetic *line-unit* wheel events; they read the precise
(pixel) deltas. The client now posts pixel-unit scroll events, which both
kinds of app accept — confirmed working in Telegram and elsewhere.
`SOFTKVM_MAC_SCROLL_MODE=line` restores the old behaviour and
`SOFTKVM_MAC_SCROLL_PIXELS` (default 32) tunes the step.

**"The mouse stopped moving on the Mac" after a rebuild.** macOS binds the
Accessibility grant to the binary's code signature, the client is signed
ad-hoc, so every `cargo build` invalidates it — and the failure is silent:
`CGEventPost` returns normally and nothing moves, which is indistinguishable
from a dead network path. The client now calls `AXIsProcessTrusted()` at
startup and logs the problem and the remedy. See `docs/linux-host.md` for the
re-grant procedure and why a stable self-signed certificate cannot be set up
over ssh.
