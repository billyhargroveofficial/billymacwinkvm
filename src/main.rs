mod cli;
mod hid;
mod latency;
mod platform;
mod protocol;
mod transport;

use anyhow::{Context, Result, bail};
use clap::Parser;
use cli::{BenchTiming, BenchTransport, Cli, Command, SinkKind};
use hid::{HidSink, KarabinerProbe, LogSink};
use protocol::{
    ClientControlEvent, Frame, InputEvent, KeyCode, KeyState, MOTION_DATAGRAM_LEN, Message,
    Modifier, MotionDatagram, MouseButton, ProtocolHello,
};
use std::collections::HashSet;
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::time::Instant;
use tokio::net::{
    TcpStream, UdpSocket,
    tcp::{OwnedReadHalf, OwnedWriteHalf},
};
use tokio::sync::mpsc;
use tokio::time::{Duration, MissedTickBehavior, interval, sleep};
use tracing::{info, trace, warn};
use uuid::Uuid;

const MAC_RIGHT_EDGE_RELEASE_REARM_PX: f64 = 48.0;
#[cfg(target_os = "macos")]
const MAC_EDGE_ENTRY_GAP_PX: f64 = 2.0;
const MAX_MOTION_DELTA_PER_FLUSH: i32 = 512;
const DEFAULT_MAC_MOTION_FLUSH_INTERVAL_MS: u64 = 2;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    install_signal_guards();

    let cli = Cli::parse();

    #[cfg(windows)]
    platform::windows::prepare_low_latency_process();

    match cli.command {
        Command::BuildInfo => build_info(),
        Command::GenPsk => gen_psk(),
        Command::MacHidProbe => mac_hid_probe(),
        Command::MacNativeHidProbe => mac_native_hid_probe().await,
        Command::MacHidSmoke => mac_hid_smoke().await,
        Command::MacKeySmoke => mac_key_smoke().await,
        Command::Client { listen, sink } => run_client(listen, sink).await,
        Command::Probe { peer } => run_probe(peer).await,
        Command::MotionBench {
            peer,
            transport,
            timing,
            hz,
            seconds,
            dx,
        } => run_motion_bench(peer, transport, timing, hz, seconds, dx).await,
        Command::Host { peer, layout } => run_host(peer, layout).await,
    }
}

fn build_info() -> Result<()> {
    println!("softkvm {}", env!("CARGO_PKG_VERSION"));
    println!("build_git_hash {}", env!("SOFTKVM_BUILD_GIT_HASH"));
    println!("protocol_version {}", protocol::PROTOCOL_VERSION);
    println!("motion_transport udp-binary-skm1-with-tcp-fallback");
    println!(
        "mac_motion_flush default_ms={} env=SOFTKVM_MAC_MOTION_FLUSH_MS",
        DEFAULT_MAC_MOTION_FLUSH_INTERVAL_MS
    );
    Ok(())
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

async fn mac_native_hid_probe() -> Result<()> {
    let mut sink = hid::native_hid_sink_async().await?;
    info!("native macOS virtual HID opened; sending no-click smoke movement");
    for _ in 0..12 {
        sink.apply(InputEvent::MouseMotion { dx: 6, dy: 0 }).await?;
        sleep(Duration::from_millis(4)).await;
    }
    for _ in 0..12 {
        sink.apply(InputEvent::MouseMotion { dx: -6, dy: 0 })
            .await?;
        sleep(Duration::from_millis(4)).await;
    }
    sink.reset().await?;
    println!("native_hid_ready true");
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
    let udp_socket = UdpSocket::bind(&listen)
        .await
        .with_context(|| format!("bind udp motion {listen}"))?;
    let (motion_tx, mut motion_rx) = mpsc::unbounded_channel();
    tokio::spawn(udp_motion_reader_task(udp_socket, motion_tx));
    info!(%listen, "softkvm client listening");
    info!(%listen, "softkvm udp motion listening");

    loop {
        let (stream, addr) = listener.accept().await?;
        info!(%addr, "accepted host");
        if let Err(err) = stream.set_nodelay(true) {
            warn!(?err, "failed to set TCP_NODELAY on accepted host stream");
        }
        while motion_rx.try_recv().is_ok() {}

        let (read_half, write_half) = stream.into_split();
        let mut writer = transport::FrameWriter::new(write_half);
        let (frame_tx, mut frame_rx) = mpsc::unbounded_channel();
        tokio::spawn(client_reader_task(read_half, frame_tx));
        let mut client_state = ClientRuntimeState::default();
        let client_session_id = Uuid::new_v4();
        let mut client_seq = 1_u64;
        let mut saw_frame = false;
        let mut touched_input = false;
        let mut read_error = None;
        let mut motion_flush_timer = interval(mac_motion_flush_interval());
        motion_flush_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            let event = {
                tokio::select! {
                    biased;

                    _ = motion_flush_timer.tick(), if client_state.has_pending_motion() => {
                        touched_input = true;
                        flush_client_motion(
                            &mut hid_sink,
                            sink,
                            &mut writer,
                            client_session_id,
                            &mut client_seq,
                            &mut client_state,
                        ).await;
                        continue;
                    }
                    event = frame_rx.recv() => {
                        let Some(event) = event else {
                            break;
                        };
                        ClientEvent::Tcp(event)
                    }
                    motion = motion_rx.recv() => {
                        let Some(motion) = motion else {
                            continue;
                        };
                        ClientEvent::Motion(motion)
                    }
                }
            };

            let (frame, read_at) = match event {
                ClientEvent::Motion(motion) => {
                    if motion.addr.ip() != addr.ip() {
                        trace!(
                            motion_addr = %motion.addr,
                            tcp_addr = %addr,
                            "ignoring udp motion from non-active host"
                        );
                        continue;
                    }

                    let queue_elapsed = motion.read_at.elapsed();
                    if latency::report(queue_elapsed) {
                        let level = if latency::slow(queue_elapsed) {
                            "slow"
                        } else {
                            "trace"
                        };
                        info!(
                            target: "softkvm::latency",
                            level,
                            seq = motion.packet.seq,
                            queue_ms = latency::ms(queue_elapsed),
                            "mac udp motion queue latency"
                        );
                    }

                    if !client_state.remote_active {
                        continue;
                    }
                    if !client_state.udp_motion_ready_sent {
                        send_udp_motion_ready(
                            &mut writer,
                            client_session_id,
                            &mut client_seq,
                            &mut client_state,
                        )
                        .await;
                    }
                    if motion.packet.dx == 0 && motion.packet.dy == 0 {
                        continue;
                    }
                    touched_input = true;
                    client_state.queue_motion(
                        motion.packet.dx,
                        motion.packet.dy,
                        "udp",
                        motion.packet.seq,
                    );
                    continue;
                }
                ClientEvent::Tcp(ClientReadEvent::Frame { frame, read_at }) => (frame, read_at),
                ClientEvent::Tcp(ClientReadEvent::Disconnected) => break,
                ClientEvent::Tcp(ClientReadEvent::Failed(err)) => {
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
            let frame_seq = frame.seq;
            match frame.message {
                Message::Hello(hello) => {
                    info!(?hello, "host hello");
                }
                Message::HostState(state) => {
                    if client_state.has_pending_motion() {
                        flush_client_motion(
                            &mut hid_sink,
                            sink,
                            &mut writer,
                            client_session_id,
                            &mut client_seq,
                            &mut client_state,
                        )
                        .await;
                    }
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
                        client_state.queue_motion(dx, dy, "tcp", frame_seq);
                        continue;
                    }

                    if client_state.has_pending_motion() {
                        flush_client_motion(
                            &mut hid_sink,
                            sink,
                            &mut writer,
                            client_session_id,
                            &mut client_seq,
                            &mut client_state,
                        )
                        .await;
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
                    client_state.clear_pending_motion();
                    client_state.reset_keys();
                    reset_input(&mut hid_sink, sink).await;
                }
                Message::Heartbeat { monotonic_ms } => {
                    trace!(monotonic_ms, "heartbeat");
                }
            }
        }

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

enum ClientEvent {
    Tcp(ClientReadEvent),
    Motion(UdpMotionEvent),
}

enum ClientReadEvent {
    Frame { frame: Frame, read_at: Instant },
    Disconnected,
    Failed(anyhow::Error),
}

struct UdpMotionEvent {
    packet: MotionDatagram,
    addr: SocketAddr,
    read_at: Instant,
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

async fn udp_motion_reader_task(socket: UdpSocket, tx: mpsc::UnboundedSender<UdpMotionEvent>) {
    let mut buffer = [0_u8; MOTION_DATAGRAM_LEN];
    loop {
        match socket.recv_from(&mut buffer).await {
            Ok((len, addr)) => {
                let read_at = Instant::now();
                let Some(packet) = MotionDatagram::decode(&buffer[..len]) else {
                    if latency::enabled() {
                        warn!(%addr, len, "ignored malformed udp motion packet");
                    }
                    continue;
                };
                if tx
                    .send(UdpMotionEvent {
                        packet,
                        addr,
                        read_at,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Err(err) => {
                warn!(?err, "udp motion reader failed");
                break;
            }
        }
    }
}

async fn handle_client_motion(
    hid_sink: &mut Box<dyn HidSink>,
    sink: SinkKind,
    writer: &mut transport::FrameWriter<OwnedWriteHalf>,
    client_session_id: Uuid,
    client_seq: &mut u64,
    client_state: &mut ClientRuntimeState,
    dx: i32,
    dy: i32,
    source: &'static str,
    source_seq: u64,
) -> bool {
    if !client_state.remote_active {
        return false;
    }
    let dx = dx.clamp(-MAX_MOTION_DELTA_PER_FLUSH, MAX_MOTION_DELTA_PER_FLUSH);
    let dy = dy.clamp(-MAX_MOTION_DELTA_PER_FLUSH, MAX_MOTION_DELTA_PER_FLUSH);

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

    let first_after_activation = client_state.take_just_activated();
    let started = Instant::now();
    apply_input(hid_sink, sink, InputEvent::MouseMotion { dx, dy }).await;
    let elapsed = started.elapsed();
    if first_after_activation || latency::report(elapsed) {
        info!(
            target: "softkvm::latency",
            source,
            source_seq,
            elapsed_ms = latency::ms(elapsed),
            first_after_activation,
            "mac motion apply latency"
        );
    }
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

async fn flush_client_motion(
    hid_sink: &mut Box<dyn HidSink>,
    sink: SinkKind,
    writer: &mut transport::FrameWriter<OwnedWriteHalf>,
    client_session_id: Uuid,
    client_seq: &mut u64,
    client_state: &mut ClientRuntimeState,
) -> bool {
    let Some(motion) = client_state.take_pending_motion() else {
        return false;
    };

    let queued_elapsed = motion.queued_at.elapsed();
    if motion.event_count > 1 || latency::report(queued_elapsed) {
        info!(
            target: "softkvm::latency",
            source = motion.source,
            source_seq = motion.source_seq,
            event_count = motion.event_count,
            dx = motion.dx,
            dy = motion.dy,
            queued_ms = latency::ms(queued_elapsed),
            "mac motion coalesced flush"
        );
    }

    handle_client_motion(
        hid_sink,
        sink,
        writer,
        client_session_id,
        client_seq,
        client_state,
        motion.dx,
        motion.dy,
        motion.source,
        motion.source_seq,
    )
    .await
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

async fn send_udp_motion_ready(
    writer: &mut transport::FrameWriter<OwnedWriteHalf>,
    client_session_id: Uuid,
    client_seq: &mut u64,
    client_state: &mut ClientRuntimeState,
) {
    if client_state.udp_motion_ready_sent {
        return;
    }
    if let Err(err) = writer
        .write_frame(Frame::new(
            client_session_id,
            *client_seq,
            Message::ClientControl(ClientControlEvent::UdpMotionReady),
        ))
        .await
    {
        warn!(?err, "failed to acknowledge udp motion");
        return;
    }

    *client_seq += 1;
    client_state.udp_motion_ready_sent = true;
    info!("acknowledged udp motion transport");
}

#[derive(Default)]
struct ClientRuntimeState {
    remote_active: bool,
    active_modifiers: HashSet<Modifier>,
    right_edge_release_armed: bool,
    just_activated: bool,
    udp_motion_ready_sent: bool,
    pending_motion: Option<PendingClientMotion>,
}

struct PendingClientMotion {
    dx: i32,
    dy: i32,
    source: &'static str,
    source_seq: u64,
    event_count: u32,
    queued_at: Instant,
}

impl ClientRuntimeState {
    fn set_remote_active(&mut self, active: bool) {
        self.remote_active = active;
        self.reset_keys();
        self.clear_pending_motion();
        self.right_edge_release_armed = false;
        self.just_activated = active;
        self.udp_motion_ready_sent = false;
    }

    fn reset_keys(&mut self) {
        self.active_modifiers.clear();
    }

    fn has_pending_motion(&self) -> bool {
        self.pending_motion.is_some()
    }

    fn clear_pending_motion(&mut self) {
        self.pending_motion = None;
    }

    fn queue_motion(&mut self, dx: i32, dy: i32, source: &'static str, source_seq: u64) {
        if !self.remote_active || (dx == 0 && dy == 0) {
            return;
        }

        match &mut self.pending_motion {
            Some(pending) => {
                pending.dx = pending.dx.saturating_add(dx);
                pending.dy = pending.dy.saturating_add(dy);
                pending.source = source;
                pending.source_seq = source_seq;
                pending.event_count = pending.event_count.saturating_add(1);
            }
            None => {
                self.pending_motion = Some(PendingClientMotion {
                    dx,
                    dy,
                    source,
                    source_seq,
                    event_count: 1,
                    queued_at: Instant::now(),
                });
            }
        }
    }

    fn take_pending_motion(&mut self) -> Option<PendingClientMotion> {
        self.pending_motion.take()
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
        SinkKind::NativeHid => hid::native_hid_sink_async().await,
    }
}

fn mac_motion_flush_interval() -> Duration {
    let millis = std::env::var("SOFTKVM_MAC_MOTION_FLUSH_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAC_MOTION_FLUSH_INTERVAL_MS)
        .clamp(1, 16);
    Duration::from_millis(millis)
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

async fn run_motion_bench(
    peer: String,
    bench_transport: BenchTransport,
    timing: BenchTiming,
    hz: u32,
    seconds: u32,
    dx: i32,
) -> Result<()> {
    bail_if_invalid_bench(hz, seconds)?;

    let stream = TcpStream::connect(&peer)
        .await
        .with_context(|| format!("connect {peer}"))?;
    stream.set_nodelay(true).context("set TCP_NODELAY")?;
    let udp = if matches!(bench_transport, BenchTransport::Udp) {
        let socket = StdUdpSocket::bind("0.0.0.0:0").context("bind udp bench")?;
        socket
            .connect(&peer)
            .with_context(|| format!("connect udp bench {peer}"))?;
        Some(socket)
    } else {
        None
    };

    let mut writer = transport::FrameWriter::new(stream);
    let session_id = Uuid::new_v4();
    let mut tcp_seq = 1_u64;
    writer
        .write_frame(Frame::new(
            session_id,
            tcp_seq,
            Message::Hello(ProtocolHello {
                device_name: hostname(),
                role: "motion-bench".to_owned(),
                protocol_version: protocol::PROTOCOL_VERSION,
            }),
        ))
        .await?;
    tcp_seq += 1;
    writer
        .write_frame(Frame::new(
            session_id,
            tcp_seq,
            Message::HostState(protocol::HostStateEvent {
                remote_active: true,
                reason: "motion bench".to_owned(),
                entry_x_ratio: Some(0.5),
                entry_y_ratio: Some(0.5),
            }),
        ))
        .await?;
    tcp_seq += 1;

    sleep(Duration::from_millis(150)).await;

    let total = u64::from(hz) * u64::from(seconds);
    let period = Duration::from_secs_f64(1.0 / f64::from(hz));
    let started = Instant::now();
    let mut next_tick = tokio::time::Instant::now();
    let mut max_send_gap = Duration::ZERO;
    let mut last_send = Instant::now();
    for i in 0..total {
        next_tick += period;
        let direction = if (i / (u64::from(hz) / 2).max(1)) % 2 == 0 {
            1
        } else {
            -1
        };
        let event_dx = dx.saturating_mul(direction);
        match (&udp, bench_transport) {
            (Some(socket), BenchTransport::Udp) => {
                let packet = MotionDatagram {
                    seq: i + 1,
                    dx: event_dx,
                    dy: 0,
                };
                let encoded = packet.encode();
                let sent = socket
                    .send(&encoded)
                    .context("send udp motion bench packet")?;
                if sent != encoded.len() {
                    bail!("short udp motion bench send: {sent}/{}", encoded.len());
                }
            }
            _ => {
                writer
                    .write_frame(Frame::new(
                        session_id,
                        tcp_seq,
                        Message::Input(InputEvent::MouseMotion {
                            dx: event_dx,
                            dy: 0,
                        }),
                    ))
                    .await?;
                tcp_seq += 1;
            }
        }
        let now = Instant::now();
        max_send_gap = max_send_gap.max(now.saturating_duration_since(last_send));
        last_send = now;
        wait_for_bench_tick(next_tick, timing).await;
    }

    writer
        .write_frame(Frame::new(session_id, tcp_seq, Message::InputReset))
        .await?;
    tcp_seq += 1;
    writer
        .write_frame(Frame::new(
            session_id,
            tcp_seq,
            Message::HostState(protocol::HostStateEvent {
                remote_active: false,
                reason: "motion bench done".to_owned(),
                entry_x_ratio: None,
                entry_y_ratio: None,
            }),
        ))
        .await?;

    println!(
        "motion_bench transport={bench_transport:?} timing={timing:?} hz={hz} seconds={seconds} packets={total} elapsed_ms={:.3} max_send_gap_ms={:.3}",
        latency::ms(started.elapsed()),
        latency::ms(max_send_gap),
    );
    Ok(())
}

async fn wait_for_bench_tick(deadline: tokio::time::Instant, timing: BenchTiming) {
    match timing {
        BenchTiming::Sleep => tokio::time::sleep_until(deadline).await,
        BenchTiming::Spin => {
            const SPIN_MARGIN: Duration = Duration::from_millis(2);
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break;
                }
                let remaining = deadline - now;
                if remaining > SPIN_MARGIN {
                    tokio::time::sleep(remaining - SPIN_MARGIN).await;
                } else {
                    while tokio::time::Instant::now() < deadline {
                        std::hint::spin_loop();
                    }
                    break;
                }
            }
        }
    }
}

fn bail_if_invalid_bench(hz: u32, seconds: u32) -> Result<()> {
    if hz == 0 || hz > 2000 {
        bail!("--hz must be in 1..=2000");
    }
    if seconds == 0 || seconds > 120 {
        bail!("--seconds must be in 1..=120");
    }
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
