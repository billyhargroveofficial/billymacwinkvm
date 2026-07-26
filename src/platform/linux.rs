//! Linux host capture (Wayland/Hyprland): reads physical input straight from
//! evdev, tracks the cursor through Hyprland IPC, and feeds the same wire
//! protocol as the Windows host. While remote control is active the captured
//! devices are grabbed (EVIOCGRAB) so the compositor stops seeing them; the
//! cursor stays parked at the left screen edge until control returns.
//!
//! Keyboard note: remappers like hk-translator grab the physical keyboards
//! and re-emit through their own uinput devices. Those virtual devices are
//! what the compositor sees, so they are what we read and grab; physical
//! keyboards that fail EVIOCGRAB with EBUSY are harmless because their events
//! already flow through the virtual ones.

use anyhow::{Context, Result, anyhow, bail};
use evdev::{Device, EventType, Key, RelativeAxisType};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::UdpSocket as StdUdpSocket;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::unix::AsyncFd;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, watch};
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::clipboard as shared_clipboard;
use crate::latency;
use crate::protocol::{
    self, ClientControlEvent, ClipboardEvent, Frame, HostStateEvent, InputEvent, KeyCode, KeyState,
    Message, Modifier, MotionDatagram, MouseButton, ProtocolHello,
};
use crate::transport::FrameWriter;

const EDGE_TRIGGER_PX: f64 = 8.0;
const EDGE_REARM_PX: f64 = 64.0;
const LINUX_RESTORE_EDGE_INSET_PX: f64 = 32.0;
const HOTKEY_DEBOUNCE_MS: u64 = 250;
const MOUSE_FLUSH_INTERVAL_MS: u64 = 4;
const MAX_MOTION_DRAIN_PER_TURN: usize = 64;
const MAX_MOTION_DELTA_PER_FLUSH: i32 = 512;
const MAC_ENTRY_X_RATIO_FROM_LINUX: f64 = 1.0;
const DEVICE_RESCAN_INTERVAL: Duration = Duration::from_secs(5);
const DEVICE_DIR: &str = "/dev/input";
const MONITOR_CACHE_TTL: Duration = Duration::from_secs(30);
const HYPR_IPC_TIMEOUT: Duration = Duration::from_millis(250);
const HYPR_RESOLVE_RETRY: Duration = Duration::from_secs(5);

static HOST_STATE: OnceLock<Mutex<HostState>> = OnceLock::new();
static GRAB_SIGNAL: OnceLock<watch::Sender<bool>> = OnceLock::new();
static REMOTE_ACTIVE_FAST: AtomicBool = AtomicBool::new(false);

pub async fn run_host(
    peer: String,
    layout: String,
    activate_on_start: bool,
    entry_x_ratio: f64,
    entry_y_ratio: f64,
    no_local_capture: bool,
) -> Result<()> {
    if layout != "mac-left" {
        bail!("only --layout mac-left is implemented right now");
    }
    let entry_x_ratio = entry_x_ratio.clamp(0.02, 0.98);
    let entry_y_ratio = entry_y_ratio.clamp(0.02, 0.98);
    let local_capture = !no_local_capture;

    crate::trace::init("linux-host");
    crate::trace::start_freeze_detector();

    let motion_transport = MotionTransport::connect(&peer).await;
    let direct_motion = motion_transport.direct_writer();
    let (tx, rx) = mpsc::unbounded_channel();
    let (grab_tx, grab_rx) = watch::channel(false);
    GRAB_SIGNAL
        .set(grab_tx)
        .map_err(|_| anyhow!("linux grab signal was already initialized"))?;
    HOST_STATE
        .set(Mutex::new(HostState::new(tx, direct_motion, local_capture)))
        .map_err(|_| anyhow!("linux host state was already initialized"))?;

    tokio::spawn(connection_supervisor(peer.clone(), rx, motion_transport));

    let (capture_tx, capture_rx) = mpsc::unbounded_channel();
    let runtime = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("softkvm-evdev-scan".to_owned())
        .spawn(move || device_scan_thread(runtime, capture_tx, grab_rx))
        .context("spawn evdev scan thread")?;
    std::thread::Builder::new()
        .name("softkvm-dispatch".to_owned())
        .spawn(move || dispatcher_thread(capture_rx))
        .context("spawn capture dispatch thread")?;

    if shared_clipboard::enabled() {
        std::thread::Builder::new()
            .name("softkvm-clipboard".to_owned())
            .spawn(clipboard_watcher_thread)
            .context("spawn clipboard watcher thread")?;
        info!("clipboard sync enabled");
    }

    info!(%peer, %layout, activate_on_start, entry_x_ratio, entry_y_ratio, local_capture, "starting Linux host capture (auto-connect)");

    if activate_on_start
        && let Ok(mut state) = lock_state()
        && let Err(err) = state.set_remote_active(
            true,
            "activate-on-start",
            Some(entry_x_ratio),
            Some(entry_y_ratio),
        )
    {
        warn!(?err, "failed to activate remote control on start");
    }

    std::future::pending::<()>().await;
    Ok(())
}

fn lock_state() -> Result<MutexGuard<'static, HostState>> {
    HOST_STATE
        .get()
        .ok_or_else(|| anyhow!("linux host state not initialized"))?
        .lock()
        .map_err(|_| anyhow!("linux host state lock poisoned"))
}

fn set_grabbed(grabbed: bool) {
    if let Some(signal) = GRAB_SIGNAL.get()
        && signal.send(grabbed).is_err()
    {
        warn!("grab signal has no receivers");
    }
}

// ---------------------------------------------------------------------------
// evdev capture
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeviceKind {
    Keyboard,
    Pointer,
    Both,
}

fn classify_device(device: &Device) -> Option<DeviceKind> {
    let keys = device.supported_keys();
    let pointer = device.supported_relative_axes().is_some_and(|axes| {
        axes.contains(RelativeAxisType::REL_X) && axes.contains(RelativeAxisType::REL_Y)
    }) && keys.is_some_and(|keys| keys.contains(Key(dev_key::BTN_LEFT)));
    let keyboard = keys.is_some_and(|keys| {
        keys.contains(Key(dev_key::KEY_A))
            && keys.contains(Key(dev_key::KEY_Z))
            && keys.contains(Key(dev_key::KEY_ENTER))
    });
    match (keyboard, pointer) {
        (true, true) => Some(DeviceKind::Both),
        (true, false) => Some(DeviceKind::Keyboard),
        (false, true) => Some(DeviceKind::Pointer),
        (false, false) => None,
    }
}

enum CaptureEvent {
    Key { code: u16, value: i32 },
    Pointer(PointerFrame),
}

#[derive(Default)]
struct PointerFrame {
    dx: i32,
    dy: i32,
    wheel_dx: i32,
    wheel_dy: i32,
    buttons: Vec<(MouseButton, KeyState)>,
}

impl PointerFrame {
    fn is_empty(&self) -> bool {
        self.dx == 0
            && self.dy == 0
            && self.wheel_dx == 0
            && self.wheel_dy == 0
            && self.buttons.is_empty()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum NodeVerdict {
    Captured,
    Ignored,
}

/// Device discovery runs on its own OS thread and opens each `/dev/input` node
/// at most once.
///
/// `open()` on an idle input node is not cheap: HDA jack-detect and NVIDIA
/// HDMI-audio nodes wake their codec and take ~30 ms each, so a full sweep of
/// this machine's 27 nodes measured 386-505 ms. `evdev::enumerate()` opens
/// every node on every call, so re-running it on a runtime worker every few
/// seconds stalled the whole host for half a second at a time. Listing the
/// directory is a plain readdir; only genuinely new paths get opened.
fn device_scan_thread(
    runtime: tokio::runtime::Handle,
    capture_tx: mpsc::UnboundedSender<CaptureEvent>,
    grab_rx: watch::Receiver<bool>,
) {
    let (gone_tx, mut gone_rx) = mpsc::unbounded_channel::<PathBuf>();
    let mut judged: HashMap<PathBuf, NodeVerdict> = HashMap::new();
    let mut warned_unreadable: HashSet<PathBuf> = HashSet::new();
    let mut warned_no_devices = false;

    loop {
        // A device whose reader task ended (unplug, read error) must be
        // reconsidered, otherwise it stays captured-but-dead until restart.
        while let Ok(path) = gone_rx.try_recv() {
            judged.remove(&path);
        }
        let present = list_event_nodes();
        judged.retain(|path, _| present.contains(path));
        warned_unreadable.retain(|path| present.contains(path));

        for path in &present {
            if judged.contains_key(path) {
                continue;
            }
            let device = match Device::open(path) {
                Ok(device) => device,
                Err(err) => {
                    // Not cached as a verdict: a node can show up before udev
                    // has applied its permissions, so keep retrying it.
                    if warned_unreadable.insert(path.clone()) {
                        warn!(path = %path.display(), ?err, "cannot open evdev node");
                    }
                    continue;
                }
            };
            let Some(kind) = classify_device(&device) else {
                judged.insert(path.clone(), NodeVerdict::Ignored);
                continue;
            };
            let name = device.name().unwrap_or("unknown").to_owned();
            if name.starts_with("softkvm") {
                judged.insert(path.clone(), NodeVerdict::Ignored);
                continue;
            }
            info!(path = %path.display(), %name, ?kind, "capturing evdev device");
            judged.insert(path.clone(), NodeVerdict::Captured);
            runtime.spawn(device_task(
                path.clone(),
                device,
                name,
                kind,
                capture_tx.clone(),
                grab_rx.clone(),
                gone_tx.clone(),
            ));
        }

        let captured = judged
            .values()
            .filter(|verdict| **verdict == NodeVerdict::Captured)
            .count();
        if captured == 0 && !warned_no_devices {
            warned_no_devices = true;
            warn!(
                "no readable evdev keyboard/pointer devices; add the user to the `input` group and re-login"
            );
        }
        if captured > 0 {
            warned_no_devices = false;
        }

        if capture_tx.is_closed() {
            return;
        }
        std::thread::sleep(DEVICE_RESCAN_INTERVAL);
    }
}

fn list_event_nodes() -> HashSet<PathBuf> {
    let mut nodes = HashSet::new();
    let entries = match std::fs::read_dir(DEVICE_DIR) {
        Ok(entries) => entries,
        Err(err) => {
            warn!(dir = DEVICE_DIR, ?err, "cannot list input devices");
            return nodes;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("event"))
        {
            nodes.insert(path);
        }
    }
    nodes
}

async fn device_task(
    path: PathBuf,
    device: Device,
    name: String,
    kind: DeviceKind,
    capture_tx: mpsc::UnboundedSender<CaptureEvent>,
    grab_rx: watch::Receiver<bool>,
    gone_tx: mpsc::UnboundedSender<PathBuf>,
) {
    device_loop(&path, device, &name, kind, capture_tx, grab_rx).await;
    let _ = gone_tx.send(path);
}

async fn device_loop(
    path: &std::path::Path,
    device: Device,
    name: &str,
    kind: DeviceKind,
    capture_tx: mpsc::UnboundedSender<CaptureEvent>,
    mut grab_rx: watch::Receiver<bool>,
) {
    let mut device = device;
    let raw_fd = std::os::unix::io::AsRawFd::as_raw_fd(&device);
    let fd = match AsyncFd::new(raw_fd) {
        Ok(fd) => fd,
        Err(err) => {
            warn!(%name, ?err, "failed to register evdev device with the async runtime");
            return;
        }
    };
    let mut grabbed = false;
    if *grab_rx.borrow() {
        set_device_grab(&mut device, name, true, &mut grabbed);
    }
    let mut frame = PointerFrame::default();

    loop {
        tokio::select! {
            changed = grab_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                let want = *grab_rx.borrow();
                set_device_grab(&mut device, name, want, &mut grabbed);
            }
            ready = fd.readable() => {
                let mut guard = match ready {
                    Ok(guard) => guard,
                    Err(err) => {
                        warn!(%name, ?err, "evdev readiness failed; dropping device");
                        return;
                    }
                };
                let events = match device.fetch_events() {
                    Ok(events) => events,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        guard.clear_ready();
                        continue;
                    }
                    Err(err) => {
                        warn!(%name, path = %path.display(), ?err, "evdev read failed; dropping device");
                        return;
                    }
                };

                let mut out: Vec<CaptureEvent> = Vec::new();
                for event in events {
                    match event.event_type() {
                        EventType::RELATIVE if kind != DeviceKind::Keyboard => {
                            match event.code() {
                                0 => frame.dx = frame.dx.saturating_add(event.value()),
                                1 => frame.dy = frame.dy.saturating_add(event.value()),
                                8 => frame.wheel_dy = frame.wheel_dy.saturating_add(event.value()),
                                6 => frame.wheel_dx = frame.wheel_dx.saturating_add(event.value()),
                                _ => {}
                            }
                        }
                        EventType::KEY => {
                            let code = event.code();
                            let value = event.value();
                            if kind != DeviceKind::Keyboard
                                && let Some(button) = mouse_button_for_code(code)
                            {
                                let state = if value == 0 { KeyState::Up } else { KeyState::Down };
                                frame.buttons.push((button, state));
                                continue;
                            }
                            flush_pointer_frame(&mut frame, &mut out);
                            out.push(CaptureEvent::Key { code, value });
                        }
                        EventType::SYNCHRONIZATION => {
                            flush_pointer_frame(&mut frame, &mut out);
                        }
                        _ => {}
                    }
                }
                guard.clear_ready();
                for event in out {
                    if capture_tx.send(event).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

fn flush_pointer_frame(frame: &mut PointerFrame, out: &mut Vec<CaptureEvent>) {
    if frame.is_empty() {
        return;
    }
    out.push(CaptureEvent::Pointer(std::mem::take(frame)));
}

fn set_device_grab(device: &mut Device, name: &str, want: bool, grabbed: &mut bool) {
    if *grabbed == want {
        return;
    }
    let result = if want { device.grab() } else { device.ungrab() };
    match result {
        Ok(()) => *grabbed = want,
        Err(err) if err.raw_os_error() == Some(16) => {
            // EBUSY: another client (e.g. hk-translator) already grabbed this
            // device; its events reach us through the virtual device anyway.
            info!(%name, "evdev grab refused (EBUSY); another grabber owns this device");
        }
        Err(err) => warn!(%name, ?err, want, "evdev grab/ungrab failed"),
    }
}

// ---------------------------------------------------------------------------
// capture dispatcher: edge activation, hotkey, forwarding decisions
// ---------------------------------------------------------------------------

/// Dispatch runs on its own OS thread because every edge decision talks to
/// Hyprland over a blocking unix socket; on a runtime worker those roundtrips
/// sit in front of the motion writer.
fn dispatcher_thread(mut rx: mpsc::UnboundedReceiver<CaptureEvent>) {
    let mut hypr = Hyprland::connect();
    if !hypr.available() {
        warn!(
            "Hyprland IPC not available yet; will keep retrying. Until then left-edge \
             activation and cursor restore are disabled, Ctrl+Alt+\\ hotkey still works"
        );
    }
    while let Some(event) = rx.blocking_recv() {
        let result = match event {
            CaptureEvent::Key { code, value } => handle_key(code, value),
            CaptureEvent::Pointer(frame) => handle_pointer(frame, &mut hypr),
        };
        if let Err(err) = result {
            warn!(?err, "capture dispatch failed");
        }
    }
}

fn handle_key(code: u16, value: i32) -> Result<()> {
    let mut state = lock_state()?;

    match value {
        1 => *state.pressed_key_counts.entry(code).or_insert(0) += 1,
        0 => {
            if let Some(count) = state.pressed_key_counts.get_mut(&code) {
                *count = count.saturating_sub(1);
            }
        }
        _ => {}
    }

    if value == 1
        && is_backslash_code(code)
        && state.active_modifiers.contains(&Modifier::Control)
        && (state.active_modifiers.contains(&Modifier::Alt)
            || state.active_modifiers.contains(&Modifier::Super))
    {
        if state.accept_hotkey_toggle() {
            let active = !state.remote_active;
            state.set_remote_active(active, "hotkey Ctrl+(Alt|Win)+\\", Some(0.5), Some(0.5))?;
        }
        return Ok(());
    }

    if !state.remote_active {
        return Ok(());
    }

    let key_state = if value == 0 { KeyState::Up } else { KeyState::Down };
    for event in state.forward_key_events(code, key_state) {
        state.send(HostCommand::Input(event))?;
    }
    Ok(())
}

fn handle_pointer(frame: PointerFrame, hypr: &mut Hyprland) -> Result<()> {
    let started = Instant::now();
    crate::trace::stamp(
        crate::trace::Stage::LnxEvdevIn,
        0,
        frame.dx,
        frame.dy,
        frame.buttons.len() as u32,
    );

    let mut state = lock_state()?;
    let mut frame = frame;

    if !state.remote_active
        && frame.dx < 0
        && state.left_edge_armed
        && let Some((cursor_x, cursor_y)) = hypr.cursor_position()
        && let Some(edge_x) = hypr.left_edge_x()
        && cursor_x <= edge_x + EDGE_TRIGGER_PX
    {
        let y_ratio = hypr
            .monitor_ratios(cursor_x, cursor_y)
            .map(|(_, y_ratio)| y_ratio)
            .unwrap_or(0.5);
        state.set_remote_active(
            true,
            "left edge",
            Some(MAC_ENTRY_X_RATIO_FROM_LINUX),
            Some(y_ratio),
        )?;
        info!(
            target: "softkvm::latency",
            dx = frame.dx,
            dy = frame.dy,
            y_ratio,
            elapsed_ms = latency::ms(started.elapsed()),
            "linux left-edge activation packet"
        );
        frame.dx = frame.dx.clamp(-64, 0);
        frame.dy = frame.dy.clamp(-64, 64);
    }

    if !state.remote_active {
        state.update_left_edge_arm(hypr);
        return Ok(());
    }

    let has_discrete = !frame.buttons.is_empty() || frame.wheel_dx != 0 || frame.wheel_dy != 0;
    if frame.dx != 0 || frame.dy != 0 {
        if has_discrete {
            state.send(HostCommand::InputImmediate(InputEvent::MouseMotion {
                dx: frame.dx,
                dy: frame.dy,
            }))?;
        } else {
            match state.send_direct_motion(frame.dx, frame.dy) {
                Ok(true) => {}
                Ok(false) => {
                    state.send(HostCommand::Input(InputEvent::MouseMotion {
                        dx: frame.dx,
                        dy: frame.dy,
                    }))?;
                }
                Err(err) => {
                    warn!(?err, "direct udp motion send failed; falling back to host writer");
                    state.send(HostCommand::Input(InputEvent::MouseMotion {
                        dx: frame.dx,
                        dy: frame.dy,
                    }))?;
                }
            }
        }
    }
    for (button, button_state) in frame.buttons {
        state.send(HostCommand::Input(InputEvent::MouseButton {
            button,
            state: button_state,
        }))?;
    }
    if frame.wheel_dy != 0 {
        state.send(HostCommand::Input(InputEvent::MouseWheel {
            dx: 0,
            dy: frame.wheel_dy,
        }))?;
    }
    if frame.wheel_dx != 0 {
        state.send(HostCommand::Input(InputEvent::MouseWheel {
            dx: frame.wheel_dx,
            dy: 0,
        }))?;
    }

    let elapsed = started.elapsed();
    if latency::report(elapsed) {
        info!(
            target: "softkvm::latency",
            remote_active = state.remote_active,
            elapsed_ms = latency::ms(elapsed),
            "linux mouse evdev handling"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// host state machine
// ---------------------------------------------------------------------------

enum HostCommand {
    HostState {
        active: bool,
        reason: &'static str,
    },
    HostStateWithEntry {
        active: bool,
        reason: &'static str,
        entry_x_ratio: f64,
        entry_y_ratio: f64,
    },
    Input(InputEvent),
    InputImmediate(InputEvent),
    UdpMotionReady,
    Reset,
    Clipboard(ClipboardEvent),
}

struct HostState {
    tx: mpsc::UnboundedSender<HostCommand>,
    direct_motion: Option<SharedUdpMotionWriter>,
    remote_active: bool,
    pressed_key_counts: HashMap<u16, u32>,
    active_modifiers: HashSet<Modifier>,
    saved_cursor_pos: Option<(f64, f64)>,
    last_hotkey_toggle: Option<Instant>,
    left_edge_armed: bool,
    local_mouse_capture: bool,
}

impl HostState {
    fn new(
        tx: mpsc::UnboundedSender<HostCommand>,
        direct_motion: Option<SharedUdpMotionWriter>,
        local_mouse_capture: bool,
    ) -> Self {
        Self {
            tx,
            direct_motion,
            remote_active: false,
            pressed_key_counts: HashMap::new(),
            active_modifiers: HashSet::new(),
            saved_cursor_pos: None,
            last_hotkey_toggle: None,
            left_edge_armed: true,
            local_mouse_capture,
        }
    }

    fn accept_hotkey_toggle(&mut self) -> bool {
        let now = Instant::now();
        if self.last_hotkey_toggle.is_some_and(|last| {
            now.duration_since(last) < Duration::from_millis(HOTKEY_DEBOUNCE_MS)
        }) {
            return false;
        }
        self.last_hotkey_toggle = Some(now);
        true
    }

    fn set_remote_active(
        &mut self,
        active: bool,
        reason: &'static str,
        entry_x_ratio: Option<f64>,
        entry_y_ratio: Option<f64>,
    ) -> Result<()> {
        if self.remote_active == active {
            return Ok(());
        }
        let total_started = Instant::now();

        if active {
            self.saved_cursor_pos = Hyprland::connect().cursor_position();
            if self.local_mouse_capture {
                set_grabbed(true);
            }
            self.remote_active = true;
            REMOTE_ACTIVE_FAST.store(true, Ordering::Relaxed);
            crate::trace::set_active(true);
            self.left_edge_armed = false;
            let send_result = match entry_x_ratio.zip(entry_y_ratio) {
                Some((entry_x_ratio, entry_y_ratio)) => {
                    self.send(HostCommand::HostStateWithEntry {
                        active,
                        reason,
                        entry_x_ratio,
                        entry_y_ratio,
                    })
                }
                None => self.send(HostCommand::HostState { active, reason }),
            };
            if let Err(err) = send_result {
                self.remote_active = false;
                REMOTE_ACTIVE_FAST.store(false, Ordering::Relaxed);
                crate::trace::set_active(false);
                set_grabbed(false);
                return Err(err);
            }
            for event in self.seed_current_modifier_state() {
                self.send(HostCommand::InputImmediate(event))?;
            }
            info!(
                target: "softkvm::latency",
                reason,
                total_ms = latency::ms(total_started.elapsed()),
                "linux remote activation latency"
            );
            info!(reason, "remote macOS control enabled");
        } else {
            self.remote_active = false;
            REMOTE_ACTIVE_FAST.store(false, Ordering::Relaxed);
            crate::trace::set_active(false);
            self.left_edge_armed = false;
            set_grabbed(false);
            self.restore_cursor(entry_x_ratio, entry_y_ratio);
            self.send(HostCommand::HostState { active, reason })?;
            self.send(HostCommand::Reset)?;
            info!(reason, "remote macOS control disabled");
        }
        Ok(())
    }

    fn restore_cursor(&mut self, entry_x_ratio: Option<f64>, entry_y_ratio: Option<f64>) {
        let Some(saved) = self.saved_cursor_pos.take() else {
            return;
        };
        let mut hypr = Hyprland::connect();
        let Some(monitor) = hypr.monitor_for_point(saved.0, saved.1) else {
            return;
        };
        let target = match entry_x_ratio.zip(entry_y_ratio) {
            Some((x_ratio, y_ratio)) => clamp_restore_point_inside_left_edge(
                monitor,
                point_from_rect_ratios(monitor, x_ratio, y_ratio),
            ),
            None => clamp_restore_point_inside_left_edge(monitor, saved),
        };
        hypr.move_cursor(target.0, target.1);
    }

    fn send(&self, command: HostCommand) -> Result<()> {
        self.tx
            .send(command)
            .map_err(|_| anyhow!("host writer task is gone"))
    }

    fn send_direct_motion(&self, dx: i32, dy: i32) -> Result<bool> {
        let Some(writer) = &self.direct_motion else {
            return Ok(false);
        };
        writer.send_motion_if_confirmed(dx, dy, "direct")
    }

    fn forward_key_events(
        &mut self,
        code: u16,
        key_state: KeyState,
    ) -> Vec<InputEvent> {
        if let Some(modifier) = modifier_for_code(code) {
            let was_active = self.active_modifiers.contains(&modifier);
            let is_active = self
                .pressed_key_counts
                .iter()
                .any(|(pressed, count)| *count > 0 && modifier_for_code(*pressed) == Some(modifier));

            if was_active == is_active {
                return Vec::new();
            }
            if is_active {
                self.active_modifiers.insert(modifier);
                vec![InputEvent::Modifier {
                    modifier,
                    state: KeyState::Down,
                }]
            } else {
                self.active_modifiers.remove(&modifier);
                vec![InputEvent::Modifier {
                    modifier,
                    state: KeyState::Up,
                }]
            }
        } else {
            vec![InputEvent::Key {
                key: key_for_code(code),
                state: key_state,
            }]
        }
    }

    fn seed_current_modifier_state(&self) -> Vec<InputEvent> {
        self.active_modifiers
            .iter()
            .map(|modifier| InputEvent::Modifier {
                modifier: *modifier,
                state: KeyState::Down,
            })
            .collect()
    }

    fn update_left_edge_arm(&mut self, hypr: &mut Hyprland) {
        if self.left_edge_armed {
            return;
        }
        let Some((cursor_x, _)) = hypr.cursor_position() else {
            return;
        };
        let Some(edge_x) = hypr.left_edge_x() else {
            return;
        };
        if cursor_x > edge_x + EDGE_TRIGGER_PX + EDGE_REARM_PX {
            self.left_edge_armed = true;
        }
    }
}

fn release_host_state_after_transport_loss(reason: &'static str) {
    match lock_state() {
        Ok(mut state) => {
            if state.remote_active {
                warn!(reason, "transport lost; releasing local Linux controls");
            }
            state.remote_active = false;
            REMOTE_ACTIVE_FAST.store(false, Ordering::Relaxed);
            crate::trace::set_active(false);
            state.left_edge_armed = false;
            set_grabbed(false);
            state.restore_cursor(None, None);
        }
        Err(err) => warn!(
            ?err,
            reason, "failed to release local controls after transport loss"
        ),
    }
}

// ---------------------------------------------------------------------------
// connection supervisor + writer session (mirrors the Windows host)
// ---------------------------------------------------------------------------

async fn connection_supervisor(
    peer: String,
    mut rx: mpsc::UnboundedReceiver<HostCommand>,
    mut motion_transport: MotionTransport,
) {
    let mut logged_waiting = false;
    loop {
        let stream = match crate::transport::connect_tcp_with_retry(
            &peer,
            1,
            Duration::from_millis(0),
        )
        .await
        {
            Ok(stream) => stream,
            Err(err) => {
                if !logged_waiting {
                    info!(%peer, %err, "peer unreachable; auto-connect keeps retrying");
                    logged_waiting = true;
                }
                tokio::time::sleep(Duration::from_millis(1500)).await;
                continue;
            }
        };
        logged_waiting = false;
        if let Err(err) = stream.set_nodelay(true) {
            warn!(?err, "failed to set TCP_NODELAY");
        }
        info!(%peer, "host connected");

        let (read_half, write_half) = stream.into_split();
        let reader = tokio::spawn(control_reader_task(read_half));
        writer_session(write_half, &mut rx, &mut motion_transport).await;
        reader.abort();

        release_host_state_after_transport_loss("connection lost; reconnecting");
        while rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
}

async fn writer_session(
    stream: OwnedWriteHalf,
    rx: &mut mpsc::UnboundedReceiver<HostCommand>,
    motion_transport: &mut MotionTransport,
) {
    let mut writer = FrameWriter::new(stream);
    let session_id = Uuid::new_v4();
    let mut seq = 1_u64;

    if !write_host_message(
        &mut writer,
        session_id,
        &mut seq,
        Message::Hello(ProtocolHello {
            protocol_version: protocol::PROTOCOL_VERSION,
            role: "linux-host".to_owned(),
            device_name: std::env::var("HOSTNAME")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "linux".to_owned()),
        }),
        "host hello failed",
    )
    .await
    {
        return;
    }

    let mut pending_dx = 0_i32;
    let mut pending_dy = 0_i32;
    let mut deferred_command = None;
    let mut flush_timer = interval(Duration::from_millis(MOUSE_FLUSH_INTERVAL_MS));
    flush_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        if let Some(command) = deferred_command.take() {
            if !handle_host_command(
                command,
                rx,
                &mut writer,
                motion_transport,
                session_id,
                &mut seq,
                &mut pending_dx,
                &mut pending_dy,
                &mut deferred_command,
            )
            .await
            {
                break;
            }
            continue;
        }

        tokio::select! {
            biased;

            _ = flush_timer.tick(), if pending_dx != 0 || pending_dy != 0 => {
                if !flush_pending_motion(&mut writer, session_id, &mut seq, &mut pending_dx, &mut pending_dy).await {
                    break;
                }
            }
            command = rx.recv() => {
                let Some(command) = command else {
                    let _ = flush_pending_motion(&mut writer, session_id, &mut seq, &mut pending_dx, &mut pending_dy).await;
                    break;
                };

                if !handle_host_command(
                    command,
                    rx,
                    &mut writer,
                    motion_transport,
                    session_id,
                    &mut seq,
                    &mut pending_dx,
                    &mut pending_dy,
                    &mut deferred_command,
                ).await {
                    break;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_host_command(
    command: HostCommand,
    rx: &mut mpsc::UnboundedReceiver<HostCommand>,
    writer: &mut FrameWriter<OwnedWriteHalf>,
    motion_transport: &mut MotionTransport,
    session_id: Uuid,
    seq: &mut u64,
    pending_dx: &mut i32,
    pending_dy: &mut i32,
    deferred_command: &mut Option<HostCommand>,
) -> bool {
    match command {
        HostCommand::Input(InputEvent::MouseMotion { dx, dy }) => {
            match motion_transport.send_motion(dx, dy).await {
                Ok(true) => return true,
                Ok(false) => {}
                Err(err) => {
                    warn!(
                        ?err,
                        "udp motion send failed; falling back to tcp/json motion"
                    );
                    *motion_transport = MotionTransport::Tcp;
                }
            }

            *pending_dx = (*pending_dx).saturating_add(dx);
            *pending_dy = (*pending_dy).saturating_add(dy);

            for _ in 0..MAX_MOTION_DRAIN_PER_TURN {
                let Ok(command) = rx.try_recv() else {
                    break;
                };
                match command {
                    HostCommand::Input(InputEvent::MouseMotion { dx, dy }) => {
                        *pending_dx = (*pending_dx).saturating_add(dx);
                        *pending_dy = (*pending_dy).saturating_add(dy);
                    }
                    other => {
                        *deferred_command = Some(other);
                        break;
                    }
                }
            }
            true
        }
        HostCommand::InputImmediate(event) => {
            if !flush_pending_motion(writer, session_id, seq, pending_dx, pending_dy).await {
                return false;
            }
            write_host_message(
                writer,
                session_id,
                seq,
                Message::Input(event),
                "host writer disconnected",
            )
            .await
        }
        HostCommand::HostState { active, reason } => {
            if !flush_pending_motion(writer, session_id, seq, pending_dx, pending_dy).await {
                return false;
            }
            write_host_message(
                writer,
                session_id,
                seq,
                Message::HostState(HostStateEvent {
                    remote_active: active,
                    reason: reason.to_owned(),
                    entry_x_ratio: None,
                    entry_y_ratio: None,
                }),
                "host writer disconnected",
            )
            .await
        }
        HostCommand::HostStateWithEntry {
            active,
            reason,
            entry_x_ratio,
            entry_y_ratio,
        } => {
            if !flush_pending_motion(writer, session_id, seq, pending_dx, pending_dy).await {
                return false;
            }
            let ok = write_host_message(
                writer,
                session_id,
                seq,
                Message::HostState(HostStateEvent {
                    remote_active: active,
                    reason: reason.to_owned(),
                    entry_x_ratio: Some(entry_x_ratio),
                    entry_y_ratio: Some(entry_y_ratio),
                }),
                "host writer disconnected",
            )
            .await;
            if ok && active {
                motion_transport.probe().await;
            }
            ok
        }
        HostCommand::UdpMotionReady => {
            motion_transport.confirm();
            true
        }
        HostCommand::Clipboard(event) => {
            if !flush_pending_motion(writer, session_id, seq, pending_dx, pending_dy).await {
                return false;
            }
            write_host_message(
                writer,
                session_id,
                seq,
                Message::Clipboard(event),
                "host writer disconnected",
            )
            .await
        }
        other => {
            if !flush_pending_motion(writer, session_id, seq, pending_dx, pending_dy).await {
                return false;
            }
            write_host_message(
                writer,
                session_id,
                seq,
                message_from_host_command(other),
                "host writer disconnected",
            )
            .await
        }
    }
}

async fn flush_pending_motion(
    writer: &mut FrameWriter<OwnedWriteHalf>,
    session_id: Uuid,
    seq: &mut u64,
    pending_dx: &mut i32,
    pending_dy: &mut i32,
) -> bool {
    if *pending_dx == 0 && *pending_dy == 0 {
        return true;
    }

    let dx =
        std::mem::take(pending_dx).clamp(-MAX_MOTION_DELTA_PER_FLUSH, MAX_MOTION_DELTA_PER_FLUSH);
    let dy =
        std::mem::take(pending_dy).clamp(-MAX_MOTION_DELTA_PER_FLUSH, MAX_MOTION_DELTA_PER_FLUSH);
    write_host_message(
        writer,
        session_id,
        seq,
        Message::Input(InputEvent::MouseMotion { dx, dy }),
        "host writer disconnected",
    )
    .await
}

async fn write_host_message(
    writer: &mut FrameWriter<OwnedWriteHalf>,
    session_id: Uuid,
    seq: &mut u64,
    message: Message,
    disconnect_reason: &'static str,
) -> bool {
    let message_label = message.label();
    let current_seq = *seq;
    let started = Instant::now();
    if let Err(err) = writer
        .write_frame(Frame::new(session_id, current_seq, message))
        .await
    {
        error!(?err, "host writer disconnected");
        release_host_state_after_transport_loss(disconnect_reason);
        return false;
    }
    let elapsed = started.elapsed();
    if latency::report(elapsed) {
        info!(
            target: "softkvm::latency",
            seq = current_seq,
            message = message_label,
            elapsed_ms = latency::ms(elapsed),
            "linux tcp write latency"
        );
    }
    *seq += 1;
    true
}

fn message_from_host_command(command: HostCommand) -> Message {
    match command {
        HostCommand::HostState { active, reason } => Message::HostState(HostStateEvent {
            remote_active: active,
            reason: reason.to_owned(),
            entry_x_ratio: None,
            entry_y_ratio: None,
        }),
        HostCommand::HostStateWithEntry {
            active,
            reason,
            entry_x_ratio,
            entry_y_ratio,
        } => Message::HostState(HostStateEvent {
            remote_active: active,
            reason: reason.to_owned(),
            entry_x_ratio: Some(entry_x_ratio),
            entry_y_ratio: Some(entry_y_ratio),
        }),
        HostCommand::Input(event) | HostCommand::InputImmediate(event) => Message::Input(event),
        HostCommand::Reset => Message::InputReset,
        HostCommand::Clipboard(event) => Message::Clipboard(event),
        HostCommand::UdpMotionReady => unreachable!("handled before serialization"),
    }
}

async fn control_reader_task(stream: OwnedReadHalf) {
    let mut reader = crate::transport::FrameReader::new(stream);

    loop {
        let frame = match reader.read_frame().await {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                warn!("host control reader disconnected");
                release_host_state_after_transport_loss("control reader disconnected");
                break;
            }
            Err(err) => {
                warn!(?err, "host control reader failed");
                release_host_state_after_transport_loss("control reader failed");
                break;
            }
        };

        if let Message::Clipboard(event) = frame.message {
            info!(kind = event.label(), "applying clipboard from client");
            tokio::task::spawn_blocking(move || apply_clipboard_event(&event));
            continue;
        }
        if let Message::ClientControl(control) = frame.message {
            match control {
                ClientControlEvent::ReleaseHost {
                    reason,
                    entry_x_ratio,
                    entry_y_ratio,
                } => {
                    info!(%reason, "client requested host release");
                    match lock_state() {
                        Ok(mut state) => {
                            if state.remote_active
                                && let Err(err) = state.set_remote_active(
                                    false,
                                    "mac right edge",
                                    entry_x_ratio,
                                    entry_y_ratio,
                                )
                            {
                                warn!(?err, "failed to release host from client control");
                            }
                        }
                        Err(err) => warn!(?err, "failed to lock host state for client control"),
                    }
                }
                ClientControlEvent::UdpMotionReady => match lock_state() {
                    Ok(state) => {
                        if let Err(err) = state.send(HostCommand::UdpMotionReady) {
                            warn!(?err, "failed to forward udp motion ready control");
                        }
                    }
                    Err(err) => warn!(?err, "failed to lock host state for udp motion ready"),
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UDP motion transport (mirrors the Windows host)
// ---------------------------------------------------------------------------

enum MotionTransport {
    Udp { writer: SharedUdpMotionWriter },
    Tcp,
}

impl MotionTransport {
    async fn connect(peer: &str) -> Self {
        let configured = std::env::var("SOFTKVM_MOTION_TRANSPORT")
            .unwrap_or_else(|_| "udp".to_owned())
            .to_ascii_lowercase();
        if !matches!(configured.as_str(), "udp" | "udp-binary" | "binary") {
            info!(mode = configured, "using tcp/json motion transport");
            return Self::Tcp;
        }

        match crate::transport::bind_udp_with_fallback() {
            Ok(socket) => match socket.connect(peer) {
                Ok(()) => {
                    info!(%peer, "using udp/binary motion transport");
                    Self::Udp {
                        writer: SharedUdpMotionWriter::new(socket),
                    }
                }
                Err(err) => {
                    warn!(?err, %peer, "udp motion connect failed; falling back to tcp/json motion");
                    Self::Tcp
                }
            },
            Err(err) => {
                warn!(
                    ?err,
                    "udp motion bind failed; falling back to tcp/json motion"
                );
                Self::Tcp
            }
        }
    }

    async fn send_motion(&mut self, dx: i32, dy: i32) -> Result<bool> {
        match self {
            Self::Udp { writer } if writer.confirmed() => {
                writer.send_motion_if_confirmed(dx, dy, "writer")
            }
            Self::Udp { writer } => {
                writer.send_motion(0, 0, "probe")?;
                Ok(false)
            }
            Self::Tcp => Ok(false),
        }
    }

    async fn probe(&mut self) {
        if let Self::Udp { writer } = self
            && !writer.confirmed()
            && let Err(err) = writer.send_motion(0, 0, "probe")
        {
            warn!(
                ?err,
                "udp motion probe failed; falling back to tcp/json motion"
            );
            *self = Self::Tcp;
        }
    }

    fn confirm(&mut self) {
        if let Self::Udp { writer } = self
            && writer.confirm()
        {
            info!("udp motion transport confirmed by macOS client");
        }
    }

    fn direct_writer(&self) -> Option<SharedUdpMotionWriter> {
        match self {
            Self::Udp { writer } => Some(writer.clone()),
            Self::Tcp => None,
        }
    }
}

#[derive(Clone)]
struct SharedUdpMotionWriter {
    socket: Arc<StdUdpSocket>,
    seq: Arc<AtomicU64>,
    confirmed: Arc<AtomicBool>,
}

impl SharedUdpMotionWriter {
    fn new(socket: StdUdpSocket) -> Self {
        info!("using immediate UDP motion sends");
        Self {
            socket: Arc::new(socket),
            seq: Arc::new(AtomicU64::new(1)),
            confirmed: Arc::new(AtomicBool::new(true)),
        }
    }

    fn confirmed(&self) -> bool {
        self.confirmed.load(Ordering::Acquire)
    }

    fn confirm(&self) -> bool {
        !self.confirmed.swap(true, Ordering::AcqRel)
    }

    fn send_motion_if_confirmed(&self, dx: i32, dy: i32, path: &'static str) -> Result<bool> {
        if !self.confirmed() {
            return Ok(false);
        }
        self.send_motion(dx, dy, path)?;
        Ok(true)
    }

    fn send_motion(&self, dx: i32, dy: i32, path: &'static str) -> Result<()> {
        let packet = MotionDatagram {
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            dx: dx.clamp(-MAX_MOTION_DELTA_PER_FLUSH, MAX_MOTION_DELTA_PER_FLUSH),
            dy: dy.clamp(-MAX_MOTION_DELTA_PER_FLUSH, MAX_MOTION_DELTA_PER_FLUSH),
            t_send_us: crate::trace::now_us() as u32,
        };
        // SKM2 (with the sender stamp) only while tracing, so untraced runs
        // keep the exact SKM1 wire format older clients expect.
        let encoded_v2;
        let encoded_v1;
        let encoded: &[u8] = if crate::trace::enabled() {
            encoded_v2 = packet.encode_v2();
            &encoded_v2
        } else {
            encoded_v1 = packet.encode();
            &encoded_v1
        };
        let started = Instant::now();
        crate::trace::stamp(
            crate::trace::Stage::LnxUdpPre,
            packet.seq,
            packet.dx,
            packet.dy,
            0,
        );
        let sent = self
            .socket
            .send(encoded)
            .context("send udp motion packet")?;
        crate::trace::stamp(
            crate::trace::Stage::LnxUdpPost,
            packet.seq,
            packet.dx,
            packet.dy,
            0,
        );
        if sent != encoded.len() {
            bail!("short udp motion send: {sent}/{}", encoded.len());
        }
        let elapsed = started.elapsed();
        if latency::report(elapsed) {
            info!(
                target: "softkvm::latency",
                path,
                seq = packet.seq,
                dx = packet.dx,
                dy = packet.dy,
                elapsed_ms = latency::ms(elapsed),
                "linux udp motion send latency"
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Hyprland IPC: cursor position, monitor geometry, cursor warps
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
struct Monitor {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

struct Hyprland {
    socket_path: Option<PathBuf>,
    monitors: Vec<Monitor>,
    monitors_fetched_at: Option<Instant>,
    warned_unavailable: bool,
    last_resolve_attempt: Option<Instant>,
}

impl Hyprland {
    fn connect() -> Self {
        let mut hypr = Self {
            socket_path: hypr_socket_path(),
            monitors: Vec::new(),
            monitors_fetched_at: None,
            warned_unavailable: false,
            last_resolve_attempt: None,
        };
        hypr.refresh_monitors();
        hypr
    }

    fn available(&self) -> bool {
        self.socket_path.is_some() && !self.monitors.is_empty()
    }

    fn request(&mut self, command: &str) -> Result<String> {
        if self.socket_path.is_none()
            && self
                .last_resolve_attempt
                .is_none_or(|attempt| attempt.elapsed() > HYPR_RESOLVE_RETRY)
        {
            self.last_resolve_attempt = Some(Instant::now());
            if let Some(path) = hypr_socket_path() {
                info!(path = %path.display(), "hyprland ipc socket discovered");
                self.socket_path = Some(path);
            }
        }
        let path = self
            .socket_path
            .as_ref()
            .ok_or_else(|| anyhow!("hyprland socket path unknown"))?;
        let mut stream = std::os::unix::net::UnixStream::connect(path)
            .context("connect hyprland ipc socket")?;
        stream
            .set_read_timeout(Some(HYPR_IPC_TIMEOUT))
            .context("set hyprland read timeout")?;
        stream
            .set_write_timeout(Some(HYPR_IPC_TIMEOUT))
            .context("set hyprland write timeout")?;
        stream
            .write_all(command.as_bytes())
            .context("write hyprland request")?;
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .context("read hyprland response")?;
        Ok(response)
    }

    fn cursor_position(&mut self) -> Option<(f64, f64)> {
        match self.request("cursorpos") {
            Ok(response) => parse_cursorpos(&response),
            Err(err) => {
                self.warn_unavailable_once(err);
                None
            }
        }
    }

    fn left_edge_x(&mut self) -> Option<f64> {
        self.refresh_monitors_if_stale();
        self.monitors.iter().map(|monitor| monitor.x).reduce(f64::min)
    }

    fn monitor_for_point(&mut self, x: f64, y: f64) -> Option<Monitor> {
        self.refresh_monitors_if_stale();
        self.monitors
            .iter()
            .copied()
            .find(|monitor| {
                x >= monitor.x
                    && x < monitor.x + monitor.width
                    && y >= monitor.y
                    && y < monitor.y + monitor.height
            })
            .or_else(|| self.monitors.first().copied())
    }

    fn monitor_ratios(&mut self, x: f64, y: f64) -> Option<(f64, f64)> {
        let monitor = self.monitor_for_point(x, y)?;
        let x_ratio = ((x - monitor.x) / monitor.width).clamp(0.0, 1.0);
        let y_ratio = ((y - monitor.y) / monitor.height).clamp(0.0, 1.0);
        Some((x_ratio, y_ratio))
    }

    fn move_cursor(&mut self, x: f64, y: f64) {
        let command = format!("dispatch movecursor {} {}", x.round() as i32, y.round() as i32);
        if let Err(err) = self.request(&command) {
            warn!(?err, x, y, "hyprland movecursor failed");
        }
    }

    fn refresh_monitors_if_stale(&mut self) {
        if self
            .monitors_fetched_at
            .is_some_and(|fetched| fetched.elapsed() < MONITOR_CACHE_TTL)
        {
            return;
        }
        self.refresh_monitors();
    }

    fn refresh_monitors(&mut self) {
        let Ok(response) = self.request("j/monitors") else {
            return;
        };
        match parse_monitors(&response) {
            Ok(monitors) if !monitors.is_empty() => {
                self.monitors = monitors;
                self.monitors_fetched_at = Some(Instant::now());
            }
            Ok(_) => warn!("hyprland returned zero monitors"),
            Err(err) => warn!(?err, "failed to parse hyprland monitors"),
        }
    }

    fn warn_unavailable_once(&mut self, err: anyhow::Error) {
        if self.warned_unavailable {
            return;
        }
        self.warned_unavailable = true;
        warn!(?err, "hyprland ipc unavailable");
    }
}

fn hypr_socket_path() -> Option<PathBuf> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|value| !value.is_empty())?;
    let hypr_dir = PathBuf::from(runtime_dir).join("hypr");

    if let Ok(signature) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        && !signature.is_empty()
    {
        let candidate = hypr_dir.join(&signature).join(".socket.sock");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // Daemon fallback: a user service may start with a stale environment (or
    // before Hyprland), so pick the most recently touched instance directory.
    let entries = std::fs::read_dir(&hypr_dir).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let socket = entry.path().join(".socket.sock");
        if !socket.exists() {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(best_modified, _)| modified > *best_modified) {
            best = Some((modified, socket));
        }
    }
    best.map(|(_, socket)| socket)
}

fn parse_cursorpos(response: &str) -> Option<(f64, f64)> {
    let mut parts = response.trim().split(',');
    let x = parts.next()?.trim().parse::<f64>().ok()?;
    let y = parts.next()?.trim().parse::<f64>().ok()?;
    Some((x, y))
}

fn parse_monitors(json: &str) -> Result<Vec<Monitor>> {
    #[derive(serde::Deserialize)]
    struct HyprMonitor {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        #[serde(default = "default_scale")]
        scale: f64,
    }
    fn default_scale() -> f64 {
        1.0
    }

    let parsed: Vec<HyprMonitor> =
        serde_json::from_str(json).context("parse hyprland monitors json")?;
    Ok(parsed
        .into_iter()
        .map(|monitor| {
            let scale = if monitor.scale > 0.0 { monitor.scale } else { 1.0 };
            Monitor {
                x: monitor.x,
                y: monitor.y,
                width: monitor.width / scale,
                height: monitor.height / scale,
            }
        })
        .collect())
}

fn point_from_rect_ratios(monitor: Monitor, x_ratio: f64, y_ratio: f64) -> (f64, f64) {
    let inset = 16.0;
    (
        monitor.x + (monitor.width - inset * 2.0) * x_ratio.clamp(0.0, 1.0) + inset,
        monitor.y + (monitor.height - inset * 2.0) * y_ratio.clamp(0.0, 1.0) + inset,
    )
}

fn clamp_restore_point_inside_left_edge(monitor: Monitor, point: (f64, f64)) -> (f64, f64) {
    let min_x = (monitor.x + EDGE_TRIGGER_PX + LINUX_RESTORE_EDGE_INSET_PX)
        .min(monitor.x + monitor.width - 1.0);
    let max_x = monitor.x + monitor.width - 1.0;
    let max_y = (monitor.y + monitor.height - 1.0).max(monitor.y);
    (
        point.0.clamp(monitor.x, max_x).max(min_x),
        point.1.clamp(monitor.y, max_y),
    )
}

// ---------------------------------------------------------------------------
// clipboard sync via wl-clipboard
// ---------------------------------------------------------------------------

fn clipboard_watcher_thread() {
    loop {
        if let Err(err) = run_clipboard_watcher() {
            warn!(?err, "clipboard watcher failed; restarting");
            std::thread::sleep(Duration::from_secs(2));
        }
    }
}

fn run_clipboard_watcher() -> Result<()> {
    let mut child = std::process::Command::new("wl-paste")
        .args(["--watch", "echo", "softkvm-clipboard-change"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawn wl-paste --watch")?;
    let stdout = child.stdout.take().context("wl-paste stdout")?;
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        let read = std::io::BufRead::read_line(&mut reader, &mut line)
            .context("read wl-paste watch line")?;
        if read == 0 {
            bail!("wl-paste --watch exited");
        }
        let Some(event) = read_clipboard_event() else {
            continue;
        };
        if !shared_clipboard::within_limits(&event) {
            warn!(kind = event.label(), "clipboard event exceeds size limits; dropped");
            continue;
        }
        if !shared_clipboard::should_send(&event) {
            continue;
        }
        let state = lock_state()?;
        state.send(HostCommand::Clipboard(event))?;
    }
}

fn read_clipboard_event() -> Option<ClipboardEvent> {
    let types = clipboard_command_output(["--list-types"])?;
    let types = String::from_utf8(types).ok()?;
    let types: Vec<&str> = types.lines().map(str::trim).collect();

    if types.contains(&"image/png")
        && let Some(png) = clipboard_command_output(["-t", "image/png"])
    {
        use base64::Engine as _;
        return Some(ClipboardEvent::ImagePng {
            base64: base64::engine::general_purpose::STANDARD.encode(png),
        });
    }

    let text_type = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING"]
        .into_iter()
        .find(|wanted| types.contains(wanted))?;
    let bytes = clipboard_command_output(["-t", text_type])?;
    let text = String::from_utf8(bytes).ok()?;
    if text.is_empty() {
        return None;
    }
    Some(ClipboardEvent::Text(text))
}

fn clipboard_command_output<const N: usize>(args: [&str; N]) -> Option<Vec<u8>> {
    let output = std::process::Command::new("wl-paste")
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

fn apply_clipboard_event(event: &ClipboardEvent) {
    use base64::Engine as _;
    shared_clipboard::note_applied(event);
    let payload = match event {
        ClipboardEvent::Text(text) => text.as_bytes().to_vec(),
        ClipboardEvent::ImagePng { base64 } => {
            match base64::engine::general_purpose::STANDARD.decode(base64) {
                Ok(bytes) => bytes,
                Err(err) => {
                    warn!(?err, "failed to decode clipboard image from peer");
                    return;
                }
            }
        }
    };
    let mut command = std::process::Command::new("wl-copy");
    if matches!(event, ClipboardEvent::ImagePng { .. }) {
        command.args(["-t", "image/png"]);
    }
    let spawned = command
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            warn!(?err, "failed to spawn wl-copy");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take()
        && let Err(err) = stdin.write_all(&payload)
    {
        warn!(?err, "failed to write clipboard payload to wl-copy");
        return;
    }
    if let Err(err) = child.wait() {
        warn!(?err, "wl-copy failed");
    }
}

// ---------------------------------------------------------------------------
// evdev key/button translation
// ---------------------------------------------------------------------------

mod dev_key {
    pub const KEY_A: u16 = 30;
    pub const KEY_Z: u16 = 44;
    pub const KEY_ENTER: u16 = 28;
    pub const BTN_LEFT: u16 = 0x110;
}

fn is_backslash_code(code: u16) -> bool {
    matches!(code, 43 | 86)
}

fn mouse_button_for_code(code: u16) -> Option<MouseButton> {
    match code {
        0x110 => Some(MouseButton::Left),
        0x111 => Some(MouseButton::Right),
        0x112 => Some(MouseButton::Middle),
        0x113 => Some(MouseButton::Back),
        0x114 => Some(MouseButton::Forward),
        _ => None,
    }
}

fn modifier_for_code(code: u16) -> Option<Modifier> {
    match code {
        29 | 97 => Some(Modifier::Control),
        56 | 100 => Some(Modifier::Alt),
        125 | 126 => Some(Modifier::Super),
        42 | 54 => Some(Modifier::Shift),
        _ => None,
    }
}

fn key_for_code(code: u16) -> KeyCode {
    match code {
        43 => KeyCode::Backslash,
        1 => KeyCode::Escape,
        57 => KeyCode::Space,
        28 => KeyCode::Enter,
        15 => KeyCode::Tab,
        _ => match usb_usage_for_code(code) {
            Some(usage) => KeyCode::Usb(usage),
            None => KeyCode::Other(u32::from(code)),
        },
    }
}

/// Linux evdev key code -> USB HID usage ID (the protocol's key currency,
/// same table the Windows host produces from vkeys).
fn usb_usage_for_code(code: u16) -> Option<u16> {
    match code {
        1 => Some(0x29),                       // Escape
        2..=11 => Some(0x1e + (code - 2)),     // 1..0
        12 => Some(0x2d),                      // -
        13 => Some(0x2e),                      // =
        14 => Some(0x2a),                      // Backspace
        15 => Some(0x2b),                      // Tab
        16 => Some(0x14),                      // Q
        17 => Some(0x1a),                      // W
        18 => Some(0x08),                      // E
        19 => Some(0x15),                      // R
        20 => Some(0x17),                      // T
        21 => Some(0x1c),                      // Y
        22 => Some(0x18),                      // U
        23 => Some(0x0c),                      // I
        24 => Some(0x12),                      // O
        25 => Some(0x13),                      // P
        26 => Some(0x2f),                      // [
        27 => Some(0x30),                      // ]
        28 => Some(0x28),                      // Enter
        30 => Some(0x04),                      // A
        31 => Some(0x16),                      // S
        32 => Some(0x07),                      // D
        33 => Some(0x09),                      // F
        34 => Some(0x0a),                      // G
        35 => Some(0x0b),                      // H
        36 => Some(0x0d),                      // J
        37 => Some(0x0e),                      // K
        38 => Some(0x0f),                      // L
        39 => Some(0x33),                      // ;
        40 => Some(0x34),                      // '
        41 => Some(0x35),                      // `
        43 => Some(0x31),                      // backslash
        44 => Some(0x1d),                      // Z
        45 => Some(0x1b),                      // X
        46 => Some(0x06),                      // C
        47 => Some(0x19),                      // V
        48 => Some(0x05),                      // B
        49 => Some(0x11),                      // N
        50 => Some(0x10),                      // M
        51 => Some(0x36),                      // ,
        52 => Some(0x37),                      // .
        53 => Some(0x38),                      // /
        55 => Some(0x55),                      // KP *
        57 => Some(0x2c),                      // Space
        58 => Some(0x39),                      // Caps lock
        59..=68 => Some(0x3a + (code - 59)),   // F1..F10
        69 => Some(0x53),                      // Num lock
        70 => Some(0x47),                      // Scroll lock
        71 => Some(0x5f),                      // KP 7
        72 => Some(0x60),                      // KP 8
        73 => Some(0x61),                      // KP 9
        74 => Some(0x56),                      // KP -
        75 => Some(0x5c),                      // KP 4
        76 => Some(0x5d),                      // KP 5
        77 => Some(0x5e),                      // KP 6
        78 => Some(0x57),                      // KP +
        79 => Some(0x59),                      // KP 1
        80 => Some(0x5a),                      // KP 2
        81 => Some(0x5b),                      // KP 3
        82 => Some(0x62),                      // KP 0
        83 => Some(0x63),                      // KP .
        86 => Some(0x64),                      // Non-US \ and |
        87 => Some(0x44),                      // F11
        88 => Some(0x45),                      // F12
        96 => Some(0x58),                      // KP Enter
        98 => Some(0x54),                      // KP /
        99 => Some(0x46),                      // Print Screen
        102 => Some(0x4a),                     // Home
        103 => Some(0x52),                     // Up
        104 => Some(0x4b),                     // Page up
        105 => Some(0x50),                     // Left
        106 => Some(0x4f),                     // Right
        107 => Some(0x4d),                     // End
        108 => Some(0x51),                     // Down
        109 => Some(0x4e),                     // Page down
        110 => Some(0x49),                     // Insert
        111 => Some(0x4c),                     // Delete
        117 => Some(0x67),                     // KP =
        119 => Some(0x48),                     // Pause
        127 => Some(0x65),                     // Compose/Application
        183..=194 => Some(0x68 + (code - 183)), // F13..F24
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evdev_letters_map_to_usb_usage() {
        assert_eq!(usb_usage_for_code(30), Some(0x04)); // A
        assert_eq!(usb_usage_for_code(44), Some(0x1d)); // Z
        assert_eq!(usb_usage_for_code(16), Some(0x14)); // Q
        assert_eq!(usb_usage_for_code(25), Some(0x13)); // P
        assert_eq!(usb_usage_for_code(50), Some(0x10)); // M
    }

    #[test]
    fn evdev_digits_and_function_keys_map_to_usb_usage() {
        assert_eq!(usb_usage_for_code(2), Some(0x1e)); // 1
        assert_eq!(usb_usage_for_code(11), Some(0x27)); // 0
        assert_eq!(usb_usage_for_code(59), Some(0x3a)); // F1
        assert_eq!(usb_usage_for_code(68), Some(0x43)); // F10
        assert_eq!(usb_usage_for_code(87), Some(0x44)); // F11
        assert_eq!(usb_usage_for_code(194), Some(0x73)); // F24
    }

    #[test]
    fn key_for_code_uses_dedicated_variants_for_hotkey_keys() {
        assert_eq!(key_for_code(43), KeyCode::Backslash);
        assert_eq!(key_for_code(1), KeyCode::Escape);
        assert_eq!(key_for_code(57), KeyCode::Space);
        assert_eq!(key_for_code(28), KeyCode::Enter);
        assert_eq!(key_for_code(15), KeyCode::Tab);
        assert_eq!(key_for_code(30), KeyCode::Usb(0x04));
        assert_eq!(key_for_code(113), KeyCode::Other(113)); // mute: unmapped
    }

    #[test]
    fn modifiers_map_left_and_right_variants() {
        assert_eq!(modifier_for_code(29), Some(Modifier::Control));
        assert_eq!(modifier_for_code(97), Some(Modifier::Control));
        assert_eq!(modifier_for_code(56), Some(Modifier::Alt));
        assert_eq!(modifier_for_code(100), Some(Modifier::Alt));
        assert_eq!(modifier_for_code(125), Some(Modifier::Super));
        assert_eq!(modifier_for_code(42), Some(Modifier::Shift));
        assert_eq!(modifier_for_code(30), None);
    }

    #[test]
    fn cursorpos_parses_hyprland_reply() {
        assert_eq!(parse_cursorpos("674, 814"), Some((674.0, 814.0)));
        assert_eq!(parse_cursorpos("0, 1439\n"), Some((0.0, 1439.0)));
        assert_eq!(parse_cursorpos("garbage"), None);
        assert_eq!(parse_cursorpos(""), None);
    }

    #[test]
    fn monitors_parse_and_apply_scale() {
        let json = r#"[{"x":0,"y":0,"width":2560,"height":1440,"scale":1.00},
                        {"x":-1920,"y":0,"width":1920,"height":1080,"scale":1.5}]"#;
        let monitors = parse_monitors(json).expect("monitors");
        assert_eq!(monitors[0].width, 2560.0);
        assert_eq!(monitors[1].x, -1920.0);
        assert_eq!(monitors[1].width, 1280.0);
        assert_eq!(monitors[1].height, 720.0);
    }

    #[test]
    fn restore_point_stays_off_the_left_edge() {
        let monitor = Monitor {
            x: 0.0,
            y: 0.0,
            width: 2560.0,
            height: 1440.0,
        };
        let point = clamp_restore_point_inside_left_edge(monitor, (0.0, 700.0));
        assert!(point.0 >= EDGE_TRIGGER_PX + LINUX_RESTORE_EDGE_INSET_PX - 1.0);
        assert_eq!(point.1, 700.0);

        let from_ratios = point_from_rect_ratios(monitor, 0.02, 0.5);
        let clamped = clamp_restore_point_inside_left_edge(monitor, from_ratios);
        assert!(clamped.0 >= EDGE_TRIGGER_PX + LINUX_RESTORE_EDGE_INSET_PX - 1.0);
    }
}
