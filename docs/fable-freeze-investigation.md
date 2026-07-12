# softkvm: periodic macOS cursor freezes from a Windows host

## Purpose of this document

This is a self-contained engineering handoff for investigating a persistent latency defect in a custom Rust software KVM. The repository is:

https://github.com/billyhargroveofficial/billymacwinkvm

Current repository head at the time of this report: `53419ec`.

The Windows test kit under test is `v0015`; its binaries were built from `c0891c1`. The large macOS motion-path redesign landed in `ce7d054`.

We need a root-cause investigation and a concrete implementation plan. Generic advice such as "use Ethernet", "increase polling rate", "disable acceleration", or "raise process priority" is not sufficient. Most obvious variants have already been tried. Every proposed cause must be tied to a falsifiable measurement.

## The actual defect

The physical mouse is connected to the Windows 11 machine. While remote control of the Mac is active, the macOS cursor moves smoothly most of the time, but suffers a very distinct hard pause approximately once every 0.5 seconds. Between pauses, movement feels appropriately responsive and high-rate. The problem is not a generally low update rate and not ordinary constant network latency.

The user describes it as:

- movement is smooth between freezes;
- approximately every 0.5 seconds the cursor visibly stops;
- after the pause it resumes;
- the pause is obvious on a 200 Hz display;
- changing general pointer speed does not remove it;
- switching TCP/UDP and multiple batching strategies did not remove it.

This distinction is critical. A physical mouse cadence around 8 ms may explain baseline 125 Hz sampling, but it does not explain a recurring visible stall. Do not reduce the diagnosis to "the mouse is only reporting at 125 Hz".

There used to be a separate freeze immediately after crossing the Windows left edge onto the Mac. That transition freeze disappeared after removing Karabiner VirtualHID and moving to the current CGEvent path. The remaining problem is the periodic freeze during continued motion.

## Physical topology

- Main host: physical Windows 11 x64 PC.
- Controlled peer: physical Mac on the left.
- Physical mouse and keyboard are attached to Windows.
- Mouse: Logitech G304, configured as 1000 Hz in G Hub, although observed Windows Raw Input cadence is usually around 8 ms.
- Windows display refresh rate: 200 Hz.
- Transport: local Wi-Fi LAN.
- Current Mac LAN address in this setup: `192.168.1.11`.
- softkvm port: TCP and UDP `49321`.
- Intended layout: `mac-left`.
- Edge transition: Windows left edge enters macOS; macOS right edge releases control back to Windows.
- Hotkey: `Ctrl+Alt+\`.
- macOS modifier policy: swap Windows Alt and Super/Win for physical-layout compatibility.
- macOS scrolling is inverted by the receiver.

Parallels must not be proposed as the visual cursor test environment. Its shared/integrated cursor changes edge and capture behavior and produced false results. Protocol handshakes were previously checked there, but all further cursor-quality diagnosis must target the two physical machines.

## Current implementation

### Language and build

- Rust.
- Release builds are used on both machines.
- Tokio is still used for control-plane work.
- The normal macOS cursor motion hot path was moved off Tokio.
- Normal per-motion latency logging is disabled because logging itself can perturb timing.

### Windows input path

Relevant file: `src/platform/windows.rs`.

The Windows process currently does the following:

1. Calls `timeBeginPeriod(1)`.
2. Raises the process to `HIGH_PRIORITY_CLASS`.
3. Raises the main thread and Tokio worker threads to `THREAD_PRIORITY_HIGHEST`.
4. Captures physical input with Raw Input.
5. Uses `GetRawInputBuffer` by default.
6. Keeps `GetRawInputData` as the fallback selected with `SOFTKVM_RAW_INPUT_READER=lparam`.
7. Uses low-level mouse and keyboard hooks for local suppression, hotkey handling, and keyboard routing.
8. Parks/clips the Windows cursor while macOS control is active.
9. Sends keyboard, buttons, host state, and control messages through length-prefixed JSON frames over TCP with `TCP_NODELAY`.
10. Sends mouse motion through small binary UDP datagrams by default.

The UDP motion datagram currently contains a sequence number plus signed `dx` and `dy` values. Default mode is:

```text
SOFTKVM_MOTION_TRANSPORT=udp
SOFTKVM_UDP_SEND_MODE=immediate
```

In immediate mode, the Raw Input path reaches `SharedUdpMotionWriter::send_motion_immediate`, which calls blocking `std::net::UdpSocket::send` for each motion report. There is also a coalesced sender-thread mode, but it did not remove the reported freezes.

Important code areas:

- `run_host`
- Raw Input registration and message handling
- `GetRawInputBuffer` parsing
- `fast_direct_motion_writer`
- `SharedUdpMotionWriter`
- `send_motion_immediate`
- low-level mouse hook suppression
- cursor clipping/parking

### Network/control path

TCP carries session state and non-motion input. UDP carries binary motion on the same destination port. UDP motion is active only while the TCP-controlled remote state is active.

The latest build added retry diagnostics for initial TCP connection failures. Probe tries 8 times at 250 ms intervals; host tries 40 times. These retries concern startup only and are not intended as a cursor-jitter fix.

### macOS motion path

Relevant files: `src/main.rs`, `src/hid.rs`, and `src/platform/macos.rs`.

Karabiner and its VirtualHID driver were removed completely because that implementation caused a large transition stall and reliability problems. The current sink is native macOS CGEvent.

The current default environment is effectively:

```text
sink=cg-event
SOFTKVM_CGEVENT_MOTION_METHOD=event
SOFTKVM_CGEVENT_TAP=session
SOFTKVM_CGEVENT_POINTER_SPEED=1.0
SOFTKVM_MAC_MODIFIER_POLICY=swap-alt-super
SOFTKVM_LATENCY_LOG=0
```

The normal UDP motion path on macOS is intentionally independent of Tokio timers:

```text
Windows Raw Input
  -> immediate binary UDP datagram
  -> blocking native macOS UDP receiver thread
  -> atomic accumulation of dx/dy and latest sequence
  -> std::thread::Thread::unpark()
  -> native CGEvent writer thread
  -> CGEventCreateMouseEvent
  -> CGEventSetIntegerValueField(deltaX/deltaY)
  -> CGEventPost(session tap)
  -> WindowServer cursor
```

Both native macOS threads request `QOS_CLASS_USER_INTERACTIVE`:

- `UDP motion receiver`
- `CGEvent motion writer`

The receiver thread blocks in `recv_from`. On each valid packet it atomically accumulates motion and unparks the writer. The writer atomically swaps out pending `dx/dy`, updates its cached cursor position, creates a mouse-moved/dragged CGEvent, sets delta fields, and posts it.

If CGEvent posting stalls, incoming UDP packets continue accumulating instead of building a replay queue. The next writer iteration applies the summed newest movement. There is no 1 ms Tokio flush timer in the normal CGEvent UDP hot path anymore.

Important code areas:

- `start_native_macos_motion_threads`
- `mac_native_udp_receiver`
- `mac_native_motion_writer`
- `MacMotionShared::queue_motion`
- `MacMotionShared::take_motion`
- `CgEventMotionSink::post_motion`
- `CgEventMotionSink::post_mouse_event`
- `CGEventPost`
- cached cursor-position handling and edge-release checks

The Tokio runtime has two worker threads. On macOS, its workers and the main thread also request user-interactive QoS, but ordinary UDP cursor motion should no longer depend on them.

## Functional status outside the freeze

The project has gone through many fixes. In the latest useful real-machine tests:

- edge switching from Windows to Mac works;
- right-edge release back to Windows works;
- the transition places the cursor at the correct edge and proportional height;
- hotkey switching has worked in both directions after multiple fixes;
- keyboard forwarding works;
- Alt/Super swapping has worked in the recent path;
- scroll inversion is implemented;
- the old large post-switch stall caused by the Karabiner path is gone;
- ordinary movement between periodic freezes feels responsive.

The periodic cursor pause is the primary unresolved behavioral defect.

## Measurements already collected

### Windows Raw Input cadence

The project includes:

```powershell
.\softkvm.exe win-raw-cadence --seconds 30 --mode raw-only
.\softkvm.exe win-raw-cadence --seconds 30 --mode hooks-passive
.\softkvm.exe win-raw-cadence --seconds 30 --mode hooks-suppress
```

Representative G304 raw-only result:

```text
raw_messages=3690 mouse_events=3690
getrawinput_ms mean=0.003 p99=0.007 max=0.072
raw_handler_ms mean=0.006 p99=0.014 max=0.075
raw_gap_ms count=3689 mean=8.012 p50=8.000 p95=9.011 p99=9.139 p99.9=16.011 max=61.573
```

Another run:

```text
raw_messages=3833
raw_gap_ms mean=7.691 p50=7.998 p95=9.993 p99=14.003 p99.9=16.001 max=16.010
```

Hooks-suppress representative result:

```text
hook_events=18801 hook_suppressed=18801
raw_messages=3729
getrawinput_ms mean=0.003 p99=0.004 max=0.027
raw_handler_ms mean=0.005 p99=0.007 max=0.046
mouse_hook_ms mean=0.000 p99.9=0.001 max=0.007
raw_gap_ms mean=7.931 p50=8.000 p95=9.005 p99=9.997 p99.9=15.001 max=16.002
```

Passive hooks also showed sub-millisecond hook execution and did not obviously introduce a regular 500 ms pause in the cadence summary.

Some earlier runs had isolated maxima of approximately 158-195 ms. Those outliers are relevant, but the user-observed defect is a recurring freeze, not merely one isolated maximum in a 30 second run.

### Earlier macOS latency observations

Before the native event-driven redesign:

- the Tokio/coalesced motion task was observed sleeping approximately 42.6 ms even with a local synthetic source;
- individual `CGEventPost` calls were occasionally measured around 8-22 ms;
- switching from the timer-driven path to native threads removed one known scheduling mechanism but did not remove the real-machine periodic freeze.

This makes `CGEventPost`/WindowServer, Windows-side send stalls, UDP delivery gaps, and thread scheduling all still plausible. They need to be separated with correlated evidence.

## Implementations and variants already tried

The following have not solved the periodic freeze:

- ShareMouse, which also felt bad and did not expose a useful virtual device.
- Karabiner VirtualHID; it caused transition stalls, socket/broken-pipe problems, and was removed.
- TCP JSON motion.
- Binary UDP motion.
- Immediate UDP sends.
- Coalesced UDP sender thread.
- 1 ms and 2 ms motion flush intervals in older designs.
- Tokio timer-driven macOS batching.
- Direct and coalesced task variants.
- Native blocking macOS UDP receiver thread.
- Native macOS CGEvent writer thread.
- Atomic accumulation instead of a replay queue.
- `Thread::unpark` instead of a periodic flush timer.
- `QOS_CLASS_USER_INTERACTIVE` on macOS hot threads.
- Windows `timeBeginPeriod(1)`.
- Windows high process/thread priorities.
- Release builds.
- Disabling ordinary hot-path latency logs.
- `CGEvent` event and warp experiments.
- CGEvent tap variants including HID, annotated-session, and session; session was the best local result but not a complete fix.
- Pointer-speed adjustments.
- Mouse acceleration adjustments.
- Raw Input with passive and suppressing hooks.

Do not simply recommend repeating these without explaining what new measurement the repetition would produce.

## New separate Windows startup failure

After one initially successful `v0015` run, Windows Defender began interfering with the unknown executable. The exact Defender action is not yet known: it may have scanned, blocked, quarantined, or restarted something. A later protocol probe then failed on every attempt with:

```text
TCP connect failed
peer=192.168.1.11:49321
attempt=1..8
error=Only one usage of each socket address (protocol/network address/port) is normally permitted.
os error 10048
error_kind=AddrInUse
raw_os_error=Some(10048)
```

PowerShell then reported:

```text
softkvm protocol probe failed for 192.168.1.11:49321 (exit 1)
```

This is `WSAEADDRINUSE`, not `WSAECONNREFUSED`. It happened in an outbound `TcpStream::connect` before a protocol connection reached the Mac.

Treat this as a separate problem unless evidence links it to the 500 ms freezes. Please explain the realistic causes of outbound `connect` returning `10048` on Windows:

- another softkvm process or duplicate invocation;
- accidental explicit local binding in Rust/Tokio/socket setup;
- exhausted or abnormally restricted dynamic TCP port range;
- many stale `TIME_WAIT` connections combined with a tiny dynamic range;
- Winsock/WFP/Defender interception;
- a local endpoint collision;
- another cause supported by Windows networking behavior.

The retry loop does not fix persistent `10048` and may add noise. Provide exact commands to distinguish these cases without rebooting blindly. Include at least `Get-Process`, `Get-NetTCPConnection`, `netstat -ano`, dynamic-port-range inspection, Defender history, and any useful WFP/ETW checks.

## What the next investigation must determine

The central question is: in which exact segment does each visible 500 ms freeze originate?

1. Physical USB/HID report delivery to Windows.
2. Windows Raw Input message delivery.
3. Windows Raw Input parsing or low-level hook interaction.
4. Synchronous Windows UDP `send` or WFP/Defender processing.
5. Wi-Fi packet scheduling/loss/bursting.
6. macOS UDP `recv_from` delivery.
7. macOS receiver-to-writer wakeup.
8. Atomic aggregation logic.
9. CGEvent creation.
10. `CGEventPost` return latency.
11. WindowServer processing after `CGEventPost` returns.
12. Display/cursor presentation.

We need evidence that puts the pause in one of these intervals.

## Required instrumentation design

Please propose a low-perturbation tracing implementation, not per-event synchronous text logging.

The preferred shape is:

- fixed-size preallocated in-memory ring buffers on both machines;
- monotonic high-resolution timestamps;
- no allocation, formatting, locks, or disk I/O in the motion hot path where avoidable;
- sequence numbers propagated end to end;
- automatic freeze detector that preserves approximately two seconds before and after a gap;
- logs flushed only after the capture or after a detected freeze;
- an offline analyzer that correlates stages and prints percentile and worst-gap tables.

Windows timestamps should use QPC or an equivalent monotonic clock at least at:

- Raw Input receipt;
- end of Raw Input parsing;
- before UDP send;
- after UDP send;
- hook entry/exit if relevant.

macOS timestamps should use `mach_continuous_time`, `mach_absolute_time`, or a justified equivalent at least at:

- UDP `recv_from` return;
- atomic queue completion;
- writer wake/loop entry;
- pending-motion take;
- before and after `CGEventCreateMouseEvent`;
- before and after `CGEventPost`;
- optional observation of the injected event through a passive event tap;
- optional cursor-position observation to distinguish post-return WindowServer delay.

Do not compute one-way latency by subtracting unsynchronized Windows and Mac clocks. Either analyze same-machine stage gaps plus sequence continuity, or define a clock-synchronization/calibration method with an explicit error bound.

The instrumentation must answer:

- Does Windows stop receiving Raw Input during the visible freeze?
- Does UDP `send` block or pause?
- Do packet sequence numbers arrive late, burst after a gap, or disappear?
- Does the native writer thread wake late despite timely UDP arrival?
- Does `CGEventPost` block?
- Does `CGEventPost` return quickly while visible cursor presentation pauses?
- Does pending delta accumulate and produce a jump after the freeze?
- Is the approximately 500 ms cadence actually periodic, and what subsystem has the same period?

## Required A/B isolation matrix

Design the smallest useful sequence of real-machine tests. It should include:

1. Real Windows Raw Input -> memory-only sink on Windows.
2. Dedicated native Windows synthetic sender -> UDP -> Mac memory-only receiver.
3. Real Windows Raw Input -> UDP -> Mac memory-only sink, without CGEvent.
4. Local native macOS synthetic source -> the exact same CGEvent writer, without network.
5. Real end-to-end path with CGEvent.
6. CGEvent event method versus `CGWarpMouseCursorPosition`, only if the test preserves comparable semantics.
7. Controlled Defender exclusion for this exact binary versus normal Defender operation, with the security tradeoff stated.
8. Optional Wi-Fi versus Ethernet test only as an isolation step, not as the final answer.
9. Optional G Hub stopped versus running, again only as an isolation step.
10. A run with normal priorities versus the current aggressive `HIGH_PRIORITY_CLASS` / `THREAD_PRIORITY_HIGHEST`, because over-prioritization may itself disturb hooks, networking, DPC scheduling, or system responsiveness.

For every run specify:

- what code path changes;
- what result would confirm a hypothesis;
- what result would reject it;
- how many seconds are needed;
- which artifact should be collected.

## Questions that require code-level review

Please inspect the repository and answer these specifically:

1. Can blocking `std::net::UdpSocket::send` from the Raw Input/message-loop path periodically block because of Winsock, WFP, Defender, or buffer pressure? Should the callback only publish into a lock-free SPSC structure consumed by a dedicated sender thread?
2. Are `HIGH_PRIORITY_CLASS` and `THREAD_PRIORITY_HIGHEST` appropriate here, or can they create starvation or pathological interaction with Windows input/network threads?
3. Is the Raw Input buffered parser correct for alignment, packet walking, and draining all reports without periodically leaving data queued?
4. Can the low-level suppression hook and cursor clipping generate recursive/synthetic events or periodic work that affects Raw Input cadence?
5. Is `Thread::unpark` plus atomic `swap(0)` correct under every race, or can wake-token coalescing leave pending motion parked until a later packet?
6. Should the macOS writer drain in a `while let Some(...)` loop before parking, and would that prevent any real lost-progress state?
7. Can concurrent updates to separate `pending_dx`, `pending_dy`, and `source_seq` atomics produce mismatched snapshots that matter for perceived motion?
8. Can the cached macOS cursor position diverge from WindowServer position and create apparent stalls or correction jumps?
9. Does creating and releasing a new CGEvent for every report cause periodic allocator/WindowServer pressure? Would a different source/event construction strategy be legal and materially better?
10. Does using a null `CGEventSource` or the session tap introduce throttling/coalescing behavior?
11. Is there a supported lower-latency macOS injection API available to an ordinary signed/notarized user-space app without private entitlements? Clearly separate feasible public APIs from entitlement-gated or private mechanisms.
12. Could App Nap, timer coalescing, QoS overrides, power management, WindowServer, or Wi-Fi power saving plausibly produce the observed period despite user-interactive threads?
13. Is there any approximately 500 ms periodic task in this repository or in likely OS/input components that could serialize with the hot path?
14. Is sequence-only UDP sufficient for diagnosis, or should the binary datagram temporarily carry source timestamp, capture batch id, and flags?
15. What exact ETW/WPR and Instruments/System Trace captures would show thread descheduling, DPC/ISR spikes, Defender/WFP work, UDP delay, and WindowServer latency?

## Constraints

- Keep Rust unless a tiny C/Objective-C shim is technically necessary and justified.
- Do not return to Karabiner.
- Do not propose Parallels for cursor-quality validation.
- Do not hand-wave toward a custom macOS driver without explaining DriverKit entitlements, signing, distribution, and whether the API is actually available to third-party developers.
- Do not treat average latency as sufficient; tail latency and periodic stalls are the target.
- Do not add synchronous per-event logging to production motion paths.
- Preserve keyboard, mouse buttons, scroll, edge switching, cursor-height mapping, hotkey switching, and Alt/Super remapping.
- Prefer small falsifiable experiments before another large architecture rewrite.

## Expected answer format

Answer in Russian and provide:

1. A short diagnosis stating what is known, what is not known, and why the 500 ms freeze is different from 125 Hz sampling.
2. A ranked table of root-cause hypotheses with probability, supporting evidence, contradicting evidence, and the fastest discriminating test.
3. A review of the exact Windows and macOS hot paths in the repository.
4. A low-overhead correlated tracing design with concrete Rust data structures and timestamp APIs.
5. A minimal ordered A/B test plan for the two physical machines.
6. Concrete patches or pseudocode for the first instrumentation build.
7. Separate diagnosis and commands for Windows `WSAEADDRINUSE 10048` after Defender interference.
8. A decision tree describing what to change for each possible measurement result.
9. Any relevant official Apple, Microsoft, Rust/Tokio, InputLeap/Barrier/Synergy, or other primary-source references.

Do not answer with a generic checklist. The most valuable output is a falsifiable measurement plan plus code changes that identify the stage responsible for the recurring pause.

## Short prompt to send with this file

Read the attached `fable-freeze-investigation.md` in full and inspect the linked repository at the stated commits. Act as a senior Windows input, macOS CGEvent/WindowServer, low-latency networking, and Rust concurrency engineer. Diagnose the recurring hard cursor freeze that occurs approximately every 0.5 seconds while Windows controls the physical Mac. Do not confuse it with the roughly 8 ms baseline Raw Input cadence. Produce the exact evidence-driven answer requested in the document, including a ranked root-cause analysis, low-perturbation cross-machine tracing design, concrete instrumentation patch, minimal physical-machine A/B matrix, and a separate diagnosis of outbound Winsock error `10048` after Defender interference. Do not recommend Parallels, Karabiner, generic priority tuning, or another protocol rewrite without a falsifiable reason.
