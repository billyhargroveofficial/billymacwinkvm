mod cli;
mod hid;
mod latency;
mod platform;
mod protocol;
mod transport;

use anyhow::{Context, Result, bail};
use clap::Parser;
use cli::{Cli, Command, SinkKind};
use hid::{HidSink, KarabinerProbe, LogSink};
use protocol::{
    ClientControlEvent, Frame, InputEvent, KeyCode, KeyState, Message, Modifier, MouseButton,
    ProtocolHello,
};
use std::collections::HashSet;
use std::time::Instant;
use tokio::net::{
    TcpStream,
    tcp::{OwnedReadHalf, OwnedWriteHalf},
};
use tokio::sync::mpsc;
use tokio::time::{Duration, MissedTickBehavior, interval, sleep};
use tracing::{info, trace, warn};
use uuid::Uuid;

const MAC_MOTION_FLUSH_INTERVAL_MS: u64 = 4;
const MAC_RIGHT_EDGE_RELEASE_REARM_PX: f64 = 48.0;
#[cfg(target_os = "macos")]
const MAC_EDGE_ENTRY_GAP_PX: f64 = 2.0;
const MAX_CLIENT_MOTION_DRAIN_PER_TURN: usize = 64;
const MAX_MOTION_DELTA_PER_FLUSH: i32 = 512;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    install_signal_guards();

    let cli = Cli::parse();

    match cli.command {
        Command::GenPsk => gen_psk(),
        Command::MacHidProbe => mac_hid_probe(),
        Command::MacHidSmoke => mac_hid_smoke().await,
        Command::MacKeySmoke => mac_key_smoke().await,
        Command::Client { listen, sink } => run_client(listen, sink).await,
        Command::Probe { peer } => run_probe(peer).await,
        Command::Host { peer, layout } => run_host(peer, layout).await,
    }
}

fn gen_psk() -> Result<()> {
    let bytes: [u8; 32] = rand::random();
    println!("{}", hex::encode(bytes));
    Ok(())
}

fn mac_hid_probe() -> Result<()> {
    let probe = KarabinerProbe::detect();
    println!("{}", serde_json::to_string_pretty(&probe)?);

    if !probe.ready {
        bail!(
            "Karabiner VirtualHID is not ready. Install/approve the system extension, then rerun this probe."
        );
    }

    Ok(())
}

async fn mac_hid_smoke() -> Result<()> {
    let mut sink = hid::karabiner_sink_async().await?;
    for _ in 0..30 {
        sink.apply(InputEvent::MouseMotion { dx: 8, dy: 0 }).await?;
        sleep(Duration::from_millis(5)).await;
    }
    for _ in 0..30 {
        sink.apply(InputEvent::MouseMotion { dx: -8, dy: 0 })
            .await?;
        sleep(Duration::from_millis(5)).await;
    }
    sink.reset().await?;
    Ok(())
}

async fn mac_key_smoke() -> Result<()> {
    let mut sink = hid::karabiner_sink_async().await?;
    info!("typing five synthetic 'a' key presses through Karabiner VirtualHID");
    for _ in 0..5 {
        sink.apply(InputEvent::Key {
            key: KeyCode::Usb(0x04),
            state: KeyState::Down,
        })
        .await?;
        sleep(Duration::from_millis(90)).await;
        sink.apply(InputEvent::Key {
            key: KeyCode::Usb(0x04),
            state: KeyState::Up,
        })
        .await?;
        sleep(Duration::from_millis(70)).await;
    }
    sink.reset().await?;
    Ok(())
}

async fn run_client(listen: String, sink: SinkKind) -> Result<()> {
    let mut hid_sink = make_sink(sink).await?;

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    info!(%listen, "softkvm client listening");

    loop {
        let (stream, addr) = listener.accept().await?;
        info!(%addr, "accepted host");
        if let Err(err) = stream.set_nodelay(true) {
            warn!(?err, "failed to set TCP_NODELAY on accepted host stream");
        }

        let (read_half, write_half) = stream.into_split();
        let mut writer = transport::FrameWriter::new(write_half);
        let (frame_tx, mut frame_rx) = mpsc::unbounded_channel();
        tokio::spawn(client_reader_task(read_half, frame_tx));
        let mut client_state = ClientRuntimeState::default();
        let client_session_id = Uuid::new_v4();
        let mut client_seq = 1_u64;
        let mut saw_frame = false;
        let mut touched_input = false;
        let mut pending_motion = PendingMotion::default();
        let mut flush_timer = interval(Duration::from_millis(MAC_MOTION_FLUSH_INTERVAL_MS));
        flush_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut read_error = None;
        let mut deferred_read_event = None;

        loop {
            let event = if let Some(event) = deferred_read_event.take() {
                Some(event)
            } else {
                tokio::select! {
                    biased;

                    _ = flush_timer.tick(), if pending_motion.has_motion() => {
                        let _ = flush_client_motion(
                            &mut hid_sink,
                            sink,
                            &mut writer,
                            client_session_id,
                            &mut client_seq,
                            &mut client_state,
                            &mut pending_motion,
                        ).await;
                        flush_timer.reset();
                        continue;
                    }
                    event = frame_rx.recv() => event,
                }
            };

            let Some(event) = event else {
                break;
            };

            let (frame, read_at) = match event {
                ClientReadEvent::Frame { frame, read_at } => (frame, read_at),
                ClientReadEvent::Disconnected => break,
                ClientReadEvent::Failed(err) => {
                    read_error = Some(err);
                    break;
                }
            };
            let queue_elapsed = read_at.elapsed();
            if latency::report(queue_elapsed) {
                let level = if latency::slow(queue_elapsed) {
                    "slow"
                } else {
                    "trace"
                };
                info!(
                    target: "softkvm::latency",
                    level,
                    message = frame.message.label(),
                    seq = frame.seq,
                    queue_ms = latency::ms(queue_elapsed),
                    "mac frame queue latency"
                );
            }

            saw_frame = true;
            match frame.message {
                Message::Hello(hello) => {
                    info!(?hello, "host hello");
                }
                Message::HostState(state) => {
                    pending_motion.clear();
                    info!(?state, "host state");
                    client_state.set_remote_active(state.remote_active);
                    if state.remote_active {
                        position_mac_cursor(state.entry_x_ratio, state.entry_y_ratio);
                    } else {
                        reset_input(&mut hid_sink, sink).await;
                    }
                }
                Message::ClientControl(control) => {
                    info!(?control, "client control from peer ignored");
                }
                Message::Input(event) => {
                    touched_input = true;

                    if !client_state.remote_active {
                        continue;
                    }

                    if let InputEvent::MouseMotion { dx, dy } = event {
                        pending_motion.add(dx, dy);
                        drain_queued_motion(
                            &mut frame_rx,
                            &mut pending_motion,
                            &mut deferred_read_event,
                        )
                        .inspect(|drained| {
                            if latency::enabled() || *drained >= MAX_CLIENT_MOTION_DRAIN_PER_TURN {
                                info!(
                                    target: "softkvm::latency",
                                    drained,
                                    pending_dx = pending_motion.dx,
                                    pending_dy = pending_motion.dy,
                                    frame_queue_len = frame_rx.len(),
                                    "mac drained queued motion"
                                );
                            }
                        });
                        if client_state.take_just_activated() {
                            let started = Instant::now();
                            let _ = flush_client_motion(
                                &mut hid_sink,
                                sink,
                                &mut writer,
                                client_session_id,
                                &mut client_seq,
                                &mut client_state,
                                &mut pending_motion,
                            )
                            .await;
                            let elapsed = started.elapsed();
                            if latency::report(elapsed) {
                                info!(
                                    target: "softkvm::latency",
                                    elapsed_ms = latency::ms(elapsed),
                                    "mac first motion flush after activation"
                                );
                            }
                            flush_timer.reset();
                        }
                        continue;
                    }

                    if flush_client_motion(
                        &mut hid_sink,
                        sink,
                        &mut writer,
                        client_session_id,
                        &mut client_seq,
                        &mut client_state,
                        &mut pending_motion,
                    )
                    .await
                    {
                        continue;
                    }

                    if client_state.should_release_for_hotkey(&event) {
                        request_host_release(
                            &mut hid_sink,
                            sink,
                            &mut writer,
                            client_session_id,
                            &mut client_seq,
                            &mut client_state,
                            "remote hotkey Ctrl+Alt+\\",
                            Some(0.5),
                            Some(0.5),
                        )
                        .await;
                        continue;
                    }

                    apply_input(&mut hid_sink, sink, event).await;
                }
                Message::InputReset => {
                    touched_input = true;
                    pending_motion.clear();
                    client_state.reset_keys();
                    reset_input(&mut hid_sink, sink).await;
                }
                Message::Heartbeat { monotonic_ms } => {
                    trace!(monotonic_ms, "heartbeat");
                }
            }
        }

        flush_client_motion(
            &mut hid_sink,
            sink,
            &mut writer,
            client_session_id,
            &mut client_seq,
            &mut client_state,
            &mut pending_motion,
        )
        .await;

        if saw_frame {
            warn!(%addr, "host disconnected");
        } else {
            info!(%addr, "empty tcp probe disconnected");
        }
        if let Some(err) = read_error {
            warn!(?err, "host read loop failed");
        }

        if touched_input {
            reset_input(&mut hid_sink, sink).await;
        }
    }
}

enum ClientReadEvent {
    Frame { frame: Frame, read_at: Instant },
    Disconnected,
    Failed(anyhow::Error),
}

async fn client_reader_task(read_half: OwnedReadHalf, tx: mpsc::UnboundedSender<ClientReadEvent>) {
    let mut reader = transport::FrameReader::new(read_half);
    loop {
        match reader.read_frame().await {
            Ok(Some(frame)) => {
                if tx
                    .send(ClientReadEvent::Frame {
                        frame,
                        read_at: Instant::now(),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(None) => {
                let _ = tx.send(ClientReadEvent::Disconnected);
                break;
            }
            Err(err) => {
                let _ = tx.send(ClientReadEvent::Failed(err));
                break;
            }
        }
    }
}

fn drain_queued_motion(
    frame_rx: &mut mpsc::UnboundedReceiver<ClientReadEvent>,
    pending_motion: &mut PendingMotion,
    deferred_read_event: &mut Option<ClientReadEvent>,
) -> Option<usize> {
    let mut drained = 0_usize;
    for _ in 0..MAX_CLIENT_MOTION_DRAIN_PER_TURN {
        let Ok(event) = frame_rx.try_recv() else {
            break;
        };
        match event {
            ClientReadEvent::Frame {
                frame:
                    Frame {
                        message: Message::Input(InputEvent::MouseMotion { dx, dy }),
                        ..
                    },
                ..
            } => {
                pending_motion.add(dx, dy);
                drained += 1;
            }
            other => {
                *deferred_read_event = Some(other);
                break;
            }
        }
    }
    (drained > 0).then_some(drained)
}

#[derive(Default)]
struct PendingMotion {
    dx: i32,
    dy: i32,
}

impl PendingMotion {
    fn add(&mut self, dx: i32, dy: i32) {
        self.dx = self.dx.saturating_add(dx);
        self.dy = self.dy.saturating_add(dy);
    }

    fn clear(&mut self) {
        self.dx = 0;
        self.dy = 0;
    }

    fn has_motion(&self) -> bool {
        self.dx != 0 || self.dy != 0
    }

    fn take(&mut self) -> Option<(i32, i32)> {
        if !self.has_motion() {
            return None;
        }
        Some((
            std::mem::take(&mut self.dx)
                .clamp(-MAX_MOTION_DELTA_PER_FLUSH, MAX_MOTION_DELTA_PER_FLUSH),
            std::mem::take(&mut self.dy)
                .clamp(-MAX_MOTION_DELTA_PER_FLUSH, MAX_MOTION_DELTA_PER_FLUSH),
        ))
    }
}

async fn flush_client_motion(
    hid_sink: &mut Box<dyn HidSink>,
    sink: SinkKind,
    writer: &mut transport::FrameWriter<OwnedWriteHalf>,
    client_session_id: Uuid,
    client_seq: &mut u64,
    client_state: &mut ClientRuntimeState,
    pending_motion: &mut PendingMotion,
) -> bool {
    let Some((dx, dy)) = pending_motion.take() else {
        return false;
    };
    if !client_state.remote_active {
        return false;
    }

    if client_state.should_release_before_motion(dx) && mac_cursor_at_right_edge() {
        let (_, y_ratio) = mac_cursor_ratios().unwrap_or((0.0, 0.5));
        request_host_release(
            hid_sink,
            sink,
            writer,
            client_session_id,
            client_seq,
            client_state,
            "mac right edge",
            Some(0.02),
            Some(y_ratio),
        )
        .await;
        return true;
    }

    apply_input(hid_sink, sink, InputEvent::MouseMotion { dx, dy }).await;
    client_state.update_right_edge_release_arm(dx);

    if client_state.should_release_after_motion(dx) && mac_cursor_at_right_edge() {
        let (_, y_ratio) = mac_cursor_ratios().unwrap_or((0.0, 0.5));
        request_host_release(
            hid_sink,
            sink,
            writer,
            client_session_id,
            client_seq,
            client_state,
            "mac right edge",
            Some(0.02),
            Some(y_ratio),
        )
        .await;
        return true;
    }

    false
}

async fn request_host_release(
    hid_sink: &mut Box<dyn HidSink>,
    sink: SinkKind,
    writer: &mut transport::FrameWriter<OwnedWriteHalf>,
    client_session_id: Uuid,
    client_seq: &mut u64,
    client_state: &mut ClientRuntimeState,
    reason: &'static str,
    entry_x_ratio: Option<f64>,
    entry_y_ratio: Option<f64>,
) {
    client_state.set_remote_active(false);

    if let Err(err) = writer
        .write_frame(Frame::new(
            client_session_id,
            *client_seq,
            Message::ClientControl(ClientControlEvent::ReleaseHost {
                reason: reason.to_owned(),
                entry_x_ratio,
                entry_y_ratio,
            }),
        ))
        .await
    {
        warn!(?err, reason, "failed to request host release");
    } else {
        *client_seq += 1;
        info!(reason, "requested Windows host release");
    }

    reset_input(hid_sink, sink).await;
}

#[derive(Default)]
struct ClientRuntimeState {
    remote_active: bool,
    active_modifiers: HashSet<Modifier>,
    right_edge_release_armed: bool,
    just_activated: bool,
}

impl ClientRuntimeState {
    fn set_remote_active(&mut self, active: bool) {
        self.remote_active = active;
        self.reset_keys();
        self.right_edge_release_armed = false;
        self.just_activated = active;
    }

    fn reset_keys(&mut self) {
        self.active_modifiers.clear();
    }

    fn should_release_for_hotkey(&mut self, event: &InputEvent) -> bool {
        match event {
            InputEvent::Modifier { modifier, state } => {
                match state {
                    KeyState::Down => {
                        self.active_modifiers.insert(*modifier);
                    }
                    KeyState::Up => {
                        self.active_modifiers.remove(modifier);
                    }
                }
                false
            }
            InputEvent::Key {
                key: KeyCode::Backslash,
                state: KeyState::Down,
            } if self.remote_active => {
                self.active_modifiers.contains(&Modifier::Control)
                    && (self.active_modifiers.contains(&Modifier::Alt)
                        || self.active_modifiers.contains(&Modifier::Super))
            }
            _ => false,
        }
    }

    fn update_right_edge_release_arm(&mut self, dx: i32) {
        if !self.remote_active || self.right_edge_release_armed || dx >= 0 {
            return;
        }
        if mac_cursor_right_gap().is_some_and(|gap| gap >= MAC_RIGHT_EDGE_RELEASE_REARM_PX) {
            self.right_edge_release_armed = true;
        }
    }

    fn should_release_before_motion(&self, dx: i32) -> bool {
        self.remote_active && self.right_edge_release_armed && dx > 0
    }

    fn should_release_after_motion(&self, dx: i32) -> bool {
        self.remote_active && self.right_edge_release_armed && dx > 0
    }

    fn take_just_activated(&mut self) -> bool {
        std::mem::take(&mut self.just_activated)
    }
}

async fn make_sink(sink: SinkKind) -> Result<Box<dyn HidSink>> {
    match sink {
        SinkKind::Log => Ok(Box::new(LogSink::default())),
        SinkKind::Karabiner => hid::karabiner_sink_async().await,
    }
}

#[cfg(target_os = "macos")]
fn mac_cursor_at_right_edge() -> bool {
    const EDGE_TRIGGER_PX: f64 = 8.0;

    match unsafe { mac_cursor_and_main_display_bounds() } {
        Some((cursor, bounds)) => cursor.x >= bounds.origin.x + bounds.size.width - EDGE_TRIGGER_PX,
        None => false,
    }
}

#[cfg(target_os = "macos")]
fn mac_cursor_right_gap() -> Option<f64> {
    let (cursor, bounds) = unsafe { mac_cursor_and_main_display_bounds()? };
    Some((bounds.origin.x + bounds.size.width - cursor.x).max(0.0))
}

#[cfg(target_os = "macos")]
fn mac_cursor_ratios() -> Option<(f64, f64)> {
    let (cursor, bounds) = unsafe { mac_cursor_and_main_display_bounds()? };
    let x = ((cursor.x - bounds.origin.x) / bounds.size.width).clamp(0.0, 1.0);
    let y = ((cursor.y - bounds.origin.y) / bounds.size.height).clamp(0.0, 1.0);
    Some((x, y))
}

#[cfg(not(target_os = "macos"))]
fn mac_cursor_ratios() -> Option<(f64, f64)> {
    None
}

#[cfg(target_os = "macos")]
fn position_mac_cursor(x_ratio: Option<f64>, y_ratio: Option<f64>) {
    let Some((x_ratio, y_ratio)) = x_ratio.zip(y_ratio) else {
        return;
    };
    let bounds = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    let x_ratio = x_ratio.clamp(0.0, 1.0);
    let y_ratio = y_ratio.clamp(0.0, 1.0);
    let x = if x_ratio >= 0.999 {
        bounds.origin.x + bounds.size.width - MAC_EDGE_ENTRY_GAP_PX
    } else if x_ratio <= 0.001 {
        bounds.origin.x + MAC_EDGE_ENTRY_GAP_PX
    } else {
        bounds.origin.x + bounds.size.width * x_ratio
    };
    let y = (bounds.origin.y + bounds.size.height * y_ratio).clamp(
        bounds.origin.y + MAC_EDGE_ENTRY_GAP_PX,
        bounds.origin.y + bounds.size.height - MAC_EDGE_ENTRY_GAP_PX,
    );
    let started = Instant::now();
    unsafe { CGWarpMouseCursorPosition(CGPoint { x, y }) };
    let elapsed = started.elapsed();
    if latency::report(elapsed) {
        info!(
            target: "softkvm::latency",
            x,
            y,
            elapsed_ms = latency::ms(elapsed),
            "mac cursor warp"
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn position_mac_cursor(_x_ratio: Option<f64>, _y_ratio: Option<f64>) {}

#[cfg(not(target_os = "macos"))]
fn mac_cursor_at_right_edge() -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
fn mac_cursor_right_gap() -> Option<f64> {
    None
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGEventCreate(source: *const std::ffi::c_void) -> *mut std::ffi::c_void;
    fn CGEventGetLocation(event: *mut std::ffi::c_void) -> CGPoint;
    fn CGWarpMouseCursorPosition(new_cursor_position: CGPoint) -> i32;
    fn CFRelease(cf: *const std::ffi::c_void);
}

#[cfg(target_os = "macos")]
unsafe fn mac_cursor_and_main_display_bounds() -> Option<(CGPoint, CGRect)> {
    let event = unsafe { CGEventCreate(std::ptr::null()) };
    if event.is_null() {
        return None;
    }
    let cursor = unsafe { CGEventGetLocation(event) };
    unsafe { CFRelease(event.cast_const()) };
    let bounds = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    Some((cursor, bounds))
}

async fn apply_input(hid_sink: &mut Box<dyn HidSink>, sink: SinkKind, event: InputEvent) {
    let started = Instant::now();
    let event_label = event.label();
    if let Err(err) = hid_sink.apply(event.clone()).await {
        warn!(?err, "input sink failed; reconnecting");
        match make_sink(sink).await {
            Ok(mut replacement) => {
                if let Err(retry_err) = replacement.apply(event).await {
                    warn!(?retry_err, "input sink retry failed");
                }
                *hid_sink = replacement;
            }
            Err(reconnect_err) => {
                warn!(?reconnect_err, "input sink reconnect failed");
            }
        }
    } else {
        let elapsed = started.elapsed();
        if latency::report(elapsed) {
            info!(
                target: "softkvm::latency",
                elapsed_ms = elapsed.as_millis(),
                event = event_label,
                "mac input sink apply latency"
            );
        }
    }
}

async fn reset_input(hid_sink: &mut Box<dyn HidSink>, sink: SinkKind) {
    if let Err(err) = hid_sink.reset().await {
        warn!(?err, "input sink reset failed; reconnecting");
        match make_sink(sink).await {
            Ok(mut replacement) => {
                if let Err(retry_err) = replacement.reset().await {
                    warn!(?retry_err, "input sink reset retry failed");
                }
                *hid_sink = replacement;
            }
            Err(reconnect_err) => {
                warn!(?reconnect_err, "input sink reconnect failed");
            }
        }
    }
}

async fn run_probe(peer: String) -> Result<()> {
    let stream = TcpStream::connect(&peer)
        .await
        .with_context(|| format!("connect {peer}"))?;
    stream.set_nodelay(true).context("set TCP_NODELAY")?;
    let mut writer = transport::FrameWriter::new(stream);
    let session_id = Uuid::new_v4();

    writer
        .write_frame(Frame::new(
            session_id,
            1,
            Message::Hello(ProtocolHello {
                device_name: hostname(),
                role: "probe".to_owned(),
                protocol_version: protocol::PROTOCOL_VERSION,
            }),
        ))
        .await?;

    for i in 0..240_u32 {
        let dx = if i < 120 { 4 } else { -4 };
        writer
            .write_frame(Frame::new(
                session_id,
                2 + u64::from(i),
                Message::Input(InputEvent::MouseMotion { dx, dy: 0 }),
            ))
            .await?;
        tokio::task::yield_now().await;
    }

    writer
        .write_frame(Frame::new(
            session_id,
            300,
            Message::Input(InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Down,
            }),
        ))
        .await?;
    writer
        .write_frame(Frame::new(
            session_id,
            301,
            Message::Input(InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Up,
            }),
        ))
        .await?;
    writer
        .write_frame(Frame::new(session_id, 302, Message::InputReset))
        .await?;

    Ok(())
}

async fn run_host(_peer: String, _layout: String) -> Result<()> {
    #[cfg(windows)]
    {
        platform::windows::run_host(_peer, _layout).await
    }

    #[cfg(not(windows))]
    {
        bail!(
            "host capture is Windows-only for the first MVP; use `probe` from macOS for client testing"
        )
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "softkvm".to_owned())
}

#[cfg(unix)]
fn install_signal_guards() {
    tokio::spawn(async {
        let Ok(mut sigquit) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit())
        else {
            return;
        };
        while sigquit.recv().await.is_some() {
            warn!("ignored SIGQUIT; remote Ctrl+\\ should be handled as a KVM hotkey");
        }
    });
}

#[cfg(not(unix))]
fn install_signal_guards() {}
