//! Low-perturbation motion tracing: preallocated lock-free ring buffer,
//! monotonic microsecond stamps, cold-thread freeze detector and binary dumps.
//!
//! Hot-path cost when disabled: one relaxed atomic load and a predicted branch.
//! Hot-path cost when enabled: one fetch_add plus four relaxed stores.
//! No allocation, locking, formatting, or I/O ever happens on a stamping thread.
//!
//! Enable with SOFTKVM_TRACE=1. Dumps land in SOFTKVM_TRACE_DIR (default: the
//! system temp dir + "/softkvm-trace"). A dump is written automatically when
//! the freeze detector sees a stalled stage (SOFTKVM_TRACE_FREEZE_MS, default
//! 250 ms) while remote control is active, keeping ~2 s of context after the
//! stall, and again when an active session ends. Analyze offline with
//! `softkvm trace-analyze <dump...>`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

pub const DUMP_MAGIC: u32 = 0x5254_4b53; // "SKTR" little-endian
pub const DUMP_VERSION: u16 = 1;
pub const EVENT_BYTES: usize = 32;

/// Stage identifiers. Stable numbering: dumps are read offline, do not reuse.
/// Variants are constructed per-platform, so some are dead on each target.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Stage {
    // Windows host stages.
    WinRawIn = 1,
    WinUdpPre = 3,
    WinUdpPost = 4,
    WinHook = 5,
    // macOS client stages.
    MacRecv = 10,
    MacQueued = 11,
    MacWake = 12,
    MacTake = 13,
    MacEvtCreated = 14,
    MacPostPre = 15,
    MacPostPost = 16,
    MacObserved = 17,
    // Session markers and synthetic sources.
    ActiveOn = 30,
    ActiveOff = 31,
    BenchTick = 40,
}

impl Stage {
    pub fn name(value: u8) -> &'static str {
        match value {
            1 => "win_raw_in",
            3 => "win_udp_pre",
            4 => "win_udp_post",
            5 => "win_hook",
            10 => "mac_recv",
            11 => "mac_queued",
            12 => "mac_wake",
            13 => "mac_take",
            14 => "mac_evt_created",
            15 => "mac_post_pre",
            16 => "mac_post_post",
            17 => "mac_observed",
            30 => "active_on",
            31 => "active_off",
            40 => "bench_tick",
            _ => "unknown",
        }
    }
}

/// One ring slot. Fields are written with relaxed atomic stores; a reader can
/// observe a torn slot at the wrap boundary, which the analyzer drops.
#[repr(C)]
struct Slot {
    t_us: AtomicU64,
    seq: AtomicU64,
    ab: AtomicU64, // a (low 32, i32 bits) | b (high 32, i32 bits)
    cs: AtomicU64, // c (low 32) | stage (bits 32..40)
}

impl Slot {
    const fn new() -> Self {
        Self {
            t_us: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            ab: AtomicU64::new(0),
            cs: AtomicU64::new(0),
        }
    }
}

struct Ring {
    slots: Box<[Slot]>,
    mask: u64,
    head: AtomicU64,
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static RING: OnceLock<Ring> = OnceLock::new();
static EPOCH: OnceLock<Instant> = OnceLock::new();
static EPOCH_WALL_MS: AtomicU64 = AtomicU64::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static ACTIVE_SINCE_US: AtomicU64 = AtomicU64::new(0);
/// Last stamp time per stage id (0..64), in trace microseconds.
static LAST_STAGE_US: [AtomicU64; 64] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU64 = AtomicU64::new(0);
    [ZERO; 64]
};
static DUMP_SERIAL: AtomicU64 = AtomicU64::new(0);

/// Role string used in dump filenames ("win-host", "mac-client", "bench").
static ROLE: OnceLock<&'static str> = OnceLock::new();

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Microseconds since trace init. Returns 0 when tracing is disabled.
#[inline]
pub fn now_us() -> u64 {
    match EPOCH.get() {
        Some(epoch) => epoch.elapsed().as_micros() as u64,
        None => 0,
    }
}

/// Initialize tracing if SOFTKVM_TRACE=1. Safe to call more than once.
pub fn init(role: &'static str) {
    let on = std::env::var("SOFTKVM_TRACE")
        .map(|v| matches!(v.as_str(), "1" | "true" | "on" | "yes"))
        .unwrap_or(false);
    if !on {
        return;
    }
    init_forced(role);
}

/// Initialize tracing unconditionally (used by bench/self-test commands).
pub fn init_forced(role: &'static str) {
    let _ = ROLE.set(role);
    let _ = EPOCH.set(Instant::now());
    let wall_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    EPOCH_WALL_MS.store(wall_ms, Ordering::Relaxed);

    let pow = std::env::var("SOFTKVM_TRACE_RING_POW")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(17)
        .clamp(12, 22);
    let capacity = 1_usize << pow;
    let slots = (0..capacity).map(|_| Slot::new()).collect::<Vec<_>>();
    let _ = RING.set(Ring {
        slots: slots.into_boxed_slice(),
        mask: (capacity as u64) - 1,
        head: AtomicU64::new(0),
    });

    ENABLED.store(true, Ordering::Release);
    info!(
        role,
        capacity,
        bytes = capacity * EVENT_BYTES,
        dir = %dump_dir().display(),
        "softkvm trace ring enabled"
    );
}

/// Start the freeze detector thread. Call once after init on live paths.
pub fn start_freeze_detector() {
    if !enabled() {
        return;
    }
    let freeze_ms = std::env::var("SOFTKVM_TRACE_FREEZE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(250)
        .clamp(50, 5000);
    std::thread::Builder::new()
        .name("softkvm-trace-detector".to_owned())
        .spawn(move || freeze_detector_loop(freeze_ms))
        .map(|_| ())
        .unwrap_or_else(|err| warn!(?err, "failed to spawn trace freeze detector"));
}

/// Record one event. Wait-free; safe from any thread including hooks.
#[inline]
pub fn stamp(stage: Stage, seq: u64, a: i32, b: i32, c: u32) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let Some(ring) = RING.get() else { return };
    let t = now_us();
    let index = (ring.head.fetch_add(1, Ordering::Relaxed) & ring.mask) as usize;
    let slot = &ring.slots[index];
    slot.t_us.store(t.max(1), Ordering::Relaxed);
    slot.seq.store(seq, Ordering::Relaxed);
    let ab = (a as u32 as u64) | ((b as u32 as u64) << 32);
    slot.ab.store(ab, Ordering::Relaxed);
    let cs = (c as u64) | ((stage as u8 as u64) << 32);
    slot.cs.store(cs, Ordering::Relaxed);
    LAST_STAGE_US[(stage as u8 & 63) as usize].store(t.max(1), Ordering::Relaxed);
}

/// Flag the remote-control/session state for the freeze detector, and mark the
/// transition in the ring so the analyzer can bound active spans.
pub fn set_active(active: bool) {
    if !enabled() {
        return;
    }
    let was = ACTIVE.swap(active, Ordering::AcqRel);
    if was == active {
        return;
    }
    if active {
        ACTIVE_SINCE_US.store(now_us(), Ordering::Relaxed);
        stamp(Stage::ActiveOn, 0, 0, 0, 0);
    } else {
        stamp(Stage::ActiveOff, 0, 0, 0, 0);
        // End-of-session snapshot if the session lasted long enough to matter.
        let since = ACTIVE_SINCE_US.load(Ordering::Relaxed);
        if now_us().saturating_sub(since) > 5_000_000 {
            dump("session-end");
        }
    }
}

fn dump_dir() -> std::path::PathBuf {
    std::env::var_os("SOFTKVM_TRACE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("softkvm-trace"))
}

/// Snapshot the ring to a binary dump file. Called from cold threads only.
pub fn dump(reason: &str) -> Option<std::path::PathBuf> {
    if !enabled() {
        return None;
    }
    let ring = RING.get()?;
    let serial = DUMP_SERIAL.fetch_add(1, Ordering::Relaxed);
    if serial >= 64 {
        return None; // hard cap per process run
    }

    let capacity = ring.slots.len();
    let mut raw = Vec::with_capacity(capacity * EVENT_BYTES);
    for slot in ring.slots.iter() {
        raw.extend_from_slice(&slot.t_us.load(Ordering::Relaxed).to_le_bytes());
        raw.extend_from_slice(&slot.seq.load(Ordering::Relaxed).to_le_bytes());
        raw.extend_from_slice(&slot.ab.load(Ordering::Relaxed).to_le_bytes());
        raw.extend_from_slice(&slot.cs.load(Ordering::Relaxed).to_le_bytes());
    }

    let dir = dump_dir();
    if let Err(err) = std::fs::create_dir_all(&dir) {
        warn!(?err, dir = %dir.display(), "trace dump dir create failed");
        return None;
    }
    let wall_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let role = ROLE.get().copied().unwrap_or("unknown");
    let safe_reason: String = reason
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let path = dir.join(format!("softkvm-{role}-{wall_ms}-{safe_reason}.sktrace"));

    let mut header = Vec::with_capacity(96);
    header.extend_from_slice(&DUMP_MAGIC.to_le_bytes());
    header.extend_from_slice(&DUMP_VERSION.to_le_bytes());
    header.extend_from_slice(&[role_code(role), 0]);
    header.extend_from_slice(&EPOCH_WALL_MS.load(Ordering::Relaxed).to_le_bytes());
    header.extend_from_slice(&wall_ms.to_le_bytes());
    header.extend_from_slice(&(capacity as u64).to_le_bytes());
    header.extend_from_slice(&ring.head.load(Ordering::Relaxed).to_le_bytes());
    let mut reason_bytes = [0_u8; 32];
    let reason_src = safe_reason.as_bytes();
    let n = reason_src.len().min(32);
    reason_bytes[..n].copy_from_slice(&reason_src[..n]);
    header.extend_from_slice(&reason_bytes);

    let body = [header, raw].concat();
    match std::fs::write(&path, body) {
        Ok(()) => {
            info!(path = %path.display(), reason, "softkvm trace dump written");
            Some(path)
        }
        Err(err) => {
            warn!(?err, path = %path.display(), "trace dump write failed");
            None
        }
    }
}

fn role_code(role: &str) -> u8 {
    match role {
        "win-host" => 0,
        "mac-client" => 1,
        "bench" => 2,
        _ => 255,
    }
}

/// Stages the detector treats as evidence of an alive pipeline, per role.
fn watched_stages(role: &str) -> &'static [u8] {
    match role {
        "win-host" => &[Stage::WinRawIn as u8, Stage::WinUdpPost as u8],
        "mac-client" => &[Stage::MacRecv as u8, Stage::MacPostPost as u8],
        _ => &[Stage::BenchTick as u8, Stage::MacPostPost as u8],
    }
}

/// Detector: while the session is active and a watched stage had traffic in
/// the last second, a watched stage that goes silent for freeze_ms while any
/// other watched stage keeps stamping (stalled stage), or all watched stages
/// going silent together after flow (upstream silence), schedules a dump with
/// ~2 s of post-freeze context. Runs at 20 Hz on a cold thread.
fn freeze_detector_loop(freeze_ms: u64) {
    let role = ROLE.get().copied().unwrap_or("unknown");
    let watched = watched_stages(role);
    let freeze_us = freeze_ms * 1000;
    // (dump due time, reason, stalled stage, last stamp of that stage at fire)
    let mut pending: Option<(u64, String, u8, u64)> = None;
    let mut last_dump_us: Option<u64> = None;

    loop {
        std::thread::sleep(Duration::from_millis(50));
        if !ACTIVE.load(Ordering::Relaxed) {
            pending = None;
            continue;
        }
        let now = now_us();
        let active_since = ACTIVE_SINCE_US.load(Ordering::Relaxed);
        if now.saturating_sub(active_since) < 1_500_000 {
            continue; // let the session settle before judging silence
        }

        if let Some((due_at, reason, stage, stall_start)) = &pending {
            if now < *due_at {
                continue;
            }
            // Distinguish a freeze from the user simply stopping: a real
            // freeze resumes (the user kept moving through it), or another
            // watched stage kept flowing while this one stayed stuck.
            let resumed =
                LAST_STAGE_US[(*stage & 63) as usize].load(Ordering::Relaxed) > *stall_start;
            let other_flowed = watched.iter().any(|&other| {
                other != *stage
                    && LAST_STAGE_US[(other & 63) as usize].load(Ordering::Relaxed) > *stall_start
            });
            if resumed || other_flowed {
                dump(reason);
                last_dump_us = Some(now);
            } else {
                info!(
                    stage = Stage::name(*stage),
                    "trace freeze candidate looked like idle input; dump skipped"
                );
            }
            pending = None;
            continue;
        }
        if last_dump_us.is_some_and(|last| now.saturating_sub(last) < 10_000_000) {
            continue; // rate-limit dumps
        }

        let mut freshest = 0_u64;
        let mut stalled: Option<(u8, u64, u64)> = None;
        for &stage in watched {
            let last = LAST_STAGE_US[(stage & 63) as usize].load(Ordering::Relaxed);
            if last == 0 || last < active_since {
                continue; // stage never flowed this session
            }
            freshest = freshest.max(last);
            let silent_for = now.saturating_sub(last);
            if silent_for > freeze_us && stalled.is_none_or(|(_, worst, _)| silent_for > worst) {
                stalled = Some((stage, silent_for, last));
            }
        }

        let Some((stage, silent_us, stall_start)) = stalled else {
            continue;
        };
        // Only meaningful if the pipeline was flowing recently: either another
        // stage is fresh (stalled stage) or flow existed within the last 3 s
        // (upstream went silent while the user was presumably still moving).
        let flow_recent = now.saturating_sub(freshest) < 3_000_000;
        if !flow_recent {
            continue;
        }
        let reason = format!("freeze-{}-{}ms", Stage::name(stage), silent_us / 1000);
        info!(
            stage = Stage::name(stage),
            silent_ms = silent_us / 1000,
            "trace freeze detector fired; dumping in 2s"
        );
        pending = Some((now + 2_000_000, reason, stage, stall_start));
    }
}

/// Passive listen-only CGEvent tap observer: stamps MacObserved for every
/// mouse-moved/dragged event WindowServer delivers to the session, providing a
/// post-CGEventPost observation point. Enabled with SOFTKVM_TRACE_OBSERVER=1.
#[cfg(target_os = "macos")]
pub mod observer {
    use super::{Stage, stamp};
    use tracing::{info, warn};

    const KCG_SESSION_EVENT_TAP: u32 = 1;
    const KCG_TAIL_APPEND_EVENT_TAP: u32 = 1;
    const KCG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1;
    const MOUSE_MOVED: u64 = 5;
    const LEFT_DRAGGED: u64 = 6;
    const RIGHT_DRAGGED: u64 = 7;
    const OTHER_DRAGGED: u64 = 27;
    const CG_MOUSE_EVENT_DELTA_X: u32 = 4;
    const CG_MOUSE_EVENT_DELTA_Y: u32 = 5;

    type CGEventRef = *mut std::ffi::c_void;
    type CFMachPortRef = *mut std::ffi::c_void;
    type CFRunLoopSourceRef = *mut std::ffi::c_void;
    type CFRunLoopRef = *mut std::ffi::c_void;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            events_of_interest: u64,
            callback: extern "C" fn(
                proxy: *mut std::ffi::c_void,
                event_type: u32,
                event: CGEventRef,
                user_info: *mut std::ffi::c_void,
            ) -> CGEventRef,
            user_info: *mut std::ffi::c_void,
        ) -> CFMachPortRef;
        fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFMachPortCreateRunLoopSource(
            allocator: *const std::ffi::c_void,
            port: CFMachPortRef,
            order: isize,
        ) -> CFRunLoopSourceRef;
        fn CFRunLoopGetCurrent() -> CFRunLoopRef;
        fn CFRunLoopAddSource(
            run_loop: CFRunLoopRef,
            source: CFRunLoopSourceRef,
            mode: *const std::ffi::c_void,
        );
        fn CFRunLoopRun();
        static kCFRunLoopCommonModes: *const std::ffi::c_void;
    }

    extern "C" fn observer_callback(
        _proxy: *mut std::ffi::c_void,
        event_type: u32,
        event: CGEventRef,
        _user_info: *mut std::ffi::c_void,
    ) -> CGEventRef {
        let (dx, dy) = unsafe {
            (
                CGEventGetIntegerValueField(event, CG_MOUSE_EVENT_DELTA_X) as i32,
                CGEventGetIntegerValueField(event, CG_MOUSE_EVENT_DELTA_Y) as i32,
            )
        };
        stamp(Stage::MacObserved, u64::from(event_type), dx, dy, 0);
        event
    }

    pub fn start_if_requested() {
        if !super::enabled() {
            return;
        }
        let requested = std::env::var("SOFTKVM_TRACE_OBSERVER")
            .map(|v| matches!(v.as_str(), "1" | "true" | "on" | "yes"))
            .unwrap_or(false);
        if !requested {
            return;
        }
        std::thread::Builder::new()
            .name("softkvm-trace-observer".to_owned())
            .spawn(run)
            .map(|_| ())
            .unwrap_or_else(|err| warn!(?err, "failed to spawn trace observer"));
    }

    fn run() {
        let mask = (1_u64 << MOUSE_MOVED)
            | (1_u64 << LEFT_DRAGGED)
            | (1_u64 << RIGHT_DRAGGED)
            | (1_u64 << OTHER_DRAGGED);
        let port = unsafe {
            CGEventTapCreate(
                KCG_SESSION_EVENT_TAP,
                KCG_TAIL_APPEND_EVENT_TAP,
                KCG_EVENT_TAP_OPTION_LISTEN_ONLY,
                mask,
                observer_callback,
                std::ptr::null_mut(),
            )
        };
        if port.is_null() {
            warn!(
                "trace observer tap creation failed; grant Input Monitoring/Accessibility or unset SOFTKVM_TRACE_OBSERVER"
            );
            return;
        }
        unsafe {
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
            if source.is_null() {
                warn!("trace observer run loop source creation failed");
                return;
            }
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
            info!("trace observer tap running (listen-only session tap)");
            CFRunLoopRun();
        }
    }
}

/// Decoded event used by the offline analyzer and in-process bench reports.
#[allow(dead_code)] // a/b carry per-stage payloads consumed selectively
#[derive(Clone, Copy, Debug)]
pub struct TraceEvent {
    pub t_us: u64,
    pub seq: u64,
    pub a: i32,
    pub b: i32,
    pub c: u32,
    pub stage: u8,
}

/// Snapshot the live ring into decoded, time-ordered events (bench use).
pub fn snapshot_events() -> Vec<TraceEvent> {
    let Some(ring) = RING.get() else {
        return Vec::new();
    };
    let mut events = Vec::with_capacity(ring.slots.len());
    for slot in ring.slots.iter() {
        let t_us = slot.t_us.load(Ordering::Relaxed);
        if t_us == 0 {
            continue;
        }
        let ab = slot.ab.load(Ordering::Relaxed);
        let cs = slot.cs.load(Ordering::Relaxed);
        events.push(TraceEvent {
            t_us,
            seq: slot.seq.load(Ordering::Relaxed),
            a: ab as u32 as i32,
            b: (ab >> 32) as u32 as i32,
            c: cs as u32,
            stage: ((cs >> 32) & 0xff) as u8,
        });
    }
    events.sort_by_key(|event| event.t_us);
    events
}
