#!/usr/bin/env python3
"""Catch threads stuck in uninterruptible sleep (D), and time a /dev/input sweep.

This is the tool that found the 5 s cursor freeze described in
docs/linux-freeze-resolved.md. It leans on /proc/<pid>/task/*/stat because the
state letter there is readable without privilege, while wchan / syscall /
stack all require PTRACE_MODE_READ and come back empty under
kernel.yama.ptrace_scope=1 for a process you did not spawn.

    # thread states of the running host
    python3 scripts/linux-d-state-sample.py $(pgrep -x softkvm) 20

    # what opening every /dev/input node costs on this machine
    python3 scripts/linux-d-state-sample.py --enumerate-cost 12

A healthy host reports zero D hits while idle. Recurring hits with a window of
tens to hundreds of milliseconds mean some thread is parked in the kernel; if
majflt and delayacct_blkio_ticks stay flat across it (both printed below), it
is neither paging nor disk.
"""

import fcntl
import glob
import os
import sys
import time
from collections import defaultdict

EVIOCGNAME = 0x81004506  # _IOC(READ, 'E', 0x06, 256)
INPUT_DIR = "/dev/input"


def read_stat_fields(path):
    """Fields of /proc/.../stat from `state` onward (comm may contain spaces)."""
    with open(path, "rb") as handle:
        raw = handle.read().decode("utf-8", "replace")
    return raw[raw.rindex(")") + 2 :].split()


def process_counters(pid):
    fields = read_stat_fields(f"/proc/{pid}/stat")
    return {
        "minflt": int(fields[7]),
        "majflt": int(fields[9]),
        "blkio_ticks": int(fields[39]),
    }


def sample_threads(pid, seconds):
    hits = defaultdict(int)
    runs = defaultdict(list)
    current = defaultdict(int)
    names = {}
    before = process_counters(pid)

    sweeps = 0
    deadline = time.time() + seconds
    while time.time() < deadline:
        for task in glob.glob(f"/proc/{pid}/task/*"):
            tid = os.path.basename(task)
            try:
                state = read_stat_fields(f"{task}/stat")[0]
                with open(f"{task}/comm") as handle:
                    names[tid] = handle.read().strip()
            except (OSError, ValueError):
                continue
            if state == "D":
                hits[tid] += 1
                current[tid] += 1
            elif current[tid]:
                runs[tid].append(current[tid])
                current[tid] = 0
        sweeps += 1
        time.sleep(0.002)

    for tid, length in current.items():
        if length:
            runs[tid].append(length)

    after = process_counters(pid)
    period_ms = seconds * 1000.0 / max(sweeps, 1)
    print(f"pid={pid} sweeps={sweeps} sample_period≈{period_ms:.2f} ms over {seconds:.0f}s")
    print(f"total D hits: {sum(hits.values())}")
    if hits:
        print(f"{'tid':>8}  {'thread':<18} {'D hits':>7} {'longest D window':>18}")
        for tid, count in sorted(hits.items(), key=lambda kv: -kv[1]):
            longest = max(runs[tid]) * period_ms if runs[tid] else 0.0
            print(f"{tid:>8}  {names.get(tid, '?'):<18} {count:>7} {longest:>15.0f} ms")
    print(
        "majflt +{majflt}  minflt +{minflt}  blkio_wait +{blkio} ms"
        "   (paging and disk both stay flat if the wait is in a driver)".format(
            majflt=after["majflt"] - before["majflt"],
            minflt=after["minflt"] - before["minflt"],
            blkio=(after["blkio_ticks"] - before["blkio_ticks"]) * 10,
        )
    )


def enumerate_cost(rounds):
    """Time what evdev::enumerate() does: open every node, name it, close it."""
    worst = {}
    sweeps = []
    for _ in range(rounds):
        started = time.perf_counter()
        for path in sorted(glob.glob(f"{INPUT_DIR}/event*")):
            node_started = time.perf_counter()
            try:
                fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
            except OSError:
                continue
            name = bytearray(256)
            try:
                fcntl.ioctl(fd, EVIOCGNAME, name)
            except OSError:
                name = bytearray(b"?")
            finally:
                os.close(fd)
            elapsed_ms = (time.perf_counter() - node_started) * 1000.0
            if path not in worst or elapsed_ms > worst[path][0]:
                worst[path] = (elapsed_ms, bytes(name).split(b"\x00")[0].decode("utf-8", "replace"))
        sweeps.append((time.perf_counter() - started) * 1000.0)
        time.sleep(0.25)

    ordered = sorted(sweeps)
    print(f"sweeps={len(sweeps)} nodes={len(worst)}")
    print(
        f"sweep_ms min={ordered[0]:.1f} "
        f"p50={ordered[len(ordered) // 2]:.1f} max={ordered[-1]:.1f}"
    )
    print("\nworst per-node open+ioctl:")
    for path, (elapsed_ms, name) in sorted(worst.items(), key=lambda kv: -kv[1][0])[:12]:
        print(f"  {elapsed_ms:8.2f} ms  {path:22s} {name}")


def main():
    args = sys.argv[1:]
    if args and args[0] == "--enumerate-cost":
        enumerate_cost(int(args[1]) if len(args) > 1 else 12)
        return
    if not args:
        print(__doc__.strip())
        sys.exit(2)
    sample_threads(int(args[0]), float(args[1]) if len(args) > 1 else 15.0)


if __name__ == "__main__":
    main()
